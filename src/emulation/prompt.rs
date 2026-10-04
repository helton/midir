//! Canonical request -> one text prompt for a text-only backend: the client's system prompt, the tool protocol, the
//! output format, the conversation and the last turn; cut to the model's size cap when needed.

use std::borrow::Cow;
use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Value, json};

use crate::canonical::{CanonicalRequest, ToolChoice, ToolSpec, Turn};
use crate::json;
use crate::text::{byte_offset_from_end, char_len, ellipsize, prefix};

pub const TOOL_PROTOCOL: &str = r#"# Tools
You can call tools. To call one, write exactly this block:
<tool_call id="call_1">
{"name": "<tool name>", "arguments": {<JSON arguments matching the tool's schema>}}
</tool_call>
Rules:
- One <tool_call> block per call. You may emit several blocks in one response when the calls are independent; number their ids call_1, call_2, ... within the response.
- Text before the first <tool_call> is allowed (the user will see it). Write nothing after the last </tool_call>.
- Results arrive in the next user message as <tool_result id="..." name="...">...</tool_result>. Never fabricate results; wait for them.
- If no tool is needed, answer in plain text with no <tool_call> block.
- "arguments" must be valid JSON and follow the schema exactly (required fields, types). Use "arguments": {} for tools without parameters. Never wrap the block in code fences.
- Text changes nothing: to create or edit a file, call a tool that writes it, in that tool's own arguments. Never put a patch, a diff or a file's new content in your reply for it to be applied ("*** Begin Patch", "diff --git"): nothing applies text.
- Announcing is not acting: if you tell the user you are about to do something that needs a tool, the <tool_call> block(s) MUST follow in the SAME response, right after the sentence. A response that only promises an action ("I will read the files...") and contains no <tool_call> is an error. When several independent calls are needed (e.g. reading several files), emit them all in one response instead of one per turn.
Tool results do not end the task: after reading them, keep working (call more tools) until the user's request is fully handled; stop to ask the user only when you truly need a decision from them.
Before you end a reply without a <tool_call>, check its last paragraph: if it is a plan, a list of next steps, or a promise about work not done yet ("I'll...", "Vou...", "Next...", "O plano é..."), do that work now with <tool_call> blocks instead of describing it. A step you have decided on is something to run, not to announce. End a reply without a tool call only when the request is fully handled, the question is answered, you need a decision only the user can make, or you are waiting for background work you already started (do not start it again). Never ask the user to confirm an action their request already asks for (for example a commit the task requests): do it.
Your capabilities are exactly the tools listed below. If a listed tool fetches web pages, searches, reads files or runs commands, then you DO have that access: never say you cannot access the internet, files or a terminal when a matching tool exists. When the user asks for current, external or verifiable information (a URL, a latest version, today's data), call the matching tool instead of answering from memory or declining. If the user names a page, site or repository without giving its URL, infer the most likely URL (e.g. the project's official site or GitHub repository) and fetch it.
Available tools (one JSON object per line: name, description, parameters as JSON Schema):
"#;
const TOOL_CHOICE_REQUIRED: &str = "\nIn this response you MUST call at least one tool; a plain-text answer is not acceptable.";
const ONE_CALL: &str = "\nIn this response call at most one tool: write a single <tool_call> block.";
pub const ASSISTANT_PREFILL_NUDGE: &str = "(continue your previous message exactly from where it stopped, without repeating it)";
const KEEP_RECENT_TURNS: usize = 4;
const TAIL_REMINDER_TEXT: &str = "<reminder>Check your last paragraph before ending: if it announces, plans or promises an action, emit its <tool_call> block(s) now, in this reply; several independent calls go in one reply. Never claim you lack an ability a listed tool provides.</reminder>";
/// A tool result or message cut to fit the size cap keeps at least this many characters (head and tail).
const MIN_KEPT_CHARS: usize = 2000;

/// Copy of a JSON Schema with every nested "description" cut at `limit` characters.
fn truncate_descriptions(schema: &Value, limit: usize) -> Value {
    match schema {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| match v {
                    Value::String(s) if k == "description" => (k.clone(), json!(ellipsize(s, limit))),
                    _ => (k.clone(), truncate_descriptions(v, limit)),
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|v| truncate_descriptions(v, limit)).collect()),
        other => other.clone(),
    }
}

fn non_empty(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Object(m)) => !m.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        _ => false,
    }
}

/// One tool per line as compact JSON without empty keys. `desc_max` > 0 cuts descriptions (also nested ones).
fn tool_line(t: &ToolSpec, desc_max: i64) -> String {
    #[derive(Serialize)]
    struct Line<'a> {
        name: &'a str,
        #[serde(skip_serializing_if = "str::is_empty")]
        description: Cow<'a, str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parameters: Option<Cow<'a, Value>>,
    }
    let limit = usize::try_from(desc_max).ok().filter(|n| *n > 0);
    let params = &t.parameters;
    let line = Line {
        name: &t.name,
        description: limit.map_or(Cow::Borrowed(t.description.as_str()), |n| Cow::Owned(ellipsize(&t.description, n))),
        parameters: (non_empty(params.get("properties")) || non_empty(params.get("required")))
            .then(|| limit.map_or(Cow::Borrowed(params), |n| Cow::Owned(truncate_descriptions(params, n)))),
    };
    serde_json::to_string(&line).unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub struct PromptInfo {
    pub chars: usize,
    pub dropped_turns: usize,
    /// tool results and messages cut in the middle to fit the cap
    pub shrunk: usize,
    pub system_chars: usize,
    pub history_turns: usize,
    pub tools: usize,
}

/// call id -> tool name, for tool results that came without their tool's name.
fn call_names(req: &CanonicalRequest) -> HashMap<&str, &str> {
    req.turns.iter().flat_map(|t| &t.tool_calls).map(|c| (c.id.as_str(), c.name.as_str())).rev().collect()
}

fn render_turn(names: &HashMap<&str, &str>, t: &Turn) -> String {
    let mut out =
        String::with_capacity(16 + t.text.len() + t.after.len() + t.tool_results.iter().map(|r| r.content.len() + 64).sum::<usize>());
    out.push('[');
    out.push_str(&t.role);
    out.push_str("]: ");
    let mut first = true;
    let mut part = |out: &mut String| {
        if !first {
            out.push('\n');
        }
        first = false;
    };
    if !t.text.is_empty() {
        part(&mut out);
        out.push_str(&t.text);
    }
    for c in &t.tool_calls {
        part(&mut out);
        // arguments that are not JSON (a client's broken history) are shown as they came
        let args = match &c.arguments {
            Value::String(s) => Cow::Borrowed(s.as_str()),
            other => Cow::Owned(json::readable(other)),
        };
        out.push_str(&format!(
            "<tool_call id=\"{}\">\n{{\"name\": {}, \"arguments\": {args}}}\n</tool_call>",
            c.id,
            json::readable(&c.name)
        ));
    }
    for r in &t.tool_results {
        part(&mut out);
        let err = if r.is_error { " is_error=\"true\"" } else { "" };
        let name = if r.name.is_empty() { names.get(r.call_id.as_str()).copied().unwrap_or("") } else { r.name.as_str() };
        out.push_str(&format!("<tool_result id=\"{}\" name=\"{name}\"{err}>\n", r.call_id));
        out.push_str(&r.content);
        out.push_str("\n</tool_result>");
    }
    if !t.after.is_empty() {
        part(&mut out);
        out.push_str(&t.after);
    }
    out
}

fn omitted_notice(dropped: usize) -> String {
    format!(
        "[{dropped} earlier turns were omitted: the conversation exceeded the size limit. Do not assume their content; ask or re-read files if you need it.]"
    )
}

const CONVERSATION_OPEN: &str = "<conversation>\n";
const CONVERSATION_CLOSE: &str = "\n</conversation>";

/// The rendered pieces of a prompt and their sizes in characters, so the size of any cut is known without building it.
struct Layout {
    system: Option<String>,
    history: Vec<String>,
    /// suffix[i] = characters of history[i..]
    suffix: Vec<usize>,
    last: String,
    reminder: bool,
}

impl Layout {
    fn new(system: Option<String>, history: Vec<String>, last: String, reminder: bool) -> Self {
        let mut suffix = vec![0; history.len() + 1];
        for i in (0..history.len()).rev() {
            suffix[i] = suffix[i + 1] + char_len(&history[i]);
        }
        Layout { system, history, suffix, last, reminder }
    }

    /// Size of the prompt that keeps history[start..] (and says that `start` turns were omitted).
    fn chars(&self, start: usize) -> usize {
        let mut parts = 0;
        let mut total = 0;
        if let Some(s) = &self.system {
            parts += 1;
            total += char_len(s);
        }
        let kept = self.history.len() - start;
        let inner = kept + usize::from(start > 0);
        if inner > 0 {
            parts += 1;
            let notice = if start > 0 { char_len(&omitted_notice(start)) } else { 0 };
            total += char_len(CONVERSATION_OPEN) + notice + self.suffix[start] + (inner - 1) + char_len(CONVERSATION_CLOSE);
        }
        parts += 1;
        total += char_len(&self.last);
        if self.reminder {
            parts += 1;
            total += char_len(TAIL_REMINDER_TEXT);
        }
        total + parts - 1
    }

    fn build(&self, start: usize) -> String {
        let mut out = String::with_capacity(self.history[start..].iter().map(String::len).sum::<usize>() + self.last.len() + 4096);
        if let Some(s) = &self.system {
            out.push_str(s);
            out.push('\n');
        }
        if start > 0 || start < self.history.len() {
            out.push_str(CONVERSATION_OPEN);
            let mut first = true;
            if start > 0 {
                out.push_str(&omitted_notice(start));
                first = false;
            }
            for h in &self.history[start..] {
                if !first {
                    out.push('\n');
                }
                out.push_str(h);
                first = false;
            }
            out.push_str(CONVERSATION_CLOSE);
            out.push('\n');
        }
        out.push_str(&self.last);
        if self.reminder {
            out.push('\n');
            out.push_str(TAIL_REMINDER_TEXT);
        }
        out
    }
}

fn cut_marker(omitted: usize) -> String {
    format!("[… {omitted} characters omitted by the gateway: the content exceeded the size limit …]")
}

/// Length of a text of `len` characters cut to keep `keep` of them (head and tail around the marker).
fn cut_len(len: usize, keep: usize) -> usize {
    if len <= keep { len } else { keep + 2 + char_len(&cut_marker(len - keep)) }
}

/// The text cut in the middle: the first and last `keep / 2` characters around a marker that says how much went.
fn cut_middle(text: &str, keep: usize) -> String {
    let len = char_len(text);
    if cut_len(len, keep) >= len {
        return text.to_string();
    }
    let head = prefix(text, keep / 2);
    let tail = &text[byte_offset_from_end(text, keep - keep / 2)..];
    format!("{head}\n{}\n{tail}", cut_marker(len - keep))
}

/// The largest size to keep per piece so that cutting the longer pieces saves `excess` characters; MIN_KEPT_CHARS
/// when even that is not enough.
fn keep_size(lens: &[usize], excess: usize) -> usize {
    let saving = |keep: usize| lens.iter().map(|&l| l - cut_len(l, keep).min(l)).sum::<usize>();
    let (mut lo, mut hi) = (MIN_KEPT_CHARS, lens.iter().copied().max().unwrap_or(0).max(MIN_KEPT_CHARS));
    if saving(lo) < excess {
        return lo;
    }
    // saving(keep) shrinks as keep grows: find the largest keep that still saves enough
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if saving(mid) >= excess { lo = mid } else { hi = mid - 1 }
    }
    lo
}

/// Where a cut can happen: a turn (index into the rendered turns) and one of its texts.
#[derive(Clone, Copy)]
enum Piece {
    Result(usize, usize),
    Text(usize),
    After(usize),
}

/// Last resort when the kept turns alone exceed the cap: cut the largest tool results in the middle, then the largest
/// user messages, keeping their head and tail. The system part is never cut. Returns the number of texts cut.
fn shrink(turns: &mut [Cow<'_, Turn>], excess: usize) -> usize {
    let mut cut = 0;
    let mut excess = excess;
    let len_of = |turns: &[Cow<'_, Turn>], p: Piece| match p {
        Piece::Result(i, j) => char_len(&turns[i].tool_results[j].content),
        Piece::Text(i) => char_len(&turns[i].text),
        Piece::After(i) => char_len(&turns[i].after),
    };
    let tool_results: Vec<Piece> =
        turns.iter().enumerate().flat_map(|(i, t)| (0..t.tool_results.len()).map(move |j| Piece::Result(i, j))).collect();
    let user_texts: Vec<Piece> =
        turns.iter().enumerate().filter(|(_, t)| t.role == "user").flat_map(|(i, _)| [Piece::Text(i), Piece::After(i)]).collect();
    for pieces in [tool_results, user_texts] {
        if excess == 0 {
            break;
        }
        let lens: Vec<usize> = pieces.iter().map(|&p| len_of(turns, p)).collect();
        let keep = keep_size(&lens, excess);
        for (&p, &len) in pieces.iter().zip(&lens) {
            let new_len = cut_len(len, keep);
            if new_len >= len {
                continue;
            }
            let t = turns[match p {
                Piece::Result(i, _) | Piece::Text(i) | Piece::After(i) => i,
            }]
            .to_mut();
            let text = match p {
                Piece::Result(_, j) => &mut t.tool_results[j].content,
                Piece::Text(_) => &mut t.text,
                Piece::After(_) => &mut t.after,
            };
            *text = cut_middle(text, keep);
            excess = excess.saturating_sub(len - new_len);
            cut += 1;
        }
    }
    cut
}

/// Layout: `<system>...</system>`, `<conversation>...</conversation>`, the last turn, and (with tools) the tail
/// reminder. Above `max_chars` the oldest history turns are dropped (never the system parts, the last turn or the last
/// KEEP_RECENT_TURNS history turns); if that is not enough, the largest tool results and then user messages of what
/// is left are cut in the middle.
pub fn render_prompt(req: &CanonicalRequest, max_chars: i64, tail_reminder: bool, tool_desc_max: i64) -> (String, PromptInfo) {
    let mut system_parts: Vec<String> = req.system.iter().filter(|s| !s.trim().is_empty()).cloned().collect();
    let tools_on = req.tools_on();
    if tools_on {
        let mut block = TOOL_PROTOCOL.to_string();
        for (i, t) in req.tools.iter().enumerate() {
            if i > 0 {
                block.push('\n');
            }
            block.push_str(&tool_line(t, tool_desc_max));
        }
        match &req.tool_choice {
            ToolChoice::Required => block.push_str(TOOL_CHOICE_REQUIRED),
            ToolChoice::Named(n) => block.push_str(&format!("\nIn this response you MUST call the tool `{n}` (and only that tool).")),
            _ => {}
        }
        if !req.parallel_tool_calls {
            block.push_str(ONE_CALL);
        }
        system_parts.push(block);
    }
    if let Some(schema) = &req.json_schema {
        let loose = schema == &json!({}) || schema == &json!({"type": "object"});
        let schema_line = if loose {
            "The value must be a JSON object.".to_string()
        } else {
            format!("The value must validate against this JSON Schema:\n{}", json::readable(schema))
        };
        system_parts.push(format!(
            "# Output format\nRespond with a single JSON value and nothing else: no code fences, no prose before or after.\n{schema_line}"
        ));
    }
    let mut turns: Vec<Cow<'_, Turn>> = req.turns.iter().map(|t| Cow::Borrowed(&**t)).collect();
    if turns.is_empty() {
        turns.push(Cow::Owned(Turn::new("user", "")));
    }
    if turns.last().is_some_and(|t| t.role == "assistant") {
        turns.push(Cow::Owned(Turn::new("user", ASSISTANT_PREFILL_NUDGE))); // CAVEAT: no assistant prefill: the model is asked to continue
    }
    let names = call_names(req);
    let system_block = if system_parts.is_empty() { None } else { Some(format!("<system>\n{}\n</system>", system_parts.join("\n\n"))) };
    let system_chars: usize = system_parts.iter().map(|s| char_len(s)).sum();
    let n_history = turns.len() - 1;
    let history: Vec<String> = turns[..n_history].iter().map(|t| render_turn(&names, t)).collect();
    let mut layout = Layout::new(system_block, history, render_turn(&names, &turns[n_history]), tools_on && tail_reminder);

    let max = usize::try_from(max_chars).unwrap_or(0);
    let original = layout.chars(0);
    let mut start = 0usize;
    while layout.chars(start) > max && n_history - start > KEEP_RECENT_TURNS {
        start += if n_history - start - KEEP_RECENT_TURNS >= 2 { 2 } else { 1 };
    }
    if start > 0 {
        tracing::warn!("prompt of {original} chars exceeded {max_chars}; dropped {start} old turns -> {} chars", layout.chars(start));
    }
    let mut shrunk = 0;
    if layout.chars(start) > max {
        let before = layout.chars(start);
        let kept = &mut turns[start..];
        shrunk = shrink(kept, before - max);
        if shrunk > 0 {
            let history: Vec<String> = turns[start..n_history].iter().map(|t| render_turn(&names, t)).collect();
            let mut full = vec![String::new(); start];
            full.extend(history);
            layout = Layout::new(layout.system.take(), full, render_turn(&names, &turns[n_history]), layout.reminder);
            tracing::warn!(
                "prompt of {before} chars still above {max_chars} after dropping old turns; cut {shrunk} tool result(s) or message(s) in the middle -> {} chars",
                layout.chars(start)
            );
        }
    }
    let chars = layout.chars(start);
    if chars > max {
        tracing::error!(
            "prompt of {chars} chars still above {max_chars} with system ({system_chars} chars) + last {} turns; sending anyway",
            (n_history - start).min(KEEP_RECENT_TURNS) + 1
        );
    }
    let prompt = layout.build(start);
    debug_assert_eq!(char_len(&prompt), chars);
    let info = PromptInfo {
        chars,
        dropped_turns: start,
        shrunk,
        system_chars,
        history_turns: n_history - start,
        tools: if tools_on { req.tools.len() } else { 0 },
    };
    (prompt, info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::ToolResult;

    /// The prompt as the first implementation built it: rebuilt after every drop (quadratic), kept as the reference.
    fn reference(req: &CanonicalRequest, max_chars: usize) -> String {
        let names = call_names(req);
        let turns: Vec<&Turn> = req.turns.iter().map(|t| &**t).collect();
        let last = turns[turns.len() - 1];
        let history: Vec<String> = turns[..turns.len() - 1].iter().map(|t| render_turn(&names, t)).collect();
        let build = |hist: &[String], dropped: usize| {
            let mut out = vec!["<system>\nS\n</system>".to_string()];
            if !hist.is_empty() || dropped > 0 {
                let mut inner = vec![];
                if dropped > 0 {
                    inner.push(omitted_notice(dropped));
                }
                inner.extend(hist.iter().cloned());
                out.push(format!("<conversation>\n{}\n</conversation>", inner.join("\n")));
            }
            out.push(render_turn(&names, last));
            out.join("\n")
        };
        let (mut start, mut prompt) = (0, build(&history, 0));
        while char_len(&prompt) > max_chars && history.len() - start > KEEP_RECENT_TURNS {
            start += if history.len() - start - KEEP_RECENT_TURNS >= 2 { 2 } else { 1 };
            prompt = build(&history[start..], start);
        }
        prompt
    }

    fn conversation(turns: usize, size: usize) -> CanonicalRequest {
        let mut req = CanonicalRequest { system: vec!["S".to_string()].into(), ..Default::default() };
        for i in 0..turns {
            req.add_text(if i % 2 == 0 { "user" } else { "assistant" }, &format!("t{i} {}", "ç".repeat(size + i % 7)));
        }
        req.add_text("user", "final question");
        req
    }

    #[test]
    fn linear_truncation_matches_the_reference() {
        for (turns, size, max) in [(3, 10, 50), (30, 100, 1000), (31, 100, 1500), (250, 40, 2000), (40, 50, 1_000_000), (12, 5, 0)] {
            let req = conversation(turns, size);
            let (prompt, info) = render_prompt(&req, max as i64, false, 0);
            assert_eq!(prompt, reference(&req, max), "{turns} turns of {size}, cap {max}");
            assert_eq!(info.chars, char_len(&prompt));
        }
    }

    #[test]
    fn truncation_keeps_the_system_part_and_recent_turns_and_says_so() {
        let req = conversation(30, 100);
        let (p, info) = render_prompt(&req, 1500, false, 0);
        assert!(info.dropped_turns > 0 && p.starts_with("<system>\nS\n</system>"));
        assert!(p.contains(&format!("[{} earlier turns were omitted", info.dropped_turns)));
        assert!(p.contains("t29 ") && p.ends_with("[user]: final question"));
    }

    #[test]
    fn an_oversized_last_turn_is_cut_in_the_middle() {
        let mut req = CanonicalRequest { system: vec!["S".to_string()].into(), ..Default::default() };
        req.add_text("user", "read the log");
        let content = format!("HEAD{}TAIL", "x".repeat(200_000));
        req.add("user", "", vec![], vec![ToolResult { call_id: "c1".into(), content, name: "read".into(), is_error: false }]);
        let (p, info) = render_prompt(&req, 50_000, false, 0);
        assert!(info.chars <= 50_000 && info.shrunk == 1, "{} chars", info.chars);
        assert!(p.starts_with("<system>\nS\n</system>") && p.contains("HEAD") && p.contains("TAIL\n</tool_result>"));
        assert!(p.contains("characters omitted by the gateway"));
        let (small, info) = render_prompt(&req, 1_000_000, false, 0);
        assert!(info.shrunk == 0 && small.contains(&"x".repeat(200_000)));
    }

    #[test]
    fn keep_size_is_the_largest_that_fits() {
        let lens = [100_000, 50_000, 3_000];
        let keep = keep_size(&lens, 60_000);
        let saving = |k: usize| lens.iter().map(|&l| l - cut_len(l, k).min(l)).sum::<usize>();
        assert!(saving(keep) >= 60_000 && saving(keep + 1) < 60_000);
        assert_eq!(keep_size(&lens, 10_000_000), MIN_KEPT_CHARS);
    }

    #[test]
    fn one_call_note_when_parallel_calls_are_off() {
        let tools: std::sync::Arc<[ToolSpec]> = vec![ToolSpec::new("f", "", None, false)].into();
        let mut req = CanonicalRequest { tools, parallel_tool_calls: false, ..Default::default() };
        req.add_text("user", "x");
        assert!(render_prompt(&req, 1_000_000, false, 0).0.contains("at most one tool"));
        req.parallel_tool_calls = true;
        assert!(!render_prompt(&req, 1_000_000, false, 0).0.contains("at most one tool"));
    }
}
