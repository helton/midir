"""OpenAI Responses <-> canonical (`POST /v1/responses`, `GET /v1/responses/{id}`)."""
from __future__ import annotations

import json
import uuid
from typing import Any, AsyncIterator

from midir.canonical import CUSTOM_TOOL_PARAMS, CanonicalRequest, CanonicalResponse, Event, ToolCall, ToolResult, ToolSpec, Turn, new_call_id
from midir.errors import ClientError
from midir.protocols.common import arguments_str, ignored_params, parse_arguments, text_of, tool_params, MEDIA_TYPES
from midir.store import ResponseStore


class Responses:
    """OpenAI Responses <-> canonical. previous_response_id chains come from a ResponseStore (midir.store)."""

    IGNORED = ("temperature", "top_p", "reasoning", "service_tier", "metadata", "parallel_tool_calls", "truncation", "user", "include", "prompt_cache_key", "safety_identifier", "background")

    # ---- request ----
    @classmethod
    def to_canonical(cls, body: dict, store: ResponseStore | None = None) -> CanonicalRequest:
        req = CanonicalRequest(ignored=ignored_params(body, cls.IGNORED))
        prev = body.get("previous_response_id")
        if prev:
            stored = store.load(prev) if store else None
            if not stored:
                where = store.describe() if store else "not stored"
                raise ClientError(f"previous_response_id '{prev}' is unknown (responses are {where})", "previous_response_not_found", 404)
            _, prev_req, prev_resp = stored
            req.turns = [t.copy() for t in prev_req.turns]
            req.add("assistant", prev_resp.text, tool_calls=list(prev_resp.tool_calls))
            req.meta.update(prev_id=prev, prev_turns=len(req.turns))
            # instructions and tools are not inherited: the current request's values win; reuse the previous only when absent
            if not body.get("instructions"):
                req.system = list(prev_req.system)
            if body.get("tools") is None:
                req.tools = list(prev_req.tools)
        if body.get("instructions"):
            req.system.append(text_of(body["instructions"], "instructions"))
        raw_input = body.get("input", "")
        items = [{"role": "user", "content": raw_input}] if isinstance(raw_input, str) else (raw_input or [])
        for it in items:
            if not isinstance(it, dict):
                continue
            t = it.get("type", "message")
            if t == "message" or (t is None and "role" in it):
                role = it.get("role", "user")
                if role in ("system", "developer"):
                    req.system.append(text_of(it.get("content"), role))
                else:
                    req.add("assistant" if role == "assistant" else "user", text_of(it.get("content"), role))
            elif t == "function_call":
                req.add("assistant", tool_calls=[ToolCall(it.get("call_id") or it.get("id") or new_call_id(), it.get("name", ""), parse_arguments(it.get("arguments")))])
            elif t == "custom_tool_call":
                req.add("assistant", tool_calls=[ToolCall(it.get("call_id") or it.get("id") or new_call_id(), it.get("name", ""), {"input": it.get("input", "")})])
            elif t in ("function_call_output", "custom_tool_call_output"):
                req.add("user", tool_results=[ToolResult(it.get("call_id", ""), text_of(it.get("output"), t))])
            elif t in ("reasoning", "item_reference"):
                continue  # no equivalent; ignored
            else:
                raise ClientError(f"input item '{t}' is not supported", "unsupported_input")

        def add_tool(t: dict) -> None:
            typ = t.get("type", "function")
            if typ == "function":
                req.tools.append(ToolSpec(t.get("name", ""), t.get("description") or "", tool_params(t.get("parameters"))))
            elif typ == "custom":
                # CAVEAT: free-form text tool (e.g. Codex apply_patch); its grammar/format is not enforced
                description = ((t.get("description") or "") + ' Free-form text tool: put the entire raw input in the single string argument "input".').strip()
                req.tools.append(ToolSpec(t.get("name", ""), description, dict(CUSTOM_TOOL_PARAMS), custom=True))
            elif typ == "namespace":
                for inner in t.get("tools") or []:
                    add_tool(inner)
            else:
                req.ignored.append(f"tool:{typ}")  # CAVEAT: built-in tool (web_search, file_search, ...) omitted with a warning

        for t in body.get("tools") or []:
            add_tool(t)
        choice = body.get("tool_choice", "auto")
        if isinstance(choice, dict):
            req.tool_choice = {"name": choice.get("name")} if choice.get("name") else "auto"
        elif choice in ("none", "required", "auto"):
            req.tool_choice = choice
        fmt = ((body.get("text") or {}).get("format")) or {}
        if fmt.get("type") == "json_object":
            req.json_schema = {"type": "object"}
        elif fmt.get("type") == "json_schema":
            req.json_schema = fmt.get("schema") or {"type": "object"}
        req.max_tokens = body.get("max_output_tokens")
        return req

    # ---- response ----
    @staticmethod
    def usage(u: dict) -> dict:
        return {"input_tokens": u["prompt_tokens"], "output_tokens": u["completion_tokens"], "total_tokens": u["total_tokens"], "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}

    @staticmethod
    def call_item(c: ToolCall, custom_names: set[str], status: str = "completed") -> dict:
        if c.name in custom_names:
            raw = c.arguments.get("input", "") if isinstance(c.arguments, dict) else str(c.arguments)
            return {"type": "custom_tool_call", "id": "ctc_" + c.id[5:], "call_id": c.id, "name": c.name, "input": raw, "status": status}
        return {"type": "function_call", "id": "fc_" + c.id[5:], "call_id": c.id, "name": c.name, "arguments": arguments_str(c.arguments), "status": status}

    @staticmethod
    def message_item(msg_id: str, text: str, status: str = "completed") -> dict:
        return {"type": "message", "id": msg_id, "status": status, "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]}

    @classmethod
    def output_items(cls, r: CanonicalResponse, msg_id: str, custom_names: set[str]) -> list[dict]:
        items = [cls.message_item(msg_id, r.text)] if (r.text or not r.tool_calls) else []
        return items + [cls.call_item(c, custom_names) for c in r.tool_calls]

    @classmethod
    def envelope(cls, body: dict, rid: str, created: int, model: str, status: str = "completed", output: list | None = None, usage: dict | None = None, r: CanonicalResponse | None = None) -> dict:
        incomplete = bool(r and r.finish == "length")
        return {"id": rid, "object": "response", "created_at": created, "status": "incomplete" if incomplete else status, "error": None, "incomplete_details": {"reason": "max_output_tokens"} if incomplete else None, "instructions": body.get("instructions"), "max_output_tokens": body.get("max_output_tokens"), "model": model, "output": output or [], "parallel_tool_calls": True, "previous_response_id": body.get("previous_response_id"), "reasoning": {"effort": None, "summary": None}, "store": body.get("store", True), "temperature": body.get("temperature", 1.0), "text": body.get("text") or {"format": {"type": "text"}}, "tool_choice": body.get("tool_choice", "auto"), "tools": body.get("tools") or [], "top_p": body.get("top_p", 1.0), "truncation": body.get("truncation", "disabled"), "usage": usage, "user": None, "metadata": body.get("metadata") or {}}

    @classmethod
    def complete_response(cls, body: dict, rid: str, created: int, model: str, req: CanonicalRequest, r: CanonicalResponse, store: ResponseStore | None = None) -> dict:
        if body.get("store", True) and store:
            store.remember(rid, req, r)
        return cls.envelope(body, rid, created, model, "completed", cls.output_items(r, "msg_" + uuid.uuid4().hex[:24], req.custom_tool_names()), cls.usage(r.usage), r)

    @classmethod
    async def stream(cls, events: AsyncIterator[Event], body: dict, rid: str, created: int, model: str, req: CanonicalRequest, store: ResponseStore | None = None) -> AsyncIterator[str]:
        """Each output item keeps one identity: a message that resumes after a tool call is a NEW message item with
        its own id and only its own text (clients such as OpenClaw abort with "Responses stream changed output item
        identity" when an id is reused). response.completed lists the items in the order they were streamed."""
        seq = 0

        def ev(name: str, data: dict) -> str:
            nonlocal seq
            seq += 1
            return f"event: {name}\ndata: {json.dumps({'type': name, 'sequence_number': seq, **data}, ensure_ascii=False)}\n\n"

        custom_names = req.custom_tool_names()
        yield ev("response.created", {"response": cls.envelope(body, rid, created, model, "in_progress")})
        yield ev("response.in_progress", {"response": cls.envelope(body, rid, created, model, "in_progress")})
        items: list[dict] = []  # completed output items, in stream order
        msg_id, msg_text, all_text = "", "", ""
        final: CanonicalResponse | None = None

        def close_text() -> list[str]:
            item = cls.message_item(msg_id, msg_text)
            items.append(item)
            index = len(items) - 1
            return [
                ev("response.output_text.done", {"item_id": msg_id, "output_index": index, "content_index": 0, "text": msg_text, "logprobs": []}),
                ev("response.content_part.done", {"item_id": msg_id, "output_index": index, "content_index": 0, "part": {"type": "output_text", "text": msg_text, "annotations": []}}),
                ev("response.output_item.done", {"output_index": index, "item": item}),
            ]

        async for e in events:
            if e.kind == "text":
                if not msg_id:
                    msg_id, msg_text = "msg_" + uuid.uuid4().hex[:24], ""
                    yield ev("response.output_item.added", {"output_index": len(items), "item": {"type": "message", "id": msg_id, "status": "in_progress", "role": "assistant", "content": []}})
                    yield ev("response.content_part.added", {"item_id": msg_id, "output_index": len(items), "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}})
                msg_text += e.text
                all_text += e.text
                yield ev("response.output_text.delta", {"item_id": msg_id, "output_index": len(items), "content_index": 0, "delta": e.text, "logprobs": []})
            elif e.kind == "tool_call" and e.call:
                if msg_id:
                    for chunk in close_text():
                        yield chunk
                    msg_id = ""
                item = cls.call_item(e.call, custom_names, "in_progress")
                index = len(items)
                if item["type"] == "custom_tool_call":
                    yield ev("response.output_item.added", {"output_index": index, "item": {**item, "input": ""}})
                    yield ev("response.custom_tool_call_input.delta", {"item_id": item["id"], "output_index": index, "delta": item["input"]})
                    yield ev("response.custom_tool_call_input.done", {"item_id": item["id"], "output_index": index, "input": item["input"]})
                else:
                    yield ev("response.output_item.added", {"output_index": index, "item": {**item, "arguments": ""}})
                    yield ev("response.function_call_arguments.delta", {"item_id": item["id"], "output_index": index, "delta": item["arguments"]})
                    yield ev("response.function_call_arguments.done", {"item_id": item["id"], "output_index": index, "arguments": item["arguments"]})
                done_item = {**item, "status": "completed"}
                items.append(done_item)
                yield ev("response.output_item.done", {"output_index": index, "item": done_item})
            elif e.kind == "done":
                final = e.response
        if msg_id:
            for chunk in close_text():
                yield chunk
        r = final or CanonicalResponse()
        r.text = all_text.strip() if r.tool_calls else all_text  # same text as the non-streaming path stores
        if not items:  # no text and no calls: still one (empty) message, as non-streaming does
            items.append(cls.message_item("msg_" + uuid.uuid4().hex[:24], ""))
        if body.get("store", True) and store:
            store.remember(rid, req, r)
        yield ev("response.completed", {"response": cls.envelope(body, rid, created, model, "completed", items, cls.usage(r.usage), r)})
