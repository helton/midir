//! `midir check`: the configuration as Midir reads it (each setting with where its value came from), the models, and
//! the checks a running server would only show in its log: unknown settings, missing credentials, the backend's token
//! (credentials, TLS, proxy) and, on request, one short prompt per agent. Nothing is served.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;

use crate::backends::Item;
use crate::config::{Config, ToolSchema};
use crate::errors::Error;
use crate::gateway::Gateway;
use crate::store::ResponseStore;
use crate::telemetry::Telemetry;
use crate::text::prefix;

pub struct Options {
    /// skip every network call (the token check and the agents)
    pub no_network: bool,
    /// send one short prompt to each model's target (one request per model, from the account's quota)
    pub agents: bool,
}

/// How long a token or agent check may take.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);
const AGENT_PROMPT: &str = "Reply with the single word OK.";

/// Collects what the configuration and the backends log at WARN while they load (unknown settings and options, the
/// legacy layout), for the report instead of the log.
#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Warnings {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Warnings {
    fn lines(&self) -> Vec<String> {
        let raw = self.0.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&raw).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
    }
}

/// A value from the file, by path (`["server", "port"]`).
fn in_file(file: &toml::Table, path: &[&str]) -> bool {
    let mut at = file;
    for (i, key) in path.iter().enumerate() {
        match at.get(*key) {
            Some(toml::Value::Table(t)) if i + 1 < path.len() => at = t,
            Some(_) if i + 1 == path.len() => return true,
            _ => return false,
        }
    }
    false
}

/// Numbers as written: `30`, not `30.0`.
fn num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { v.to_string() }
}

/// A proxy URL without its credentials.
fn without_userinfo(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_, host)) => format!("{scheme}://***@{host}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

struct Report {
    out: String,
    problems: usize,
    warnings: usize,
}

impl Report {
    fn section(&mut self, title: &str) {
        let _ = write!(self.out, "\n{title}\n");
    }

    fn row(&mut self, name: &str, value: &str, source: &str) {
        let _ = writeln!(self.out, "  {name:<28} {value:<44} {source}");
    }

    fn ok(&mut self, what: &str) {
        let _ = writeln!(self.out, "  ok    {what}");
    }

    fn warn(&mut self, what: &str) {
        self.warnings += 1;
        let _ = writeln!(self.out, "  WARN  {what}");
    }

    fn fail(&mut self, what: &str) {
        self.problems += 1;
        let _ = writeln!(self.out, "  FAIL  {what}");
    }

    fn skip(&mut self, what: &str) {
        let _ = writeln!(self.out, "  -     {what}");
    }
}

/// The report, and whether everything checked passed (warnings do not fail it).
pub async fn run(env: indexmap::IndexMap<String, String>, root: &Path, dotenv: Option<&Path>, opts: &Options) -> (String, bool) {
    let mut r = Report { out: format!("Midir {}: configuration check\n", crate::buildinfo::full_version()), problems: 0, warnings: 0 };
    let warnings = Warnings::default();
    let collector = {
        let w = warnings.clone();
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .without_time()
            .with_target(false)
            .with_level(false)
            .with_ansi(false)
            .with_writer(move || w.clone())
            .finish()
    };
    let loaded = tracing::subscriber::with_default(collector, || {
        let cfg = Config::load(env.clone(), root.to_path_buf())?;
        let cfg = Arc::new(cfg);
        // telemetry stays off: the check exports nothing
        let gw = Gateway::without_store(cfg.clone(), Arc::new(Telemetry::disabled()))?;
        Ok::<_, crate::config::ConfigError>((cfg, gw))
    });

    let (cfg, gw) = match loaded {
        Ok(v) => v,
        Err(e) => {
            r.section("Configuration");
            for w in warnings.lines() {
                r.warn(&w);
            }
            r.fail(&e.to_string());
            return finish(r);
        }
    };
    r.section("Files");
    let file: toml::Table = std::fs::read_to_string(&cfg.config_file).ok().and_then(|t| t.parse().ok()).unwrap_or_default();
    r.row("config", &cfg.source, if cfg.config_file.is_file() { "" } else { "(no file: environment and defaults only)" });
    match dotenv {
        Some(p) => r.row(".env", &p.display().to_string(), "loaded"),
        None => r.row(".env", "-", "not found (the environment alone)"),
    }
    for var in ["HTTPS_PROXY", "https_proxy", "NO_PROXY", "no_proxy", "SSL_CERT_FILE"] {
        if let Some(v) = env.get(var).filter(|v| !v.is_empty()) {
            r.row(var, &without_userinfo(v), "env");
        }
    }

    let source = |var: &str, path: &[&str]| -> String {
        if env.get(var).is_some_and(|v| !v.is_empty()) {
            format!("env {var}")
        } else if in_file(&file, path) {
            "file".into()
        } else {
            "default".into()
        }
    };
    let s = &cfg.server;
    r.section("Server");
    let schema = match s.tool_schema {
        ToolSchema::Json => "json",
        ToolSchema::Compact => "compact",
    };
    let dir = s.responses_dir.as_ref().map_or("- (memory only)".into(), |d| d.display().to_string());
    let rows: Vec<(&str, &str, String)> = vec![
        ("port", "MIDIR_PORT", s.port.to_string()),
        ("api_key", "MIDIR_API_KEY", if s.api_key.is_some() { "set".into() } else { "not set (no authentication)".into() }),
        ("max_prompt_chars", "MIDIR_MAX_PROMPT_CHARS", s.max_prompt_chars.to_string()),
        ("tail_reminder", "MIDIR_TAIL_REMINDER", s.tail_reminder.to_string()),
        ("tool_desc_max", "MIDIR_TOOL_DESC_MAX", s.tool_desc_max.to_string()),
        ("tool_schema", "MIDIR_TOOL_SCHEMA", schema.into()),
        ("followups", "MIDIR_FOLLOWUPS", s.followups.to_string()),
        ("keepalive_s", "MIDIR_KEEPALIVE_S", num(s.keepalive_s)),
        ("read_timeout_s", "MIDIR_READ_TIMEOUT_S", num(s.read_timeout_s)),
        ("retry_backoff_s", "MIDIR_RETRY_BACKOFF_S", num(s.retry_backoff_s)),
        ("shutdown_grace_s", "MIDIR_SHUTDOWN_GRACE_S", num(s.shutdown_grace_s)),
        ("responses_dir", "MIDIR_RESPONSES_DIR", dir),
        ("responses_retention_days", "MIDIR_RESPONSES_RETENTION_DAYS", num(s.responses_retention_days)),
        ("responses_max_mb", "MIDIR_RESPONSES_MAX_MB", num(s.responses_max_mb)),
        ("responses_memory_mb", "MIDIR_RESPONSES_MEMORY_MB", num(s.responses_memory_mb)),
    ];
    for (key, var, value) in rows {
        let from = source(var, &["server", key]);
        r.row(key, &value, &from);
    }

    r.section("Telemetry");
    let t = &cfg.telemetry;
    let endpoint = if t.otlp_endpoint.is_empty() { "- (off)".to_string() } else { t.otlp_endpoint.clone() };
    let rows = [
        ("otlp_endpoint", "OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
        ("service_name", "OTEL_SERVICE_NAME", t.service_name.clone()),
        ("prometheus", "MIDIR_PROMETHEUS", t.prometheus.to_string()),
    ];
    for (key, var, value) in rows {
        let from = source(var, &["telemetry", key]);
        r.row(key, &value, &from);
    }

    for (name, b) in &gw.backends {
        r.section(&format!("Backend {name} ({})", b.kind()));
        for (key, var, value) in b.options() {
            let from = source(var, &["backends", name, key]);
            r.row(key, &value, &from);
        }
        if let Some(settings) = cfg.backends.get(name) {
            let l = &settings.limits;
            let rows = [
                ("max_concurrent", "MIDIR_MAX_CONCURRENT", l.max_concurrent.to_string()),
                ("requests_per_minute", "MIDIR_REQUESTS_PER_MINUTE", l.requests_per_minute.to_string()),
                ("queue_timeout_s", "MIDIR_QUEUE_TIMEOUT", num(l.queue_timeout_s)),
                ("cooldown_on_429_s", "MIDIR_COOLDOWN_ON_429", num(l.cooldown_on_429_s)),
                ("max_waiting", "MIDIR_MAX_WAITING", l.max_waiting.to_string()),
            ];
            for (key, var, value) in rows {
                let from = source(var, &["backends", name, "limits", key]);
                r.row(&format!("limits.{key}"), &value, &from);
            }
        }
    }

    r.section(&format!("Models (default: {})", cfg.default.name));
    for m in cfg.exposed_models() {
        let target = gw.backends.get(&m.backend).map_or(m.target.clone(), |b| b.describe_target(&m.target));
        let mut notes = vec![];
        if !m.aliases.is_empty() {
            notes.push(format!("aliases {}", m.aliases.join(", ")));
        }
        if let Some(p) = &m.match_pattern {
            notes.push(format!("match {p}"));
        }
        let knobs = cfg.knobs(Some(&m));
        let server = cfg.knobs(None);
        if knobs != server {
            let mut own = vec![];
            if knobs.max_prompt_chars != server.max_prompt_chars {
                own.push(format!("max_prompt_chars={}", knobs.max_prompt_chars));
            }
            if knobs.tail_reminder != server.tail_reminder {
                own.push(format!("tail_reminder={}", knobs.tail_reminder));
            }
            if knobs.tool_desc_max != server.tool_desc_max {
                own.push(format!("tool_desc_max={}", knobs.tool_desc_max));
            }
            if knobs.tool_schema != server.tool_schema {
                own.push(format!("tool_schema={}", if knobs.tool_schema == ToolSchema::Compact { "compact" } else { "json" }));
            }
            if knobs.followups != server.followups {
                own.push(format!("followups={}", knobs.followups));
            }
            notes.push(format!("own {}", own.join(", ")));
        }
        r.row(&m.name, &format!("{}:{target}", m.backend), &notes.join("; "));
    }

    r.section("Checks");
    for w in warnings.lines() {
        r.warn(&w);
    }
    match gw.validate() {
        Ok(()) => r.ok("every backend that serves a model has its credentials"),
        Err(e) => r.fail(&e.to_string()),
    }
    match &cfg.server.responses_dir {
        Some(d) => match ResponseStore::probe(d) {
            None => r.ok(&format!("responses store: {} is writable", d.display())),
            Some(why) => r.warn(&format!("responses store: {why}; previous_response_id would be kept in memory only")),
        },
        None => r.skip("responses store: memory only (responses_dir is empty)"),
    }
    if opts.no_network {
        r.skip("backends: not contacted (--no-network)");
        return finish(r);
    }
    for (name, b) in &gw.backends {
        let t0 = Instant::now();
        match tokio::time::timeout(NETWORK_TIMEOUT, b.ready()).await {
            Ok(Ok(())) => r.ok(&format!("backend {name}: credentials accepted, TLS and proxy work ({:.1} s)", t0.elapsed().as_secs_f64())),
            Ok(Err(e)) => r.fail(&format!("backend {name}: {}", prefix(&describe(&e), 400))),
            Err(_) => r.fail(&format!("backend {name}: no answer within {} s (network, proxy or firewall)", NETWORK_TIMEOUT.as_secs())),
        }
    }
    if !opts.agents {
        r.skip("agents: not asked (midir check --agents sends each model one short prompt, from the account's quota)");
        return finish(r);
    }
    let mut asked: Vec<(String, String)> = vec![];
    for m in cfg.exposed_models() {
        if asked.contains(&(m.backend.clone(), m.target.clone())) {
            continue;
        }
        asked.push((m.backend.clone(), m.target.clone()));
        let Some(b) = gw.backends.get(&m.backend) else { continue };
        let t0 = Instant::now();
        let answer = tokio::time::timeout(NETWORK_TIMEOUT, async {
            let mut items = b.stream(AGENT_PROMPT, &m.target, None).await?;
            let mut text = String::new();
            while let Some(item) = items.next().await {
                if let Item::Text(t) = item? {
                    text.push_str(&t);
                }
            }
            Ok::<_, Error>(text)
        })
        .await;
        match answer {
            Ok(Ok(text)) => {
                r.ok(&format!("model {}: the agent answered {:?} ({:.1} s)", m.name, prefix(text.trim(), 40), t0.elapsed().as_secs_f64()))
            }
            Ok(Err(e)) => {
                let hint = match &e {
                    Error::Backend(b) if b.status == 403 || b.status == 404 => " (is the agent id right, and shared with this client?)",
                    _ => "",
                };
                r.fail(&format!("model {}: {}{hint}", m.name, prefix(&describe(&e), 400)));
            }
            Err(_) => r.fail(&format!("model {}: no answer within {} s", m.name, NETWORK_TIMEOUT.as_secs())),
        }
    }
    finish(r)
}

fn describe(e: &Error) -> String {
    match e {
        Error::Backend(b) => b.message(),
        other => other.to_string(),
    }
}

fn finish(mut r: Report) -> (String, bool) {
    let verdict = match (r.problems, r.warnings) {
        (0, 0) => "\nResult: OK\n".to_string(),
        (0, w) => format!("\nResult: OK, with {w} warning(s)\n"),
        (p, _) => format!("\nResult: {p} problem(s)\n"),
    };
    r.out.push_str(&verdict);
    let ok = r.problems == 0;
    (r.out, ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_credentials_are_not_shown() {
        assert_eq!(without_userinfo("http://user:pw@proxy:8080"), "http://***@proxy:8080");
        assert_eq!(without_userinfo("http://proxy:8080"), "http://proxy:8080");
    }

    #[test]
    fn file_paths_and_numbers() {
        let file: toml::Table = "[server]\nport = 1\n[backends.a.limits]\nmax_waiting = 2\n".parse().unwrap();
        assert!(in_file(&file, &["server", "port"]));
        assert!(in_file(&file, &["backends", "a", "limits", "max_waiting"]));
        assert!(!in_file(&file, &["server", "keepalive_s"]) && !in_file(&file, &["backends", "b", "realm"]));
        assert_eq!((num(30.0), num(0.5)), ("30".to_string(), "0.5".to_string()));
    }
}
