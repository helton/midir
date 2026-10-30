//! MCP (Model Context Protocol) over HTTP: `POST /mcp` on the same port as the APIs, JSON-RPC in, JSON out
//! (the Streamable HTTP transport without sessions or server-initiated streams), with two tools for agents that
//! have no web access of their own: `web_search`, answered by a backend agent that has the platform's web tools,
//! and `web_fetch`, a page downloaded by Midir itself (through the same proxy and trust store as the backends, so
//! internal pages work) and reduced to text, with that agent as the fallback for pages that need a browser.

pub mod html;

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::backends::{Item, TextBackend};
use crate::config::{Config, McpSettings};
use crate::errors::{Error, NetError};
use crate::text::{char_len, prefix};

/// The protocol revisions answered as the client asks for them; a newer or unknown one gets the latest.
const PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL: &str = "2025-06-18";
/// The most of a page `web_fetch` downloads.
const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;
/// Results `web_search` may be asked for.
const MAX_RESULTS: i64 = 10;
/// A page this large whose text is this short is built by JavaScript: the agent reads it instead.
const JS_PAGE_BYTES: usize = 10_000;
const JS_PAGE_CHARS: usize = 300;
/// The tracking parameter the web agent appends to every URL.
const UTM: &str = "utm_source=stackspot";

pub struct Mcp {
    settings: McpSettings,
    /// the backend that serves the web agent, when one is configured
    backend: Option<Arc<dyn TextBackend>>,
    http: reqwest::Client,
    http_error: Option<String>,
}

/// What a tool call gives back: text, or an error the model should see (`isError`).
type ToolResult = Result<String, String>;

impl Mcp {
    /// None when `[mcp]` is off.
    pub fn new(cfg: &Config, backends: &IndexMap<String, Arc<dyn TextBackend>>) -> Option<Arc<Mcp>> {
        let s = &cfg.mcp;
        if !s.enabled {
            return None;
        }
        let backend = s.web_agent.as_ref().and_then(|_| backends.get(&s.backend).cloned());
        let mut builder = reqwest::Client::builder()
            // a fetcher's honest identity; sites that answer bots with a wall do so by behavior, not this string
            .user_agent(format!("Mozilla/5.0 (compatible; midir/{})", crate::buildinfo::VERSION))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs_f64(s.timeout_s))
            .tcp_nodelay(true);
        let mut http_error = None;
        if let Some(path) = &s.ca_bundle {
            match std::fs::read(path)
                .map_err(|e| e.to_string())
                .and_then(|pem| reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| e.to_string()))
            {
                Ok(certs) => builder = builder.tls_certs_merge(certs),
                Err(e) => http_error = Some(format!("cannot read [mcp] ca_bundle {path}: {e}")),
            }
        }
        let http = builder.build().unwrap_or_default();
        if backend.is_none() {
            tracing::warn!("mcp: no web_agent: web_search is not offered and web_fetch has no fallback for pages that need a browser");
        }
        if let Some(e) = &http_error {
            tracing::warn!("mcp: {e}");
        }
        Some(Arc::new(Mcp { settings: s.clone(), backend, http, http_error }))
    }

    pub fn settings(&self) -> &McpSettings {
        &self.settings
    }

    pub fn has_web_agent(&self) -> bool {
        self.backend.is_some()
    }

    pub fn tool_names(&self) -> Vec<&'static str> {
        if self.backend.is_some() { vec!["web_search", "web_fetch"] } else { vec!["web_fetch"] }
    }

    /// The tool listing (`tools/list`). Descriptions are short on purpose: every client lists them in every prompt.
    pub fn tools(&self) -> Vec<Value> {
        let mut tools = vec![];
        if self.backend.is_some() {
            tools.push(json!({
                "name": "web_search",
                "description": "Search the web. Returns up to max_results lines: title | url | snippet. Takes 30-40 s.",
                "inputSchema": {"type": "object", "properties": {
                    "query": {"type": "string", "description": "What to search for"},
                    "max_results": {"type": "integer", "description": "1-10, default 5"}
                }, "required": ["query"]}
            }));
        }
        tools.push(json!({
            "name": "web_fetch",
            "description": format!("Fetch a URL (http/https, internal pages too) as plain text, cut at max_chars (default {}).", self.settings.fetch_max_chars),
            "inputSchema": {"type": "object", "properties": {
                "url": {"type": "string", "description": "The page to read"},
                "max_chars": {"type": "integer", "description": "Where to cut the text"}
            }, "required": ["url"]}
        }));
        tools
    }

    /// One HTTP body: a JSON-RPC request, notification or batch. The HTTP status and the body to answer (none for a
    /// notification: 202).
    pub async fn handle(&self, body: &[u8]) -> (u16, Option<Value>) {
        let msg: Value = match serde_json::from_slice(&crate::json::sanitize(body)) {
            Ok(v) => v,
            Err(e) => return (400, Some(error_reply(Value::Null, -32700, &format!("parse error: {e}")))),
        };
        match msg {
            Value::Array(batch) => {
                if batch.is_empty() {
                    return (400, Some(error_reply(Value::Null, -32600, "invalid request: an empty batch")));
                }
                let mut replies = vec![];
                for m in &batch {
                    if let Some(r) = self.message(m).await {
                        replies.push(r);
                    }
                }
                if replies.is_empty() { (202, None) } else { (200, Some(Value::Array(replies))) }
            }
            Value::Object(_) => match self.message(&msg).await {
                Some(r) => (200, Some(r)),
                None => (202, None),
            },
            _ => (400, Some(error_reply(Value::Null, -32600, "invalid request: not a JSON-RPC message"))),
        }
    }

    /// One message: the reply to a request, none for a notification.
    async fn message(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").and_then(Value::as_object);
        let Some(id) = id else {
            // a notification (initialized, cancelled, progress): nothing to answer
            if method.is_empty() {
                tracing::debug!("mcp: a message with neither method nor id ignored");
            }
            return None;
        };
        let result = match method {
            "initialize" => {
                let asked = params.and_then(|p| p.get("protocolVersion")).and_then(Value::as_str).unwrap_or(LATEST_PROTOCOL);
                let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { LATEST_PROTOCOL };
                let client = params.and_then(|p| p.get("clientInfo")).and_then(|c| c.get("name")).and_then(Value::as_str).unwrap_or("?");
                tracing::info!("mcp: {client} connected (protocol {version}; tools {})", self.tool_names().join(", "));
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "midir", "version": crate::buildinfo::full_version()}
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tools()})),
            "tools/call" => {
                let name = params.and_then(|p| p.get("name")).and_then(Value::as_str).unwrap_or("");
                let empty = Map::new();
                let args = params.and_then(|p| p.get("arguments")).and_then(Value::as_object).unwrap_or(&empty);
                match self.call_tool(name, args).await {
                    Ok(Ok(text)) => Ok(json!({"content": [{"type": "text", "text": text}], "isError": false})),
                    Ok(Err(text)) => Ok(json!({"content": [{"type": "text", "text": text}], "isError": true})),
                    Err((code, message)) => Err((code, message)),
                }
            }
            "" => Err((-32600, "invalid request: no method".to_string())),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => error_reply(id, code, &message),
        })
    }

    /// Ok(tool result) for a known tool; Err(JSON-RPC error) for an unknown one or bad arguments.
    async fn call_tool(&self, name: &str, args: &Map<String, Value>) -> Result<ToolResult, (i64, String)> {
        let t0 = Instant::now();
        let timeout = Duration::from_secs_f64(self.settings.timeout_s);
        let string = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let number = |key: &str| args.get(key).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)));
        let result = match name {
            "web_search" if self.backend.is_some() => {
                let Some(query) = string("query") else { return Err((-32602, "web_search needs a query".into())) };
                let n = number("max_results").unwrap_or(5).clamp(1, MAX_RESULTS) as usize;
                match tokio::time::timeout(timeout, self.web_search(query, n)).await {
                    Ok(Ok((text, found))) => {
                        tracing::info!("mcp web_search {:?}: {found} results ({:.1} s)", prefix(query, 80), t0.elapsed().as_secs_f64());
                        Ok(text)
                    }
                    Ok(Err(e)) => Err(format!("web_search failed: {}", describe(&e))),
                    Err(_) => Err(format!("web_search: no answer within {} s", self.settings.timeout_s)),
                }
            }
            "web_fetch" => {
                let Some(url) = string("url") else { return Err((-32602, "web_fetch needs a url".into())) };
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err((-32602, format!("web_fetch: not an http(s) URL: {}", prefix(url, 100))));
                }
                let max = number("max_chars").filter(|n| *n > 0).unwrap_or(self.settings.fetch_max_chars).max(200) as usize;
                match tokio::time::timeout(timeout, self.web_fetch(url, max)).await {
                    Ok(Ok(text)) => {
                        tracing::info!(
                            "mcp web_fetch {}: {} chars ({:.1} s)",
                            prefix(url, 120),
                            char_len(&text),
                            t0.elapsed().as_secs_f64()
                        );
                        Ok(text)
                    }
                    Ok(Err(why)) => {
                        tracing::info!("mcp web_fetch {}: {why} ({:.1} s)", prefix(url, 120), t0.elapsed().as_secs_f64());
                        Err(format!("web_fetch failed: {why}"))
                    }
                    Err(_) => Err(format!("web_fetch: no answer within {} s", self.settings.timeout_s)),
                }
            }
            other => return Err((-32602, format!("unknown tool: {other} (tools: {})", self.tool_names().join(", ")))),
        };
        if let Err(e) = &result {
            tracing::warn!("mcp {name}: {e}");
        }
        Ok(result)
    }

    /// The whole text of the web agent's reply to `prompt`.
    async fn ask_agent(&self, prompt: &str) -> Result<String, Error> {
        let (Some(backend), Some(target)) = (&self.backend, self.settings.web_agent.as_deref()) else {
            return Err(Error::Internal("no web agent configured".into()));
        };
        let mut items = backend.stream(prompt, target, None).await?;
        let mut text = String::new();
        while let Some(item) = items.next().await {
            if let Item::Text(t) = item? {
                text.push_str(&t);
            }
        }
        Ok(text)
    }

    /// The web agent's results, cleaned: the tracking parameter it appends removed, one line per distinct URL.
    pub async fn web_search(&self, query: &str, n: usize) -> Result<(String, usize), Error> {
        let prompt = format!(
            "Search the web for: {query}\nReply with only the top {n} results with {n} DISTINCT URLs, one per line, as: \
             title | url | one-line snippet. Prefer official documentation. No commentary."
        );
        let reply = self.ask_agent(&prompt).await?;
        Ok(clean_results(&reply))
    }

    /// The page as text: downloaded here when it can be, else read by the web agent.
    pub async fn web_fetch(&self, url: &str, max_chars: usize) -> Result<String, String> {
        let native = match &self.http_error {
            Some(e) => Err(e.clone()),
            None => self.fetch_native(url).await,
        };
        let fallback_reason = match &native {
            Ok(page) if page.html && page.bytes > JS_PAGE_BYTES && char_len(&page.text) < JS_PAGE_CHARS => {
                "the page is built by JavaScript".to_string()
            }
            Ok(page) => return Ok(cut(&page.render(), max_chars)),
            // gone for everyone, or not a page: not worth the agent's time
            Err(why) if why.starts_with("HTTP 404") || why.starts_with("HTTP 410") || why.starts_with("not text") => {
                return Err(why.clone());
            }
            Err(why) => why.clone(),
        };
        if self.backend.is_none() {
            return match native {
                Ok(page) => Ok(format!("({fallback_reason}; nothing to read without a browser)\n\n{}", cut(&page.render(), max_chars))),
                Err(why) => Err(why),
            };
        }
        let prompt = format!(
            "Fetch {url} and return its main text content as plain text, at most {max_chars} characters, in the page's own \
             language, no commentary. If the page cannot be read, say so in one line."
        );
        match self.ask_agent(&prompt).await {
            Ok(text) if !text.trim().is_empty() => {
                Ok(format!("(read by the web agent: {fallback_reason})\n\n{}", cut(text.trim(), max_chars)))
            }
            Ok(_) => Err(format!("{fallback_reason}; the web agent returned nothing")),
            Err(e) => Err(format!("{fallback_reason}; the web agent failed: {}", describe(&e))),
        }
    }

    /// GET through Midir's own client (proxy, trust store), at most `MAX_FETCH_BYTES`, as text.
    async fn fetch_native(&self, url: &str) -> Result<Page, String> {
        let r = self
            .http
            .get(url)
            .header("Accept", "text/html, text/plain;q=0.9, application/json;q=0.8, */*;q=0.5")
            .send()
            .await
            .map_err(|e| NetError::from_reqwest(&e, false).to_string())?;
        let status = r.status().as_u16();
        let content_type = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
        let final_url = r.url().to_string();
        let mut body: Vec<u8> = vec![];
        let mut stream = r.bytes_stream();
        let mut truncated = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| NetError::from_reqwest(&e, true).to_string())?;
            if body.len() + chunk.len() > MAX_FETCH_BYTES {
                body.extend_from_slice(&chunk[..MAX_FETCH_BYTES - body.len()]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        if status >= 400 {
            let raw = String::from_utf8_lossy(&body);
            let text = if raw.trim_start().starts_with('<') { html::to_text(&raw).1 } else { raw.trim().to_string() };
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            return Err(format!("HTTP {status}{}", if text.is_empty() { String::new() } else { format!(": {}", prefix(&text, 200)) }));
        }
        let raw = String::from_utf8_lossy(&body);
        let head = prefix(raw.trim_start(), 300).to_lowercase();
        let html = content_type.contains("text/html")
            || content_type.contains("xhtml")
            || head.starts_with("<!doctype html")
            || head.contains("<html");
        let textual = html
            || content_type.starts_with("text/")
            || ["json", "xml", "javascript", "yaml", "toml", "markdown", "x-sh", "x-www-form"].iter().any(|t| content_type.contains(t))
            || (content_type.is_empty() && !raw.chars().take(2000).any(|c| c.is_control() && !c.is_whitespace()));
        if !textual {
            return Err(format!(
                "not text (content-type {}, {} bytes)",
                if content_type.is_empty() { "unknown" } else { &content_type },
                body.len()
            ));
        }
        let (title, text) = if html { html::to_text(&raw) } else { (None, raw.trim().to_string()) };
        Ok(Page { title, url: final_url, text, html, bytes: body.len(), truncated })
    }
}

struct Page {
    title: Option<String>,
    url: String,
    text: String,
    html: bool,
    bytes: usize,
    truncated: bool,
}

impl Page {
    fn render(&self) -> String {
        let mut out = String::new();
        if let Some(t) = &self.title {
            out.push_str(t);
            out.push('\n');
        }
        out.push_str(&self.url);
        if self.truncated {
            out.push_str(&format!("\n(only the first {} MB of the page were read)", MAX_FETCH_BYTES / (1024 * 1024)));
        }
        out.push_str("\n\n");
        out.push_str(&self.text);
        out
    }
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn describe(e: &Error) -> String {
    match e {
        Error::Backend(b) => b.message(),
        other => other.to_string(),
    }
}

/// `text` cut at `max` characters, with a note that says so.
pub fn cut(text: &str, max: usize) -> String {
    let total = char_len(text);
    if total <= max {
        return text.to_string();
    }
    format!("{}\n[cut at {max} of {total} characters]", prefix(text, max).trim_end())
}

/// The web agent's result lines without the tracking parameter it appends, one line per URL (the first line wins),
/// and how many distinct URLs there are.
pub fn clean_results(reply: &str) -> (String, usize) {
    let mut seen: Vec<String> = vec![];
    let mut lines = vec![];
    for line in reply.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let line = strip_utm(line);
        let url = line.split(|c: char| c.is_whitespace() || c == '|').find(|w| w.starts_with("http://") || w.starts_with("https://"));
        if let Some(u) = url {
            let key = u.trim_end_matches(['/', '.', ',', ')']).to_lowercase();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
        }
        lines.push(line);
    }
    (lines.join("\n"), seen.len())
}

fn strip_utm(line: &str) -> String {
    line.replace(&format!("?{UTM}&"), "?").replace(&format!("&{UTM}"), "").replace(&format!("?{UTM}"), "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_lose_the_tracking_parameter_and_their_duplicates() {
        let reply = "Tokio releases | https://github.com/tokio-rs/tokio/releases?utm_source=stackspot | all versions\n\n\
                     Tokio releases | https://github.com/tokio-rs/tokio/releases/?utm_source=stackspot | again\n\
                     Advisory | https://github.com/tokio-rs/tokio/security/advisories/GHSA-1?utm_source=stackspot&x=1 | a race\n\
                     No more results.";
        let (text, n) = clean_results(reply);
        assert_eq!(n, 2);
        assert_eq!(
            text,
            "Tokio releases | https://github.com/tokio-rs/tokio/releases | all versions\n\
             Advisory | https://github.com/tokio-rs/tokio/security/advisories/GHSA-1?x=1 | a race\n\
             No more results."
        );
    }

    #[test]
    fn cutting_says_so() {
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(cut("abcdef ", 3), "abc\n[cut at 3 of 7 characters]");
    }
}
