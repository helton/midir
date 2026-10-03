//! Runs canonical requests on a text-only backend: renders the prompt, streams the backend's
//! text through the tool-call parser and the output limiter, and makes the automatic follow-ups (invalid tool JSON,
//! announce-and-stop, false incapacity, tool_choice retry, JSON-mode repair).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

use super::followups;
use super::jsonmode::check_json;
use super::output::OutputLimiter;
use super::parser::{Parsed, ToolCallParser};
use super::prompt::render_prompt;
use crate::backends::{Completion, Item, StackSpotBackend};
use crate::canonical::{estimate_tokens, CanonicalRequest, CanonicalResponse, Event, ToolCall, ToolChoice, Usage, CHARS_PER_TOKEN};
use crate::config::Config;
use crate::errors::Error;
use crate::py::json as pyjson;
use crate::py::text;

const LOG: &str = "midir.emulation.engine";

pub type EventStream = BoxStream<'static, Result<Event, Error>>;

pub struct EmulationEngine {
    pub backend: Arc<StackSpotBackend>,
    config: Arc<Config>,
    /// target -> prompt cap learned from an input-too-long refusal
    learned_max_chars: Mutex<HashMap<String, i64>>,
    ignored_seen: Mutex<HashSet<(String, Vec<String>)>>,
}

fn add_usage(a: &Usage, b: Option<Usage>) -> Usage {
    b.map_or(*a, |b| a.add(&b))
}

impl EmulationEngine {
    pub fn new(backend: Arc<StackSpotBackend>, config: Arc<Config>) -> Self {
        EmulationEngine { backend, config, learned_max_chars: Mutex::new(HashMap::new()), ignored_seen: Mutex::new(HashSet::new()) }
    }

    /// Parameters accepted without effect: reported once per client and set of parameters at INFO, then at DEBUG.
    fn report_ignored(&self, req: &CanonicalRequest, rid: &str) {
        let mut sorted = req.ignored.clone();
        sorted.sort();
        let key = (req.meta().client.clone(), sorted);
        let mut seen = self.ignored_seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.contains(&key) {
            crate::debug!(LOG, "{rid} accepted parameters without effect: {}", req.ignored.join(", "));
            return;
        }
        let client = if key.0.is_empty() { "unknown".to_string() } else { key.0.clone() };
        seen.insert(key);
        crate::info!(
            LOG,
            "{rid} accepted parameters without effect: {} (client {client}; reported once per client)",
            req.ignored.join(", ")
        );
    }

    /// Event stream. JSON mode is fully buffered (CAVEAT: no incremental streaming) so it can be validated and repaired.
    pub fn run(self: &Arc<Self>, req: Arc<CanonicalRequest>, rid: String) -> EventStream {
        let this = self.clone();
        Box::pin(async_stream::try_stream! {
            if !req.ignored.is_empty() {
                this.report_ignored(&req, &rid);
            }
            if req.json_schema.is_some() {
                let resp = this.json_mode(&req, &rid).await?;
                if !resp.text.is_empty() {
                    yield Event::Text(resp.text.clone());
                }
                yield Event::Done(resp);
                return;
            }
            let mut text_ = String::new();
            let mut calls: Vec<ToolCall> = vec![];
            let mut final_: Option<CanonicalResponse> = None;
            {
                let mut s = this.stream_once(req.clone(), rid.clone());
                while let Some(ev) = s.next().await {
                    match ev? {
                        Event::Done(r) => {
                            final_ = Some(r);
                            break;
                        }
                        Event::Text(t) => {
                            text_.push_str(&t);
                            yield Event::Text(t);
                        }
                        Event::ToolCall(c) => {
                            calls.push(c.clone());
                            yield Event::ToolCall(c);
                        }
                        Event::Keepalive => {}
                    }
                }
            }
            let mut resp = final_.unwrap_or_default();
            if !resp.rejected_calls.is_empty() && !req.tools.is_empty() && req.tool_choice != ToolChoice::None && (resp.finish == "stop" || resp.finish == "tool_calls") {
                // CAVEAT: a <tool_call> had JSON that could not be decoded; one follow-up asks for only the broken call(s)
                crate::warn!(LOG, "{rid} {} tool call(s) with invalid JSON; asking for corrected calls (1 follow-up)", resp.rejected_calls.len());
                req.meta().followups += 1;
                let mut follow = req.derive(None, None, None, None);
                follow.add("assistant", &text_, calls.clone(), vec![]);
                let bad: Vec<String> = resp.rejected_calls.iter().map(|b| format!("<tool_call>\n{b}\n</tool_call>")).collect();
                let done: Vec<String> = calls.iter().map(|c| format!("{} {}", c.name, followups::call_target(c))).collect();
                let done = done.join("; ");
                let mut ask = format!(
                    "These {} <tool_call> block(s) could not be parsed as JSON, so they were not executed:\n{}\nRe-emit only these call(s) as valid JSON, with the same tool and the same intended values (file contents and commands unchanged). Reply with the <tool_call> block(s) only.",
                    resp.rejected_calls.len(),
                    bad.join("\n")
                );
                if !calls.is_empty() {
                    ask.push_str(&format!(" Do not repeat the calls that already went through ({}).", text::head(&done, 500)));
                }
                follow.add("user", &ask, vec![], vec![]);
                let mut extra: Vec<ToolCall> = vec![];
                let mut extra_usage: Option<Usage> = None;
                let mut seen: HashSet<(String, String)> = calls.iter().map(followups::call_key).collect();
                let sorted = pyjson::Style { ensure_ascii: true, compact: false, sort_keys: true };
                let exact: HashSet<(String, String)> = calls.iter().map(|c| (c.name.clone(), pyjson::dumps(&c.arguments, sorted))).collect();
                let rejected_text = resp.rejected_calls.join("\n");
                let mut dropped = 0;
                let mut s = this.stream_once(Arc::new(follow), format!("{rid}/repair"));
                while let Some(ev) = s.next().await {
                    match ev? {
                        Event::ToolCall(c) => {
                            let key = followups::call_key(&c);
                            let target = key.1.split_once('=').map_or(key.1.as_str(), |(_, t)| t).to_string();
                            let quoted = pyjson::dumps_str(&target, true);
                            let inner = &quoted[1..quoted.len() - 1];
                            let dup = exact.contains(&(c.name.clone(), pyjson::dumps(&c.arguments, sorted))) || (seen.contains(&key) && !rejected_text.contains(inner));
                            if dup || extra.len() >= resp.rejected_calls.len() {
                                dropped += 1;
                                continue;
                            }
                            seen.insert(key);
                            extra.push(c.clone());
                            yield Event::ToolCall(c);
                        }
                        Event::Done(r) => extra_usage = Some(r.usage),
                        _ => {}
                    }
                }
                drop(s);
                crate::info!(LOG, "{rid}/repair kept {} call(s), dropped {dropped} (duplicates of streamed calls or beyond the {} requested)", extra.len(), resp.rejected_calls.len());
                if !extra.is_empty() {
                    let mut all = calls.clone();
                    all.extend(extra);
                    resp.tool_calls = all;
                    resp.finish = "tool_calls".into();
                }
                resp.usage = add_usage(&resp.usage, extra_usage);
                if !resp.tool_calls.is_empty() {
                    calls = resp.tool_calls.clone();
                }
            }
            let ask = if followups::false_incapacity(&req, &resp, &text_, &calls) {
                // CAVEAT: the model denied having web/file/shell access although a listed tool provides it
                let names: Vec<&str> = req.tools.iter().filter(|t| followups::TOOL_ABILITY_RE.is_match(&format!("{} {}", t.name, t.description))).map(|t| t.name.as_str()).collect();
                let names = text::head(&names.join(", "), 300).to_string();
                crate::warn!(LOG, "{rid} response denies an ability that a tool provides ({}); requesting the call (1 follow-up)", text::head(&names, 80));
                Some((format!("{rid}/ability"), format!(
                    "You do have that ability through your tools ({names}). Use the appropriate tool now: reply with the <tool_call> block(s) only, inferring the URL or path if the user did not give one."
                )))
            } else if followups::promise_only(&req, &resp, &text_, &calls) {
                // CAVEAT: the model announced an action and stopped without a <tool_call>; one follow-up asks for the calls
                let confirm = followups::redundant_confirmation(&req, &text_);
                let note = confirm.map_or(String::new(), |c| format!(" [asked to confirm '{c}', already requested]"));
                crate::warn!(LOG, "{rid} response only announces an action without a tool call; requesting the calls (1 follow-up){note}");
                let prompt = match confirm {
                    Some(c) => format!("The request already asks for this ({c}); do not ask for confirmation. Do it now: reply with the <tool_call> block(s) only."),
                    None => "You announced an action but emitted no <tool_call>. Do it now: reply with the <tool_call> block(s) for what you just announced, and nothing else.".to_string(),
                };
                Some((format!("{rid}/act"), prompt))
            } else {
                None
            };
            if let Some((frid, prompt)) = ask {
                req.meta().followups += 1;
                let mut follow = req.derive(None, None, None, None);
                follow.add("assistant", &text_, vec![], vec![]);
                follow.add("user", &prompt, vec![], vec![]);
                let mut extra: Vec<ToolCall> = vec![];
                let mut extra_usage: Option<Usage> = None;
                let mut s = this.stream_once(Arc::new(follow), frid);
                while let Some(ev) = s.next().await {
                    match ev? {
                        Event::ToolCall(c) => {
                            extra.push(c.clone());
                            yield Event::ToolCall(c);
                        }
                        Event::Done(r) => extra_usage = Some(r.usage),
                        _ => {}
                    }
                }
                drop(s);
                if !extra.is_empty() {
                    let mut all = calls.clone();
                    all.extend(extra);
                    resp.tool_calls = all;
                    resp.finish = "tool_calls".into();
                }
                resp.usage = add_usage(&resp.usage, extra_usage);
            }
            yield Event::Done(resp);
        })
    }

    /// Non-streaming: collect everything; retry once when tool_choice is required/named and nothing was called (CAVEAT).
    pub async fn complete(self: &Arc<Self>, req: Arc<CanonicalRequest>, rid: &str) -> Result<CanonicalResponse, Error> {
        let resp = collect(self.run(req.clone(), rid.to_string())).await?;
        let forced = matches!(req.tool_choice, ToolChoice::Required | ToolChoice::Named(_));
        if !req.tools.is_empty() && forced && req.json_schema.is_none() && resp.tool_calls.is_empty() {
            crate::warn!(LOG, "{rid} tool_choice={} but no tool call; retrying once", req.tool_choice.display());
            req.meta().followups += 1;
            let mut follow = req.derive(None, None, None, req.max_tokens);
            follow.add("assistant", &resp.text, vec![], vec![]);
            follow.add("user", "You did not call a tool. You MUST respond with a <tool_call> block now, and nothing else.", vec![], vec![]);
            let mut retry = collect(self.stream_once(Arc::new(follow), format!("{rid}/retry"))).await?;
            if !retry.tool_calls.is_empty() {
                retry.usage = resp.usage.add(&retry.usage);
                return Ok(retry);
            }
        }
        Ok(resp)
    }

    async fn json_mode(self: &Arc<Self>, req: &Arc<CanonicalRequest>, rid: &str) -> Result<CanonicalResponse, Error> {
        let mut resp = collect(self.stream_once(req.clone(), rid.to_string())).await?;
        let schema = req.json_schema.clone().filter(text::truthy).unwrap_or(json!({}));
        if !schema.is_object() && text::truthy(&schema) {
            return Err(Error::Internal(format!("AttributeError(\"'{}' object has no attribute 'get'\")", text::type_name(&schema))));
        }
        let (normalized, errs) = check_json(&resp.text, &schema);
        if let Some(n) = normalized {
            resp.text = n;
            return Ok(resp);
        }
        let joined = errs.join("; ");
        crate::warn!(LOG, "{rid} invalid JSON ({}); one repair attempt", text::head(&joined, 200));
        req.meta().repairs += 1;
        let mut repair = req.derive(Some(vec![]), Some(ToolChoice::None), req.json_schema.clone(), req.max_tokens);
        repair.add("assistant", &resp.text, vec![], vec![]);
        repair.add(
            "user",
            &format!(
                "Your previous response was not valid: {}. Respond again with only the JSON value, no fences, no prose.",
                text::head(&joined, 500)
            ),
            vec![],
            vec![],
        );
        let mut repaired = collect(self.stream_once(Arc::new(repair), format!("{rid}/repair"))).await?;
        let (normalized, errs) = check_json(&repaired.text, &schema);
        repaired.usage = resp.usage.add(&repaired.usage);
        match normalized {
            Some(n) => repaired.text = n,
            None => crate::error!(LOG, "{rid} JSON still invalid after repair: {}", text::head(&errs.join("; "), 200)),
        }
        Ok(repaired)
    }

    fn target(&self, req: &CanonicalRequest) -> String {
        req.route.as_ref().map_or_else(|| self.config.default.target.clone(), |r| r.target.clone())
    }

    /// One backend call (plus one input-too-long retry), parsed into text and tool-call events, then `done`.
    pub fn stream_once(self: &Arc<Self>, req: Arc<CanonicalRequest>, rid: String) -> EventStream {
        let this = self.clone();
        Box::pin(async_stream::try_stream! {
            let (configured, tail_reminder, tool_desc_max) = this.config.knobs(req.route.as_deref());
            let target = this.target(&req);
            let mut max_chars = {
                let learned = this.learned_max_chars.lock().unwrap_or_else(|e| e.into_inner());
                configured.min(*learned.get(&target).unwrap_or(&configured))
            };
            let t0 = Instant::now();
            let mut opened = None;
            let mut prompt = String::new();
            for attempt in 1..=2 {
                let (p, info) = render_prompt(&req, max_chars, tail_reminder, tool_desc_max);
                prompt = p;
                let dropped = if info.dropped_turns > 0 { format!(", -{} dropped", info.dropped_turns) } else { String::new() };
                crate::info!(LOG, "{rid} prompt={} chars (system={}, history={} turns, tools={}{dropped})", info.chars, info.system_chars, info.history_turns, info.tools);
                {
                    let mut m = req.meta();
                    m.prompt_chars += info.chars as i64;
                    m.dropped_turns += info.dropped_turns as i64;
                    m.upstream_calls += 1;
                }
                if info.dropped_turns > 0 {
                    let m = req.meta().clone();
                    this.backend.telemetry.truncated(&m, info.dropped_turns as i64);
                }
                crate::debug!(LOG, "{rid} PROMPT >>>\n{prompt}\n<<< PROMPT");
                let result = match this.backend.stream(&prompt, &target, Some(req.meta.clone())).await {
                    Ok(mut s) => match s.next().await {
                        Some(Ok(item)) => Ok((Some(item), Some(s))),
                        Some(Err(e)) => Err(e),
                        None => Ok((None, Some(s))),
                    },
                    Err(e) => Err(e),
                };
                match result {
                    Ok(x) => {
                        opened = Some(x);
                        break;
                    }
                    Err(Error::Backend(e)) => {
                        let limits = if attempt == 1 { this.backend.input_limit_exceeded(&e) } else { None };
                        match limits {
                            Some((limit, actual)) if actual > limit => {
                                let cap = (info.chars as f64 * limit as f64 / actual.max(1) as f64 * 0.9) as i64;
                                crate::warn!(LOG, "{rid} input above the model's token limit at {} chars; retrying once capped at {cap} chars (kept for this target)", info.chars);
                                this.learned_max_chars.lock().unwrap_or_else(|e| e.into_inner()).insert(target.clone(), cap);
                                max_chars = cap;
                            }
                            _ => Err(Error::Backend(e))?,
                        }
                    }
                    Err(e) => Err(e)?,
                }
            }
            let (head, stream) = opened.unwrap_or((None, None));
            let mut parser = ToolCallParser::new(req.tools.clone());
            let mut limiter = OutputLimiter::new(req.stop.clone(), req.max_tokens.map(|m| (m as f64 * CHARS_PER_TOKEN) as i64));
            let mut calls: Vec<ToolCall> = vec![];
            let mut final_ = Completion::default();
            let mut emitted = 0usize;
            let mut received = 0usize;
            let mut first = true;
            let mut stopped = false;
            if let Some(head) = head {
                let mut stream = stream;
                let mut next: Option<Item> = Some(head);
                while let Some(item) = next.take() {
                    match item {
                        Item::Completion(c) => final_ = c,
                        Item::Text(t) => {
                            received += text::len(&t);
                            if first {
                                crate::info!(LOG, "{rid} ttfb {:.2}s", t0.elapsed().as_secs_f64());
                                first = false;
                            }
                            for p in parser.feed(&t) {
                                match p {
                                    Parsed::Text(v) => {
                                        let (out, stop) = limiter.apply(&v, false);
                                        stopped = stop;
                                        if !out.is_empty() {
                                            emitted += text::len(&out);
                                            yield Event::Text(out);
                                        }
                                        if stopped {
                                            break;
                                        }
                                    }
                                    Parsed::Call(c) => {
                                        calls.push(c.clone());
                                        yield Event::ToolCall(c);
                                    }
                                }
                            }
                            if stopped {
                                break; // CAVEAT: the backend stream is closed; the tokens already generated were billed
                            }
                        }
                    }
                    next = match stream.as_mut() {
                        Some(s) => match s.next().await {
                            Some(Ok(i)) => Some(i),
                            Some(Err(e)) => Err(e)?,
                            None => None,
                        },
                        None => None,
                    };
                }
                drop(stream); // releases the backend slot now
            }
            if !stopped {
                let tail = parser.finish();
                let mut tail_text = String::new();
                for p in tail {
                    match p {
                        Parsed::Call(c) => {
                            calls.push(c.clone());
                            yield Event::ToolCall(c);
                        }
                        Parsed::Text(t) => tail_text.push_str(&t),
                    }
                }
                let (out, _) = limiter.apply(&tail_text, true);
                if !out.is_empty() {
                    emitted += text::len(&out);
                    yield Event::Text(out);
                }
            }
            if !parser.errors.is_empty() {
                crate::warn!(LOG, "{rid} parser: {}", parser.errors.join("; "));
                req.meta().parse_errors += parser.errors.len() as i64;
            }
            let finish = if !calls.is_empty() && limiter.finish == "stop" { "tool_calls".to_string() } else { limiter.finish.clone() };
            let usage = match final_.usage {
                Some(u) => u,
                None => {
                    // CAVEAT: the backend's token counts were not read: estimated from characters
                    let p = estimate_tokens(&prompt);
                    let c = ((received as f64 / CHARS_PER_TOKEN) as i64).max(1);
                    req.meta().usage_estimated = true;
                    Usage { prompt_tokens: p, completion_tokens: c, total_tokens: p + c }
                }
            };
            let resp = CanonicalResponse {
                text: String::new(),
                tool_calls: calls.clone(),
                finish: finish.clone(),
                stop_sequence: limiter.stop_sequence.clone(),
                usage,
                message_id: final_.message_id.clone(),
                rejected_calls: parser.rejected.clone(),
            };
            crate::info!(LOG, "{rid} ok in {:.1}s, {emitted} chars, {} tool calls, finish={finish}, usage={}", t0.elapsed().as_secs_f64(), calls.len(), resp.usage.repr());
            let _ = Value::Null;
            yield Event::Done(resp);
        })
    }
}

pub async fn collect(mut events: EventStream) -> Result<CanonicalResponse, Error> {
    let mut text_ = String::new();
    let mut calls: Vec<ToolCall> = vec![];
    let mut final_: Option<CanonicalResponse> = None;
    while let Some(ev) = events.next().await {
        match ev? {
            Event::Text(t) => text_.push_str(&t),
            Event::ToolCall(c) => calls.push(c),
            Event::Done(r) => final_ = Some(r),
            Event::Keepalive => {}
        }
    }
    let mut resp = final_.unwrap_or_default();
    resp.text = if calls.is_empty() { text_ } else { text::strip(&text_).to_string() };
    resp.tool_calls = calls;
    Ok(resp)
}
