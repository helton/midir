//! Canonical request -> one text prompt for a text-only backend: the client's system prompt,
//! the tool protocol, the output format, the conversation and the last turn.

use serde_json::{json, Map, Value};

use crate::canonical::{CanonicalRequest, ToolChoice, ToolSpec, Turn};
use crate::py::json as pyjson;
use crate::py::text;

const LOG: &str = "midir.emulation.prompt";

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
- Announcing is not acting: if you tell the user you are about to do something that needs a tool, the <tool_call> block(s) MUST follow in the SAME response, right after the sentence. A response that only promises an action ("I will read the files...") and contains no <tool_call> is an error. When several independent calls are needed (e.g. reading several files), emit them all in one response instead of one per turn.
Tool results do not end the task: after reading them, keep working (call more tools) until the user's request is fully handled; stop to ask the user only when you truly need a decision from them.
Before you end a reply without a <tool_call>, check its last paragraph: if it is a plan, a list of next steps, or a promise about work not done yet ("I'll...", "Vou...", "Next...", "O plano é..."), do that work now with <tool_call> blocks instead of describing it. A step you have decided on is something to run, not to announce. End a reply without a tool call only when the request is fully handled, the question is answered, you need a decision only the user can make, or you are waiting for background work you already started (do not start it again). Never ask the user to confirm an action their request already asks for (for example a commit the task requests): do it.
Your capabilities are exactly the tools listed below. If a listed tool fetches web pages, searches, reads files or runs commands, then you DO have that access: never say you cannot access the internet, files or a terminal when a matching tool exists. When the user asks for current, external or verifiable information (a URL, a latest version, today's data), call the matching tool instead of answering from memory or declining. If the user names a page, site or repository without giving its URL, infer the most likely URL (e.g. the project's official site or GitHub repository) and fetch it.
Available tools (one JSON object per line: name, description, parameters as JSON Schema):
"#;
const TOOL_CHOICE_REQUIRED: &str = "\nIn this response you MUST call at least one tool; a plain-text answer is not acceptable.";
pub const ASSISTANT_PREFILL_NUDGE: &str = "(continue your previous message exactly from where it stopped, without repeating it)";
const KEEP_RECENT_TURNS: usize = 4;
const TAIL_REMINDER_TEXT: &str = "<reminder>Check your last paragraph before ending: if it announces, plans or promises an action, emit its <tool_call> block(s) now, in this reply; several independent calls go in one reply. Never claim you lack an ability a listed tool provides.</reminder>";

/// Copy of a JSON Schema with every nested "description" cut at `limit` chars.
fn truncate_descriptions(schema: &Value, limit: usize) -> Value {
    match schema {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                match v {
                    Value::String(s) if k == "description" && text::len(s) > limit => {
                        out.insert(k.clone(), json!(format!("{}…", text::rstrip(text::head(s, limit)))));
                    }
                    _ => {
                        out.insert(k.clone(), truncate_descriptions(v, limit));
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| truncate_descriptions(v, limit)).collect()),
        other => other.clone(),
    }
}

/// One tool per line as compact JSON without empty keys.
fn tool_line(t: &ToolSpec, desc_max: i64) -> String {
    let mut d = Map::new();
    d.insert("name".into(), json!(t.name));
    let mut description = t.description.clone();
    if desc_max != 0 && (text::len(&description) as i64) > desc_max {
        description = format!("{}…", text::rstrip(text::head(&description, desc_max.max(0) as usize)));
    }
    if !description.is_empty() {
        d.insert("description".into(), json!(description));
    }
    let params = &t.parameters;
    if text::truthy_opt(params.get("properties")) || text::truthy_opt(params.get("required")) {
        d.insert("parameters".into(), if desc_max != 0 { truncate_descriptions(params, desc_max.max(0) as usize) } else { params.clone() });
    }
    pyjson::dumps(&Value::Object(d), pyjson::COMPACT)
}

#[derive(Debug, Clone, Default)]
pub struct PromptInfo {
    pub chars: usize,
    pub dropped_turns: usize,
    pub system_chars: usize,
    pub history_turns: usize,
    pub tools: usize,
}

fn render_turn(req: &CanonicalRequest, t: &Turn) -> String {
    let mut parts: Vec<String> = vec![];
    if !t.text.is_empty() {
        parts.push(t.text.clone());
    }
    for c in &t.tool_calls {
        let args = match &c.arguments {
            Value::String(s) => s.clone(),
            other => pyjson::dumps(other, pyjson::DEFAULT),
        };
        parts.push(format!(
            "<tool_call id=\"{}\">\n{{\"name\": {}, \"arguments\": {args}}}\n</tool_call>",
            c.id,
            pyjson::dumps_str(&c.name, true)
        ));
    }
    for r in &t.tool_results {
        let err = if r.is_error { " is_error=\"true\"" } else { "" };
        let name = if r.name.is_empty() { req.tool_name_for(&r.call_id) } else { r.name.clone() };
        parts.push(format!("<tool_result id=\"{}\" name=\"{name}\"{err}>\n{}\n</tool_result>", r.call_id, r.content));
    }
    if !t.after.is_empty() {
        parts.push(t.after.clone());
    }
    format!("[{}]: {}", t.role, parts.join("\n"))
}

/// Layout: `<system>...</system>`, `<conversation>...</conversation>`, the last turn, and (with tools) the tail
/// reminder. Above `max_chars` the oldest history turns are dropped (never the system parts, the last turn or the last
/// KEEP_RECENT_TURNS history turns).
pub fn render_prompt(req: &CanonicalRequest, max_chars: i64, tail_reminder: bool, tool_desc_max: i64) -> (String, PromptInfo) {
    let mut system_parts: Vec<String> = req.system.iter().filter(|s| !text::is_blank(s)).cloned().collect();
    let tools_on = !req.tools.is_empty() && req.tool_choice != ToolChoice::None;
    if tools_on {
        let mut block = TOOL_PROTOCOL.to_string();
        block.push_str(&req.tools.iter().map(|t| tool_line(t, tool_desc_max)).collect::<Vec<_>>().join("\n"));
        match &req.tool_choice {
            ToolChoice::Required => block.push_str(TOOL_CHOICE_REQUIRED),
            ToolChoice::Named(n) => {
                block.push_str(&format!("\nIn this response you MUST call the tool `{}` (and only that tool).", text::str_of(n)))
            }
            _ => {}
        }
        system_parts.push(block);
    }
    if let Some(schema) = &req.json_schema {
        let loose = text::eq(schema, &json!({})) || text::eq(schema, &json!({"type": "object"}));
        let schema_line = if loose {
            "The value must be a JSON object.".to_string()
        } else {
            format!("The value must validate against this JSON Schema:\n{}", pyjson::dumps(schema, pyjson::DEFAULT))
        };
        system_parts.push(format!(
            "# Output format\nRespond with a single JSON value and nothing else: no code fences, no prose before or after.\n{schema_line}"
        ));
    }
    let mut turns: Vec<&Turn> = req.turns.iter().collect();
    let empty = Turn::new("user", "");
    let nudge = Turn::new("user", ASSISTANT_PREFILL_NUDGE);
    if turns.is_empty() {
        turns.push(&empty);
    }
    if turns.last().map_or(false, |t| t.role == "assistant") {
        turns.push(&nudge); // CAVEAT: assistant prefill has no equivalent; the model is asked to continue
    }
    let last = turns[turns.len() - 1];
    let history: Vec<String> = turns[..turns.len() - 1].iter().map(|t| render_turn(req, t)).collect();
    let rendered_last = render_turn(req, last);
    let system_block = if system_parts.is_empty() { None } else { Some(format!("<system>\n{}\n</system>", system_parts.join("\n\n"))) };

    let build = |hist: &[String], dropped: usize| -> String {
        let mut out: Vec<String> = vec![];
        if let Some(s) = &system_block {
            out.push(s.clone());
        }
        if !hist.is_empty() || dropped > 0 {
            let mut inner: Vec<String> = vec![];
            if dropped > 0 {
                inner.push(format!(
                    "[{dropped} earlier turns were omitted: the conversation exceeded the size limit. Do not assume their content; ask or re-read files if you need it.]"
                ));
            }
            inner.extend(hist.iter().cloned());
            out.push(format!("<conversation>\n{}\n</conversation>", inner.join("\n")));
        }
        out.push(rendered_last.clone());
        if tools_on && tail_reminder {
            out.push(TAIL_REMINDER_TEXT.to_string());
        }
        out.join("\n")
    };

    let mut start = 0usize;
    let mut prompt = build(&history, 0);
    let mut prompt_len = text::len(&prompt);
    let original = prompt_len;
    let mut dropped = 0usize;
    while (prompt_len as i64) > max_chars && history.len() - start > KEEP_RECENT_TURNS {
        let n = if history.len() - start - KEEP_RECENT_TURNS >= 2 { 2 } else { 1 };
        start += n;
        dropped += n;
        prompt = build(&history[start..], dropped);
        prompt_len = text::len(&prompt);
    }
    let system_chars: usize = system_parts.iter().map(|s| text::len(s)).sum();
    if dropped > 0 {
        crate::warn!(LOG, "prompt of {original} chars exceeded {max_chars}; dropped {dropped} old turns -> {prompt_len} chars");
    }
    let history_turns = history.len() - start;
    if (prompt_len as i64) > max_chars {
        crate::error!(
            LOG,
            "prompt of {prompt_len} chars still above {max_chars} with system ({system_chars} chars) + last {} turns; sending anyway",
            history_turns.min(KEEP_RECENT_TURNS) + 1
        );
    }
    let info = PromptInfo {
        chars: prompt_len,
        dropped_turns: dropped,
        system_chars,
        history_turns,
        tools: if tools_on { req.tools.len() } else { 0 },
    };
    (prompt, info)
}
