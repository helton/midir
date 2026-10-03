"""Anthropic Messages <-> canonical (`POST /v1/messages`, `POST /v1/messages/count_tokens`)."""
from __future__ import annotations

import json
import uuid
from typing import Any, AsyncIterator

from midir.canonical import CUSTOM_TOOL_PARAMS, CanonicalRequest, CanonicalResponse, Event, ToolCall, ToolResult, ToolSpec, Turn, new_call_id
from midir.errors import ClientError
from midir.protocols.common import arguments_str, ignored_params, parse_arguments, text_of, tool_params, MEDIA_TYPES


class Messages:
    """Anthropic Messages <-> canonical."""

    IGNORED = ("temperature", "top_p", "top_k", "metadata", "thinking", "service_tier", "cache_control", "container", "mcp_servers", "context_management")
    SKIPPED_BLOCKS = {"thinking", "redacted_thinking", "server_tool_use", "web_search_tool_result"}

    @classmethod
    def to_canonical(cls, body: dict) -> CanonicalRequest:
        messages = body.get("messages")
        if not isinstance(messages, list) or not messages:
            raise ClientError("'messages' is required and must be a non-empty list", "invalid_request_error")
        req = CanonicalRequest(ignored=ignored_params(body, cls.IGNORED))
        system = body.get("system")
        if system:
            req.system.append(system if isinstance(system, str) else "\n".join(text_of([b], "system") for b in system))
        for m in messages:
            role = "assistant" if m.get("role") == "assistant" else "user"
            content = m.get("content")
            if isinstance(content, str):
                req.add(role, content)
                continue
            texts: list[str] = []
            after: list[str] = []  # text blocks that follow a tool_result (Claude Code's reminders, mid-turn user notes)
            calls: list[ToolCall] = []
            results: list[ToolResult] = []
            for b in content or []:
                t = b.get("type")
                if t == "text":
                    (after if results else texts).append(str(b.get("text", "")))
                elif t == "tool_use":
                    calls.append(ToolCall(b.get("id") or ("toolu_" + uuid.uuid4().hex[:24]), b.get("name", ""), b.get("input") if b.get("input") is not None else {}))
                elif t == "tool_result":
                    c = b.get("content")
                    txt = c if isinstance(c, str) else "\n".join(text_of([p], "tool_result") for p in (c or []))
                    results.append(ToolResult(b.get("tool_use_id", ""), txt, is_error=bool(b.get("is_error"))))
                elif t in MEDIA_TYPES:
                    texts.append(text_of([b], role))
                elif t in cls.SKIPPED_BLOCKS:
                    continue
                else:
                    texts.append(json.dumps(b, ensure_ascii=False))
            req.add(role, "\n".join(texts), tool_calls=calls, tool_results=results)
            if after:
                req.add(role, "\n".join(after))
        for t in body.get("tools") or []:
            typ = t.get("type") or "custom"
            if not t.get("input_schema") and typ != "custom":
                req.ignored.append(f"tool:{typ}")  # CAVEAT: Anthropic server tool (web_search_2025..., text_editor...) omitted with a warning
                continue
            req.tools.append(ToolSpec(t.get("name", ""), t.get("description") or "", tool_params(t.get("input_schema"))))
        choice = body.get("tool_choice") or {}
        typ = choice.get("type", "auto") if isinstance(choice, dict) else "auto"
        named = {"name": choice["name"]} if typ == "tool" and isinstance(choice, dict) and choice.get("name") else "required"  # "tool" without a name: any tool
        req.tool_choice = {"auto": "auto", "any": "required", "none": "none"}.get(typ, named if typ == "tool" else "auto")
        stops = body.get("stop_sequences")
        req.stop = [stops] if isinstance(stops, str) else [s for s in (stops or []) if isinstance(s, str)]
        req.max_tokens = body.get("max_tokens")
        fmt = (body.get("output_config") or {}).get("format") or {}
        if fmt.get("type") == "json_schema":  # CAVEAT: Anthropic structured outputs (beta) only via json_schema
            req.json_schema = fmt.get("schema") or {"type": "object"}
        return req

    @staticmethod
    def stop_reason(r: CanonicalResponse) -> str:
        return {"tool_calls": "tool_use", "length": "max_tokens", "stop_sequence": "stop_sequence"}.get(r.finish, "end_turn")

    @staticmethod
    def usage(u: dict) -> dict:
        return {"input_tokens": u["prompt_tokens"], "output_tokens": u["completion_tokens"], "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}

    @staticmethod
    def tool_use_id(call_id: str) -> str:
        return call_id if call_id.startswith("toolu_") else "toolu_" + call_id.removeprefix("call_")

    @staticmethod
    def tool_input(c: ToolCall) -> dict:
        return c.arguments if isinstance(c.arguments, dict) else {"_raw": c.arguments}  # CAVEAT: invalid JSON arrives as _raw

    @classmethod
    def content_blocks(cls, r: CanonicalResponse) -> list[dict]:
        blocks: list[dict] = [{"type": "text", "text": r.text}] if (r.text or not r.tool_calls) else []
        return blocks + [{"type": "tool_use", "id": cls.tool_use_id(c.id), "name": c.name, "input": cls.tool_input(c)} for c in r.tool_calls]

    @classmethod
    def response(cls, r: CanonicalResponse, mid: str, model: str) -> dict:
        return {"id": mid, "type": "message", "role": "assistant", "model": model, "content": cls.content_blocks(r), "stop_reason": cls.stop_reason(r), "stop_sequence": r.stop_sequence, "usage": cls.usage(r.usage)}

    @classmethod
    async def stream(cls, events: AsyncIterator[Event], mid: str, model: str) -> AsyncIterator[str]:
        def ev(name: str, data: dict) -> str:
            return f"event: {name}\ndata: {json.dumps({'type': name, **data}, ensure_ascii=False)}\n\n"

        yield ev("message_start", {"message": {"id": mid, "type": "message", "role": "assistant", "model": model, "content": [], "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}})
        yield ev("ping", {})
        index, text_open = 0, False
        final: CanonicalResponse | None = None
        async for e in events:
            if e.kind == "text":
                if not text_open:
                    yield ev("content_block_start", {"index": index, "content_block": {"type": "text", "text": ""}})
                    text_open = True
                yield ev("content_block_delta", {"index": index, "delta": {"type": "text_delta", "text": e.text}})
            elif e.kind == "tool_call" and e.call:
                if text_open:
                    yield ev("content_block_stop", {"index": index})
                    text_open, index = False, index + 1
                yield ev("content_block_start", {"index": index, "content_block": {"type": "tool_use", "id": cls.tool_use_id(e.call.id), "name": e.call.name, "input": {}}})
                yield ev("content_block_delta", {"index": index, "delta": {"type": "input_json_delta", "partial_json": json.dumps(cls.tool_input(e.call), ensure_ascii=False)}})
                yield ev("content_block_stop", {"index": index})
                index += 1
            elif e.kind == "done":
                final = e.response
        if text_open:
            yield ev("content_block_stop", {"index": index})
        r = final or CanonicalResponse()
        yield ev("message_delta", {"delta": {"stop_reason": cls.stop_reason(r), "stop_sequence": r.stop_sequence}, "usage": {"output_tokens": r.usage["completion_tokens"], "input_tokens": r.usage["prompt_tokens"]}})
        yield ev("message_stop", {})
