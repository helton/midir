"""Canonical request -> one text prompt for a text-only backend: the client's system prompt, the tool protocol (how to
write <tool_call> blocks), the output format, the conversation and the last turn."""
from __future__ import annotations

import json
import logging
from typing import Any

from midir.canonical import CanonicalRequest, ToolSpec, Turn

log = logging.getLogger(__name__)

TOOL_PROTOCOL = """# Tools
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
Tool results do not end the task: after reading them, keep working (call more tools) until the user's request is fully handled; stop to ask the user only when you truly need a decision from them.\nBefore you end a reply without a <tool_call>, check its last paragraph: if it is a plan, a list of next steps, or a promise about work not done yet (\"I'll...\", \"Vou...\", \"Next...\", \"O plano é...\"), do that work now with <tool_call> blocks instead of describing it. A step you have decided on is something to run, not to announce. End a reply without a tool call only when the request is fully handled, the question is answered, you need a decision only the user can make, or you are waiting for background work you already started (do not start it again). Never ask the user to confirm an action their request already asks for (for example a commit the task requests): do it.
Your capabilities are exactly the tools listed below. If a listed tool fetches web pages, searches, reads files or runs commands, then you DO have that access: never say you cannot access the internet, files or a terminal when a matching tool exists. When the user asks for current, external or verifiable information (a URL, a latest version, today's data), call the matching tool instead of answering from memory or declining. If the user names a page, site or repository without giving its URL, infer the most likely URL (e.g. the project's official site or GitHub repository) and fetch it.
Available tools (one JSON object per line: name, description, parameters as JSON Schema):
"""
TOOL_CHOICE_REQUIRED = "\nIn this response you MUST call at least one tool; a plain-text answer is not acceptable."
TOOL_CHOICE_NAMED = "\nIn this response you MUST call the tool `{name}` (and only that tool)."
JSON_MODE = "# Output format\nRespond with a single JSON value and nothing else: no code fences, no prose before or after.\n{schema_line}"
ASSISTANT_PREFILL_NUDGE = "(continue your previous message exactly from where it stopped, without repeating it)"
TRUNCATION_NOTE = "[{n} earlier turns were omitted: the conversation exceeded the size limit. Do not assume their content; ask or re-read files if you need it.]"
KEEP_RECENT_TURNS = 4  # never drop the last N history turns, even when the prompt exceeds the cap
TAIL_REMINDER_TEXT = "<reminder>Check your last paragraph before ending: if it announces, plans or promises an action, emit its <tool_call> block(s) now, in this reply; several independent calls go in one reply. Never claim you lack an ability a listed tool provides.</reminder>"


def _truncate_descriptions(schema: Any, limit: int) -> Any:
    """Copy of a JSON Schema with every nested "description" cut at `limit` chars (experiment knob)."""
    if isinstance(schema, dict):
        out = {}
        for k, v in schema.items():
            if k == "description" and isinstance(v, str) and len(v) > limit:
                out[k] = v[:limit].rstrip() + "…"
            else:
                out[k] = _truncate_descriptions(v, limit)
        return out
    if isinstance(schema, list):
        return [_truncate_descriptions(v, limit) for v in schema]
    return schema


def _tool_line(t: ToolSpec, desc_max: int = 0) -> str:
    """One tool per line as compact JSON without empty keys (Copilot's 74 tools: ~125k -> ~90k chars)."""
    d: dict = {"name": t.name}
    description = t.description
    if desc_max and len(description) > desc_max:
        description = description[:desc_max].rstrip() + "…"
    if description:
        d["description"] = description
    params = t.parameters or {}
    if params.get("properties") or params.get("required"):
        d["parameters"] = _truncate_descriptions(params, desc_max) if desc_max else params
    return json.dumps(d, ensure_ascii=False, separators=(",", ":"))


def render_prompt(req: CanonicalRequest, max_chars: int, *, tail_reminder: bool = True, tool_desc_max: int = 0) -> tuple[str, dict]:
    """
    Layout:
        <system>client system prompt(s) + tool protocol + output format</system>
        <conversation>[user]: ... [assistant]: ... <tool_call>...</tool_call> [user]: <tool_result>...</tool_result></conversation>
        [user]: last turn
    Above max_chars the oldest history turns are dropped; never the system parts, the last turn, or the last
    KEEP_RECENT_TURNS history turns. `tail_reminder` adds a one-line tool-protocol reminder after the last turn;
    `tool_desc_max` truncates tool descriptions (0 = off).
    """
    system_parts = [s for s in req.system if s and s.strip()]
    tools_on = bool(req.tools) and req.tool_choice != "none"
    desc_max, tail = tool_desc_max, tail_reminder
    if tools_on:
        block = TOOL_PROTOCOL + "\n".join(_tool_line(t, desc_max) for t in req.tools)
        if req.tool_choice == "required":
            block += TOOL_CHOICE_REQUIRED
        elif isinstance(req.tool_choice, dict):
            block += TOOL_CHOICE_NAMED.format(name=req.tool_choice.get("name", ""))
        system_parts.append(block)
    if req.json_schema is not None:
        loose = req.json_schema in ({}, {"type": "object"})
        schema_line = "The value must be a JSON object." if loose else "The value must validate against this JSON Schema:\n" + json.dumps(req.json_schema, ensure_ascii=False)
        system_parts.append(JSON_MODE.format(schema_line=schema_line))

    def render_turn(t: Turn) -> str:
        parts = [t.text] if t.text else []
        for c in t.tool_calls:
            args = c.arguments if isinstance(c.arguments, str) else json.dumps(c.arguments, ensure_ascii=False)
            parts.append(f'<tool_call id="{c.id}">\n{{"name": {json.dumps(c.name)}, "arguments": {args}}}\n</tool_call>')
        for r in t.tool_results:
            err = ' is_error="true"' if r.is_error else ""
            parts.append(f'<tool_result id="{r.call_id}" name="{r.name or req.tool_name_for(r.call_id)}"{err}>\n{r.content}\n</tool_result>')
        if t.after:
            parts.append(t.after)
        return f"[{t.role}]: " + "\n".join(parts)

    turns = list(req.turns) or [Turn("user", "")]
    if turns[-1].role == "assistant":
        turns.append(Turn("user", ASSISTANT_PREFILL_NUDGE))  # CAVEAT: assistant prefill has no equivalent; the model is asked to continue
    last, history = turns[-1], turns[:-1]

    def build(hist: list[Turn], dropped: int = 0) -> str:
        out: list[str] = []
        if system_parts:
            out.append("<system>\n" + "\n\n".join(system_parts) + "\n</system>")
        if hist or dropped:
            note = [TRUNCATION_NOTE.format(n=dropped)] if dropped else []
            out.append("<conversation>\n" + "\n".join(note + [render_turn(t) for t in hist]) + "\n</conversation>")
        out.append(render_turn(last))
        if tools_on and tail:
            out.append(TAIL_REMINDER_TEXT)
        return "\n".join(out)

    prompt = build(history)
    original, dropped = len(prompt), 0
    while len(prompt) > max_chars and len(history) > KEEP_RECENT_TURNS:
        n = 2 if len(history) - KEEP_RECENT_TURNS >= 2 else 1
        history, dropped = history[n:], dropped + n
        prompt = build(history, dropped)
    system_chars = sum(map(len, system_parts))
    if dropped:
        log.warning("prompt of %d chars exceeded %d; dropped %d old turns -> %d chars", original, max_chars, dropped, len(prompt))
    if len(prompt) > max_chars:
        log.error("prompt of %d chars still above %d with system (%d chars) + last %d turns; sending anyway", len(prompt), max_chars, system_chars, min(len(history), KEEP_RECENT_TURNS) + 1)
    info = {"chars": len(prompt), "original_chars": original, "dropped_turns": dropped, "system_chars": system_chars, "history_turns": len(history), "tools": len(req.tools) if tools_on else 0}
    return prompt, info
