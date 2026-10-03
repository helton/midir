"""OpenAI Chat Completions <-> canonical (`POST /v1/chat/completions`)."""
from __future__ import annotations

import json
import uuid
from typing import Any, AsyncIterator

from midir.canonical import CUSTOM_TOOL_PARAMS, CanonicalRequest, CanonicalResponse, Event, ToolCall, ToolResult, ToolSpec, Turn, new_call_id
from midir.errors import ClientError
from midir.protocols.common import arguments_str, ignored_params, parse_arguments, text_of, tool_params, MEDIA_TYPES


class ChatCompletions:
    """OpenAI Chat Completions <-> canonical."""

    IGNORED = ("temperature", "top_p", "seed", "presence_penalty", "frequency_penalty", "logit_bias", "reasoning_effort", "parallel_tool_calls", "service_tier", "store", "metadata", "prediction", "audio", "modalities", "verbosity")

    @classmethod
    def to_canonical(cls, body: dict) -> CanonicalRequest:
        if body.get("logprobs") or body.get("top_logprobs"):
            raise ClientError("logprobs are not available from StackSpot", "unsupported_parameter")
        if body.get("n", 1) not in (1, None):
            raise ClientError("n > 1 is not supported (one generation per request)", "unsupported_parameter")
        messages = body.get("messages")
        if not isinstance(messages, list) or not messages:
            raise ClientError("'messages' is required and must be a non-empty list", "missing_messages")
        req = CanonicalRequest(ignored=ignored_params(body, cls.IGNORED))
        for m in messages:
            role = m.get("role", "user")
            if role in ("system", "developer"):
                req.system.append(text_of(m.get("content"), "system"))
            elif role == "assistant":
                calls = [ToolCall(tc.get("id") or new_call_id(), (tc.get("function") or {}).get("name", ""), parse_arguments((tc.get("function") or {}).get("arguments"))) for tc in (m.get("tool_calls") or [])]
                if m.get("function_call"):  # legacy
                    calls.append(ToolCall(new_call_id(), m["function_call"].get("name", ""), parse_arguments(m["function_call"].get("arguments"))))
                req.add("assistant", text_of(m.get("content"), "assistant"), tool_calls=calls)
            elif role in ("tool", "function"):
                req.add("user", tool_results=[ToolResult(m.get("tool_call_id") or m.get("name", ""), text_of(m.get("content"), "tool"), m.get("name", ""))])
            else:
                req.add("user", text_of(m.get("content"), role))
        for t in body.get("tools") or []:
            if t.get("type", "function") != "function":
                req.ignored.append(f"tool:{t.get('type')}")  # CAVEAT: built-in tool omitted with a warning
                continue
            f = t.get("function") or {}
            req.tools.append(ToolSpec(f.get("name", ""), f.get("description"), tool_params(f.get("parameters"))))
        for f in body.get("functions") or []:  # legacy
            req.tools.append(ToolSpec(f.get("name", ""), f.get("description", ""), tool_params(f.get("parameters"))))
        choice = body.get("tool_choice", body.get("function_call", "auto"))
        if isinstance(choice, dict):
            name = (choice.get("function") or {}).get("name") or choice.get("name")
            req.tool_choice = {"name": name} if name else "auto"
        elif choice in ("none", "required", "auto"):
            req.tool_choice = choice
        fmt = body.get("response_format")
        if isinstance(fmt, dict):
            if fmt.get("type") == "json_object":
                req.json_schema = {"type": "object"}
            elif fmt.get("type") == "json_schema":
                req.json_schema = (fmt.get("json_schema") or {}).get("schema") or {"type": "object"}
        stop = body.get("stop")
        req.stop = [stop] if isinstance(stop, str) else [s for s in (stop or []) if isinstance(s, str)]
        req.max_tokens = body.get("max_completion_tokens") or body.get("max_tokens")
        return req

    @staticmethod
    def finish_reason(r: CanonicalResponse) -> str:
        return {"tool_calls": "tool_calls", "length": "length"}.get(r.finish, "stop")

    @staticmethod
    def usage(u: dict) -> dict:
        """Same shape streaming (last chunk) and not."""
        return {**u, "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 0}}

    @staticmethod
    def _tool_call(c: ToolCall, index: int | None = None) -> dict:
        d: dict = {"id": c.id, "type": "function", "function": {"name": c.name, "arguments": arguments_str(c.arguments)}}
        if index is not None:
            d["index"] = index
        return d

    @classmethod
    def response(cls, r: CanonicalResponse, cid: str, created: int, model: str) -> dict:
        message: dict = {"role": "assistant", "content": r.text if (r.text or not r.tool_calls) else None, "refusal": None}
        if r.tool_calls:
            message["tool_calls"] = [cls._tool_call(c) for c in r.tool_calls]
        return {"id": cid, "object": "chat.completion", "created": created, "model": model, "choices": [{"index": 0, "message": message, "finish_reason": cls.finish_reason(r), "logprobs": None}], "usage": cls.usage(r.usage), "system_fingerprint": r.message_id}

    @classmethod
    async def stream(cls, events: AsyncIterator[Event], cid: str, created: int, model: str, include_usage: bool) -> AsyncIterator[str]:
        def chunk(delta: dict, finish: str | None = None, usage: dict | None = None) -> str:
            d: dict = {"id": cid, "object": "chat.completion.chunk", "created": created, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish, "logprobs": None}]}
            if usage is not None:
                d["usage"] = usage
            return f"data: {json.dumps(d, ensure_ascii=False)}\n\n"

        yield chunk({"role": "assistant", "content": ""})
        n_calls = 0
        async for ev in events:
            if ev.kind == "text":
                yield chunk({"content": ev.text})
            elif ev.kind == "tool_call" and ev.call:
                yield chunk({"tool_calls": [cls._tool_call(ev.call, n_calls)]})
                n_calls += 1
            elif ev.kind == "keepalive":
                yield ": keepalive\n\n"  # SSE comment: ignored by parsers, keeps idle-timeout proxies and clients waiting
            elif ev.kind == "done" and ev.response:
                yield chunk({}, finish=cls.finish_reason(ev.response), usage=cls.usage(ev.response.usage) if include_usage else None)
        yield "data: [DONE]\n\n"
