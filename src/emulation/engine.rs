//! Runs canonical requests on a text-only backend: renders the prompt, streams the backend's text through the
//! tool-call parser and the output limiter, and makes the automatic follow-ups (invalid tool JSON, forced tool choice,
//! false incapacity, announce-and-stop, JSON-mode repair). Each follow-up is a function that returns the request to
//! send, or None.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::json;

use super::followups;
use super::jsonmode::check_json;
use super::output::OutputLimiter;
use super::parser::{Parsed, ToolCallParser};
use super::prompt::render_prompt;
use crate::backends::{Completion, Item, TextBackend};
use crate::canonical::{CHARS_PER_TOKEN, CanonicalRequest, CanonicalResponse, Event, Finish, ToolCall, ToolChoice, Usage, estimate_tokens};
use crate::config::Config;
use crate::errors::Error;
use crate::json;
use crate::telemetry::Telemetry;
use crate::text::{char_len, prefix};

pub type EventStream = BoxStream<'static, Result<Event, Error>>;

/// Follow-ups that ask for tool calls (forced tool choice, false incapacity, announce-and-stop): at most this many per
/// turn; a second one only when the first reply announced again without acting.
const MAX_ACT_FOLLOWUPS: usize = 2;
static NAME_KEY_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r#""name"\s*:"#).unwrap());

pub struct EmulationEngine {
    pub backend: Arc<dyn TextBackend>,
    telemetry: Arc<Telemetry>,
    config: Arc<Config>,
    /// target -> prompt cap learned from an input-too-long refusal
    learned_max_chars: Mutex<HashMap<String, i64>>,
    ignored_seen: Mutex<HashSet<(String, Vec<String>)>>,
}

fn add_usage(a: &Usage, b: Option<Usage>) -> Usage {
    b.map_or(*a, |b| a.add(&b))
}

/// A follow-up call: the request, the suffix of its log id ("repair", "act", ...) and what it asks.
struct FollowUp {
    req: CanonicalRequest,
    suffix: String,
    prompt: String,
}

/// What a reply produced so far: its text (as streamed to the client), its tool calls and how it ended.
#[derive(Default)]
struct Reply {
    text: String,
    calls: Vec<ToolCall>,
    resp: CanonicalResponse,
}

/// The invalid-JSON repair: one follow-up that asks for the broken call(s) only.
fn repair_request(req: &CanonicalRequest, reply: &Reply) -> Option<FollowUp> {
    let rejected = &reply.resp.rejected_calls;
    if rejected.is_empty() || !req.tools_on() || !matches!(reply.resp.finish, Finish::Stop | Finish::ToolCalls) {
        return None;
    }
    let mut follow = req.derive();
    follow.add("assistant", &reply.text, reply.calls.clone(), vec![]);
    let bad: Vec<String> = rejected.iter().map(|b| format!("<tool_call>\n{b}\n</tool_call>")).collect();
    let mut ask = format!(
        "These {} <tool_call> block(s) could not be parsed as JSON, so they were not executed:\n{}\nRe-emit only these call(s) as valid JSON, with the same tool and the same intended values (file contents and commands unchanged). Reply with the <tool_call> block(s) only.",
        rejected.len(),
        bad.join("\n")
    );
    if !reply.calls.is_empty() {
        let done: Vec<String> = reply.calls.iter().map(|c| format!("{} {}", c.name, followups::call_target(c))).collect();
        ask.push_str(&format!(" Do not repeat the calls that already went through ({}).", prefix(&done.join("; "), 500)));
    }
    follow.add_text("user", &ask);
    Some(FollowUp { req: follow, suffix: "repair".into(), prompt: ask })
}

/// Which calls of a repair reply to keep: not a re-emission of a call that already went through (unless the broken
/// block targeted the same thing, a second edit of the same file), and no more than were asked for.
struct RepairFilter {
    seen: HashSet<(String, String)>,
    exact: HashSet<(String, String)>,
    rejected_text: String,
    wanted: usize,
    kept: usize,
    dropped: usize,
}

impl RepairFilter {
    fn new(calls: &[ToolCall], rejected: &[String]) -> Self {
        // a broken block may hold several calls: one per "name" key in it, at least one per block
        let wanted = rejected.iter().map(|b| NAME_KEY_RE.find_iter(b).count().max(1)).sum();
        RepairFilter {
            seen: calls.iter().map(followups::call_key).collect(),
            exact: calls.iter().map(|c| (c.name.clone(), json::sorted(&c.arguments))).collect(),
            rejected_text: rejected.join("\n"),
            wanted,
            kept: 0,
            dropped: 0,
        }
    }

    fn accept(&mut self, c: &ToolCall) -> bool {
        let key = followups::call_key(c);
        let target = key.1.split_once('=').map_or(key.1.as_str(), |(_, t)| t);
        let quoted = json!(target).to_string();
        let as_in_json = &quoted[1..quoted.len() - 1];
        let dup = self.exact.contains(&(c.name.clone(), json::sorted(&c.arguments)))
            || (self.seen.contains(&key) && !self.rejected_text.contains(as_in_json));
        if dup || self.kept >= self.wanted {
            self.dropped += 1;
            return false;
        }
        self.seen.insert(key);
        self.kept += 1;
        true
    }
}

/// The follow-up that asks for tool calls the reply should have made, if any: a forced tool choice with no call, a
/// denied ability a tool provides, or an announced action without its call. `round` > 0 asks again after a follow-up
/// whose reply still announced without acting.
fn act_request(req: &CanonicalRequest, reply: &Reply, history: &[(String, String)], round: usize) -> Option<FollowUp> {
    if !reply.calls.is_empty() || !req.tools_on() || reply.resp.finish != Finish::Stop {
        return None;
    }
    let again = if round > 0 { format!("{}", round + 1) } else { String::new() };
    let (suffix, prompt, keep_max_tokens) = if req.tool_choice.forced() && req.json_schema.is_none() {
        // CAVEAT: tool_choice required/named is a prompt instruction; the model ignored it
        tracing::warn!("tool_choice={} but no tool call; asking again", req.tool_choice);
        ("retry", "You did not call a tool. You MUST respond with a <tool_call> block now, and nothing else.".to_string(), true)
    } else if let tools @ [_, ..] = followups::false_incapacity(req, &reply.resp, &reply.text, &reply.calls).as_slice() {
        // CAVEAT: the model denied having web/file/shell access although a listed tool provides it
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        let names = prefix(&names.join(", "), 300).to_string();
        tracing::warn!("response denies an ability that a tool provides ({}); requesting the call", prefix(&names, 80));
        (
            "ability",
            format!(
                "You do have that ability through your tools ({names}). Use the appropriate tool now: reply with the <tool_call> block(s) only, inferring the URL or path if the user did not give one."
            ),
            false,
        )
    } else if followups::promise_only(req, &reply.resp, &reply.text, &reply.calls) {
        // CAVEAT: the model announced an action and stopped without a <tool_call>
        let confirm = followups::redundant_confirmation(req, &reply.text);
        let note = confirm.map_or(String::new(), |c| format!(" [asked to confirm '{c}', already requested]"));
        tracing::warn!("response only announces an action without a tool call; requesting the calls{note}");
        let prompt = match confirm {
            Some(c) => format!("The request already asks for this ({c}); do not ask for confirmation. Do it now, and only that: reply with the <tool_call> block(s) only."),
            None => "You announced an action but emitted no <tool_call>. Do it now: reply with the <tool_call> block(s) for what you just announced, and nothing else.".to_string(),
        };
        ("act", prompt, false)
    } else if followups::forgotten_commit(req, &reply.resp, &reply.text, &reply.calls) {
        // CAVEAT: the final report claims the work done, but the commit the user ordered was never made
        tracing::warn!("response reports the work done without the commit the request orders; requesting it");
        (
            "commit",
            "The request also asks for a commit, and none was made. Make it now: reply with the <tool_call> block(s) only.".to_string(),
            false,
        )
    } else {
        return None;
    };
    let mut follow = if keep_max_tokens { CanonicalRequest { max_tokens: req.max_tokens, ..req.derive() } } else { req.derive() };
    for (assistant, user) in history {
        follow.add_text("assistant", assistant);
        follow.add_text("user", user);
    }
    follow.add_text("assistant", &reply.text);
    follow.add_text("user", &prompt);
    Some(FollowUp { req: follow, suffix: format!("{suffix}{again}"), prompt })
}

/// `parallel_tool_calls: false`: the first call goes through, later ones are dropped (and counted).
struct CallGate {
    one: bool,
    passed: usize,
    dropped: usize,
}

impl CallGate {
    fn new(req: &CanonicalRequest) -> Self {
        CallGate { one: !req.parallel_tool_calls, passed: 0, dropped: 0 }
    }

    /// Whether another call would go through.
    fn open(&self) -> bool {
        !(self.one && self.passed > 0)
    }

    fn pass(&mut self) -> bool {
        if !self.open() {
            self.dropped += 1;
            return false;
        }
        self.passed += 1;
        true
    }
}

impl EmulationEngine {
    pub fn new(backend: Arc<dyn TextBackend>, telemetry: Arc<Telemetry>, config: Arc<Config>) -> Self {
        EmulationEngine {
            backend,
            telemetry,
            config,
            learned_max_chars: Mutex::new(HashMap::new()),
            ignored_seen: Mutex::new(HashSet::new()),
        }
    }

    /// Parameters accepted without effect: reported once per client and set of parameters at INFO, then at DEBUG.
    fn report_ignored(&self, req: &CanonicalRequest, rid: &str) {
        let mut sorted = req.ignored.clone();
        sorted.sort();
        let key = (req.meta().client.clone(), sorted);
        let mut seen = self.ignored_seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.contains(&key) {
            tracing::debug!("{rid} accepted parameters without effect: {}", req.ignored.join(", "));
            return;
        }
        let client = if key.0.is_empty() { "unknown".to_string() } else { key.0.clone() };
        seen.insert(key);
        tracing::info!("{rid} accepted parameters without effect: {} (client {client}; reported once per client)", req.ignored.join(", "));
    }

    /// The event stream of one request: the first backend call streamed to the client, then the follow-ups' tool calls.
    /// JSON mode is fully buffered (CAVEAT: no incremental streaming) so it can be validated and repaired.
    pub fn run(self: &Arc<Self>, req: Arc<CanonicalRequest>, rid: String) -> EventStream {
        let this = self.clone();
        Box::pin(async_stream::try_stream! {
            if !req.ignored.is_empty() {
                this.report_ignored(&req, &rid);
            }
            if req.json_schema.is_some() {
                let mut s = this.json_mode(req.clone(), rid.clone());
                while let Some(ev) = s.next().await {
                    yield ev?;
                }
                return;
            }
            let mut gate = CallGate::new(&req);
            let mut reply = Reply::default();
            {
                let mut s = this.stream_once(req.clone(), rid.clone());
                while let Some(ev) = s.next().await {
                    match ev? {
                        Event::Done(r) => {
                            reply.resp = r;
                            break;
                        }
                        Event::Text(t) => {
                            reply.text.push_str(&t);
                            yield Event::Text(t);
                        }
                        Event::ToolCall(c) => {
                            if gate.pass() {
                                reply.calls.push(c.clone());
                                yield Event::ToolCall(c);
                            }
                        }
                        e @ Event::Prompt { .. } => yield e,
                        Event::Keepalive => {}
                    }
                }
            }
            reply.resp.tool_calls.clone_from(&reply.calls);
            if let Some(follow) = repair_request(&req, &reply).filter(|_| gate.open()) {
                // CAVEAT: a <tool_call> had JSON that could not be decoded; one follow-up asks for only the broken call(s)
                tracing::warn!("{rid} {} tool call(s) with invalid JSON; asking for corrected calls (1 follow-up)", reply.resp.rejected_calls.len());
                req.meta().followups += 1;
                let mut filter = RepairFilter::new(&reply.calls, &reply.resp.rejected_calls);
                let mut usage = None;
                let mut s = this.stream_once(Arc::new(follow.req), format!("{rid}/{}", follow.suffix));
                while let Some(ev) = s.next().await {
                    match ev? {
                        Event::ToolCall(c) => {
                            if gate.open() && filter.accept(&c) && gate.pass() {
                                reply.calls.push(c.clone());
                                yield Event::ToolCall(c);
                            }
                        }
                        Event::Done(r) => usage = Some(r.usage),
                        _ => {}
                    }
                }
                drop(s);
                tracing::info!("{rid}/repair kept {} call(s), dropped {} (duplicates of streamed calls or beyond the {} requested)", filter.kept, filter.dropped, filter.wanted);
                reply.resp.usage = add_usage(&reply.resp.usage, usage);
            }
            // follow-ups that ask for calls the reply should have made; their text is not shown, only their calls
            let mut history: Vec<(String, String)> = vec![];
            let mut current = Reply { text: reply.text.clone(), calls: reply.calls.clone(), resp: reply.resp.clone() };
            for round in 0..MAX_ACT_FOLLOWUPS {
                let Some(follow) = act_request(&req, &current, &history, round) else { break };
                req.meta().followups += 1;
                let prompt = follow.prompt;
                let mut next = Reply::default();
                let mut s = this.stream_once(Arc::new(follow.req), format!("{rid}/{}", follow.suffix));
                while let Some(ev) = s.next().await {
                    match ev? {
                        Event::ToolCall(c) => {
                            if gate.pass() {
                                next.calls.push(c.clone());
                                yield Event::ToolCall(c);
                            }
                        }
                        Event::Text(t) => next.text.push_str(&t),
                        Event::Done(r) => next.resp = r,
                        _ => {}
                    }
                }
                drop(s);
                reply.resp.usage = add_usage(&reply.resp.usage, Some(next.resp.usage));
                reply.calls.extend(next.calls.iter().cloned());
                history.push((current.text.clone(), prompt));
                current = next;
            }
            if gate.dropped > 0 {
                tracing::info!("{rid} parallel_tool_calls=false: kept the first tool call, dropped {}", gate.dropped);
            }
            if !reply.calls.is_empty() {
                reply.resp.finish = if reply.resp.finish == Finish::Stop { Finish::ToolCalls } else { reply.resp.finish };
            }
            reply.resp.tool_calls = reply.calls;
            yield Event::Done(reply.resp);
        })
    }

    /// Non-streaming: everything `run` streams, collected.
    pub async fn complete(self: &Arc<Self>, req: Arc<CanonicalRequest>, rid: &str) -> Result<CanonicalResponse, Error> {
        collect(self.run(req, rid.to_string())).await
    }

    /// Structured output: the reply must be a JSON value matching the schema (one repair otherwise). A reply with tool
    /// calls is an answer of its own: the calls are returned as they are.
    fn json_mode(self: &Arc<Self>, req: Arc<CanonicalRequest>, rid: String) -> EventStream {
        let this = self.clone();
        Box::pin(async_stream::try_stream! {
            let mut gate = CallGate::new(&req);
            let mut reply = Reply::default();
            {
                let mut s = this.stream_once(req.clone(), rid.clone());
                while let Some(ev) = s.next().await {
                    match ev? {
                        e @ Event::Prompt { .. } => yield e,
                        Event::Text(t) => reply.text.push_str(&t),
                        Event::ToolCall(c) => {
                            if gate.pass() {
                                reply.calls.push(c);
                            }
                        }
                        Event::Done(r) => reply.resp = r,
                        Event::Keepalive => {}
                    }
                }
            }
            let mut resp = reply.resp;
            if !reply.calls.is_empty() {
                resp.text = reply.text.trim().to_string();
                if !resp.text.is_empty() {
                    yield Event::Text(resp.text.clone());
                }
                for c in &reply.calls {
                    yield Event::ToolCall(c.clone());
                }
                resp.tool_calls = reply.calls;
                yield Event::Done(resp);
                return;
            }
            let schema = req.json_schema.clone().unwrap_or_else(|| json!({}));
            let (normalized, errs) = check_json(&reply.text, &schema);
            if let Some(n) = normalized {
                resp.text = n;
            } else {
                let joined = errs.join("; ");
                tracing::warn!("{rid} invalid JSON ({}); one repair attempt", prefix(&joined, 200));
                req.meta().repairs += 1;
                let mut repair = CanonicalRequest {
                    tools: Arc::new([]),
                    tool_choice: ToolChoice::None,
                    json_schema: req.json_schema.clone(),
                    max_tokens: req.max_tokens,
                    ..req.derive()
                };
                repair.add_text("assistant", &reply.text);
                repair.add_text(
                    "user",
                    &format!("Your previous response was not valid: {}. Respond again with only the JSON value, no fences, no prose.", prefix(&joined, 500)),
                );
                let mut repaired = collect(this.stream_once(Arc::new(repair), format!("{rid}/repair"))).await?;
                let (normalized, errs) = check_json(&repaired.text, &schema);
                repaired.usage = resp.usage.add(&repaired.usage);
                match normalized {
                    Some(n) => repaired.text = n,
                    None => tracing::error!("{rid} JSON still invalid after repair: {}", prefix(&errs.join("; "), 200)),
                }
                resp = repaired;
            }
            if !resp.text.is_empty() {
                yield Event::Text(resp.text.clone());
            }
            yield Event::Done(resp);
        })
    }

    fn target(&self, req: &CanonicalRequest) -> String {
        req.route.as_ref().map_or_else(|| self.config.default.target.clone(), |r| r.target.clone())
    }

    /// One backend call (plus one input-too-long retry), parsed into text and tool-call events, then `done`. The first
    /// event says how big the prompt is.
    pub fn stream_once(self: &Arc<Self>, req: Arc<CanonicalRequest>, rid: String) -> EventStream {
        let this = self.clone();
        let meta = req.meta.clone();
        let label = rid.split_once('/').map_or("first", |(_, s)| s).to_string();
        let call = Box::pin(async_stream::try_stream! {
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
                let mut cut = String::new();
                if info.dropped_turns > 0 {
                    cut.push_str(&format!(", -{} dropped", info.dropped_turns));
                }
                if info.shrunk > 0 {
                    cut.push_str(&format!(", {} cut", info.shrunk));
                }
                tracing::info!("{rid} prompt={} chars (system={}, history={} turns, tools={}{cut})", info.chars, info.system_chars, info.history_turns, info.tools);
                {
                    let mut m = req.meta();
                    m.prompt_chars += info.chars as i64;
                    m.dropped_turns += info.dropped_turns as i64;
                    m.shrunk_parts += info.shrunk as i64;
                    m.upstream_calls += 1;
                }
                if info.dropped_turns > 0 || info.shrunk > 0 {
                    let m = req.meta().clone();
                    this.telemetry.truncated(&m, info.dropped_turns as i64);
                }
                yield Event::Prompt { tokens: estimate_tokens(&prompt) };
                tracing::debug!("{rid} PROMPT >>>\n{prompt}\n<<< PROMPT");
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
                                tracing::warn!("{rid} input above the model's token limit at {} chars; retrying once capped at {cap} chars (kept for this target)", info.chars);
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
            let mut limiter = OutputLimiter::new(req.stop.clone(), req.max_tokens.map(|m| (m as f64 * CHARS_PER_TOKEN) as usize));
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
                            received += char_len(&t);
                            if first {
                                tracing::info!("{rid} ttfb {:.2}s", t0.elapsed().as_secs_f64());
                                first = false;
                            }
                            for p in parser.feed(&t) {
                                let (out, stop) = match &p {
                                    Parsed::Text(v) => limiter.apply(v, false),
                                    // text held back for a stop sequence goes out before the call, not after it
                                    Parsed::Call(_) => limiter.apply("", true),
                                };
                                stopped = stop;
                                if !out.is_empty() {
                                    emitted += char_len(&out);
                                    yield Event::Text(out);
                                }
                                if stopped {
                                    break;
                                }
                                if let Parsed::Call(c) = p {
                                    calls.push(c.clone());
                                    yield Event::ToolCall(c);
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
                let mut tail_text = String::new();
                for p in parser.finish() {
                    match p {
                        Parsed::Call(c) => {
                            let (out, _) = limiter.apply(&std::mem::take(&mut tail_text), true);
                            if !out.is_empty() {
                                emitted += char_len(&out);
                                yield Event::Text(out);
                            }
                            calls.push(c.clone());
                            yield Event::ToolCall(c);
                        }
                        Parsed::Text(t) => tail_text.push_str(&t),
                    }
                }
                let (out, _) = limiter.apply(&tail_text, true);
                if !out.is_empty() {
                    emitted += char_len(&out);
                    yield Event::Text(out);
                }
            }
            if !parser.errors.is_empty() {
                tracing::warn!("{rid} parser: {}", parser.errors.join("; "));
                req.meta().parse_errors += parser.errors.len() as i64;
            }
            let finish = if !calls.is_empty() && limiter.finish == Finish::Stop { Finish::ToolCalls } else { limiter.finish };
            let usage = match final_.usage {
                Some(u) => u,
                None => {
                    // CAVEAT: the backend's token counts were not read: estimated from characters
                    let p = estimate_tokens(&prompt);
                    let c = ((received as f64 / CHARS_PER_TOKEN) as i64).max(1);
                    req.meta().usage_estimated = true;
                    Usage::new(p, c)
                }
            };
            let resp = CanonicalResponse {
                text: String::new(),
                tool_calls: calls.clone(),
                finish,
                stop_sequence: limiter.stop_sequence.clone(),
                usage,
                message_id: final_.message_id.clone(),
                rejected_calls: parser.rejected.clone(),
            };
            tracing::info!("{rid} ok in {:.1}s, {emitted} chars, {} tool calls, finish={finish}, usage={}", t0.elapsed().as_secs_f64(), calls.len(), resp.usage);
            yield Event::Done(resp);
        });
        self.telemetry.trace_call(call, &meta, self.backend.kind(), &label)
    }
}

/// Everything a stream says, as one response (text, calls, the final usage and finish).
pub async fn collect(mut events: EventStream) -> Result<CanonicalResponse, Error> {
    let mut text_ = String::new();
    let mut calls: Vec<ToolCall> = vec![];
    let mut final_: Option<CanonicalResponse> = None;
    while let Some(ev) = events.next().await {
        match ev? {
            Event::Text(t) => text_.push_str(&t),
            Event::ToolCall(c) => calls.push(c),
            Event::Done(r) => final_ = Some(r),
            Event::Keepalive | Event::Prompt { .. } => {}
        }
    }
    let mut resp = final_.unwrap_or_default();
    resp.text = if calls.is_empty() { text_ } else { text_.trim().to_string() };
    resp.tool_calls = calls;
    Ok(resp)
}
