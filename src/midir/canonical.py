"""Protocol-neutral request and response. Every protocol adapter translates to and from these types, and every
backend works on them, so neither side knows about the other."""
from __future__ import annotations

import uuid
from dataclasses import dataclass, field
from typing import Any

CHARS_PER_TOKEN = 4.0  # CAVEAT: heuristic for count_tokens, max_tokens and missing usage; measured ~4.9 for prose, ~3.5 for code
CUSTOM_TOOL_PARAMS = {"type": "object", "properties": {"input": {"type": "string", "description": "The raw text input for this tool (free-form, exactly as the tool expects it)"}}, "required": ["input"]}


def new_call_id() -> str:
    return "call_" + uuid.uuid4().hex[:24]


def estimate_tokens(text: str) -> int:
    return max(1, int(len(text) / CHARS_PER_TOKEN))


def empty_usage() -> dict:
    return {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}


def sum_usage(a: dict, b: dict) -> dict:
    return {k: a.get(k, 0) + b.get(k, 0) for k in ("prompt_tokens", "completion_tokens", "total_tokens")}


@dataclass
class ToolSpec:
    name: str
    description: str = ""
    parameters: dict = field(default_factory=lambda: {"type": "object", "properties": {}})
    custom: bool = False  # Responses "custom" tool: free-form text input, emulated as {"input": "<text>"}

    def __post_init__(self) -> None:  # clients send null descriptions and parameters; keep the types the prompt code expects
        self.name, self.description = str(self.name or ""), str(self.description or "")
        if not isinstance(self.parameters, dict):
            self.parameters = {"type": "object", "properties": {}}


@dataclass
class ToolCall:
    id: str
    name: str
    arguments: Any  # a dict; a raw str only when a client sent invalid JSON in its history (CAVEAT)


@dataclass
class ToolResult:
    call_id: str
    content: str
    name: str = ""
    is_error: bool = False


@dataclass
class Turn:
    role: str  # user | assistant
    text: str = ""
    tool_calls: list[ToolCall] = field(default_factory=list)
    tool_results: list[ToolResult] = field(default_factory=list)
    after: str = ""  # text that came after the tool results (OpenClaw's runtime context, Claude Code's reminders)

    def copy(self) -> "Turn":
        return Turn(self.role, self.text, list(self.tool_calls), list(self.tool_results), self.after)


@dataclass
class CanonicalRequest:
    system: list[str] = field(default_factory=list)
    turns: list[Turn] = field(default_factory=list)
    tools: list[ToolSpec] = field(default_factory=list)
    tool_choice: Any = "auto"  # auto | none | required | {"name": ...}
    json_schema: dict | None = None  # {"type": "object"} alone means "any JSON object"
    stop: list[str] = field(default_factory=list)
    max_tokens: int | None = None
    ignored: list[str] = field(default_factory=list)  # accepted parameters without effect (for the log)
    route: Any = None  # the resolved midir.config.ModelSpec (backend, target, per-model knobs); None = defaults
    meta: dict = field(default_factory=dict)  # telemetry labels and counters for this request (shared with follow-up requests)

    def add(self, role: str, text: str = "", tool_calls: list[ToolCall] | None = None, tool_results: list[ToolResult] | None = None) -> None:
        """Append content, merging with the previous turn when the role repeats (several `tool` messages -> one user turn).
        An assistant message with no text and no calls adds nothing (clients send them, e.g. Hermes after an aborted turn)."""
        if role == "assistant" and not (text and text.strip()) and not tool_calls and not tool_results:
            return
        if self.turns and self.turns[-1].role == role:
            t = self.turns[-1]
            if text and (t.tool_results or t.after):  # keep the order: results first, then what was said after them
                t.after = f"{t.after}\n{text}" if t.after else text
            elif text:
                t.text = f"{t.text}\n{text}" if t.text else text
            t.tool_calls += tool_calls or []
            t.tool_results += tool_results or []
        else:
            self.turns.append(Turn(role, text, list(tool_calls or []), list(tool_results or [])))

    def tool_name_for(self, call_id: str) -> str:
        for t in self.turns:
            for c in t.tool_calls:
                if c.id == call_id:
                    return c.name
        return ""

    def custom_tool_names(self) -> set[str]:
        return {t.name for t in self.tools if t.custom}

    def derive(self, *, tools: list[ToolSpec] | None = None, tool_choice: Any = None, json_schema: dict | None = None, max_tokens: int | None = None) -> "CanonicalRequest":
        """Copy for a follow-up call (retry, repair), keeping system and turns."""
        turns = [t.copy() for t in self.turns]
        return CanonicalRequest(list(self.system), turns, self.tools if tools is None else tools, self.tool_choice if tool_choice is None else tool_choice,
                                json_schema, list(self.stop), max_tokens, route=self.route, meta=self.meta)


@dataclass
class CanonicalResponse:
    text: str = ""
    tool_calls: list[ToolCall] = field(default_factory=list)
    finish: str = "stop"  # stop | tool_calls | length | stop_sequence
    stop_sequence: str | None = None
    usage: dict = field(default_factory=empty_usage)
    message_id: str | None = None
    rejected_calls: list[str] = field(default_factory=list)  # raw <tool_call> content dropped as invalid JSON


@dataclass
class Event:
    """One item of a response stream: text, a complete tool call, or the end (with the whole response)."""

    kind: str  # text | tool_call | done
    text: str = ""
    call: ToolCall | None = None
    response: CanonicalResponse | None = None
