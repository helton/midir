"""Model text -> text and tool-call events, incrementally (the emulated tool protocol of midir.emulation.prompt)."""
from __future__ import annotations

import json
import logging
import re
from typing import Any

from midir.canonical import ToolCall, ToolSpec, new_call_id

log = logging.getLogger(__name__)

OPEN_TAG = "<tool_call"
CLOSE_TAG = "</tool_call>"
OPEN_RE = re.compile(r'<tool_call(?:\s+id\s*=\s*"?([\w.-]*)"?)?\s*>', re.S)
FENCE_RE = re.compile(r"^\s*```(?:json)?\s*|\s*```\s*$", re.S)


class ToolCallParser:
    """Feed text deltas; get ("text", str) and ("tool_call", ToolCall) events.
    Holds back any suffix that could be the start of "<tool_call" so the tag never leaks as text."""

    def __init__(self, tools: list[ToolSpec] | None = None) -> None:
        self.buf = ""
        self.in_call = False
        self.errors: list[str] = []
        self.rejected: list[str] = []  # raw content of calls dropped because their JSON could not be decoded
        self.tools = tools or []
        self.saw_call = False

    def feed(self, delta: str) -> list[tuple[str, Any]]:
        self.buf += delta
        out: list[tuple[str, Any]] = []
        while True:
            if not self.in_call:
                i = self.buf.find(OPEN_TAG)
                if i >= 0:
                    before = self.buf[:i]
                    if before.strip():
                        out.append(("text", before if before.endswith("\n\n") else before.rstrip()))
                    self.buf, self.in_call = self.buf[i:], True
                    continue
                k = self.buf.rfind("<")
                hold_from = k if k >= 0 and OPEN_TAG.startswith(self.buf[k:]) else len(self.buf)
                if hold_from > 0 and not self.buf[:hold_from].strip():
                    return out  # whitespace alone (e.g. between two calls) waits for real text: it never becomes a text block of its own
                if hold_from > 0:
                    out.append(("text", self.buf[:hold_from]))
                    self.buf = self.buf[hold_from:]
                return out
            j = self.buf.find(CLOSE_TAG)
            if j < 0:
                return out
            block, self.buf = self.buf[: j + len(CLOSE_TAG)], self.buf[j + len(CLOSE_TAG):]
            self.in_call, self.saw_call = False, True
            for call in self._parse_block(block):
                out.append(("tool_call", call))

    def finish(self) -> list[tuple[str, Any]]:
        out: list[tuple[str, Any]] = []
        if self.in_call and self.buf.strip():
            calls = self._parse_block(self.buf + CLOSE_TAG)  # CAVEAT: block without </tool_call>; parsed anyway
            out.extend([("tool_call", c) for c in calls] if calls else [("text", self.buf)])
        elif self.buf and (self.buf.strip() or not self.saw_call):
            out.append(("text", self.buf))
        self.buf, self.in_call = "", False
        return out

    def _parse_block(self, block: str) -> list[ToolCall]:
        """One block may hold one call, a JSON array of calls, or several JSON objects in a row (one per line or
        comma-separated: models do this for parallel calls). Arguments that are not valid JSON are never passed on:
        such a call is dropped with a warning that includes the raw block (truncated), so the next case can be read."""
        m = OPEN_RE.match(block)
        inner = block[m.end(): -len(CLOSE_TAG)] if m else block[len(OPEN_TAG): -len(CLOSE_TAG)]
        inner = FENCE_RE.sub("", inner.strip())
        try:
            parsed = json.loads(inner)
            objs = parsed if isinstance(parsed, list) else [parsed]
        except json.JSONDecodeError as e:
            objs, rest = self._decode_sequence(inner)
            if rest.strip():
                salvaged = self._salvage(rest)
                if salvaged:
                    objs.append(salvaged)
                elif objs and not self._looks_like_call(rest):
                    pass  # trailing junk after a decoded call (an extra "}" or "]"): nothing was lost, nothing to repair
                else:
                    self.rejected.append(rest.strip()[:4000])
                self.errors.append(f"invalid JSON in tool_call ({e}); kept {len(objs)} call(s); raw block: {inner[:300]!r}")
                log.debug("tool_call raw block: %r", inner[:4000])
            elif len(objs) > 1:
                self.errors.append(f"tool_call block held {len(objs)} JSON objects; split into {len(objs)} calls")
        calls = []
        for obj in objs:
            call = self._to_call(obj)
            if call:
                calls.append(call)
        return calls

    @staticmethod
    def _decode_sequence(text: str) -> tuple[list, str]:
        """Decode consecutive JSON values (objects or arrays of objects) separated by whitespace or commas.
        Returns the decoded objects and the undecodable remainder ("" when everything was consumed)."""
        dec, objs, i = json.JSONDecoder(), [], 0
        while True:
            while i < len(text) and text[i] in " \t\r\n,":
                i += 1
            if i >= len(text):
                return objs, ""
            try:
                value, i = dec.raw_decode(text, i)
            except json.JSONDecodeError:
                return objs, text[i:]
            objs.extend(value if isinstance(value, list) else [value])

    @staticmethod
    def _looks_like_call(text: str) -> bool:
        """A remainder that is a (broken) call of its own, not leftover punctuation."""
        return bool(re.search(r'"(name|arguments|parameters|input|tool|function)"\s*:', text))

    @staticmethod
    def _salvage(text: str) -> dict | None:
        """Last resort for one broken object: the "name" string and the "arguments" value decoded on their own."""
        name = re.search(r'"name"\s*:\s*"([^"]+)"', text)
        args = re.search(r'"(?:arguments|parameters|input)"\s*:\s*', text)
        if not name:
            return None
        if not args:
            return {"name": name.group(1), "arguments": {}}
        try:
            value, _ = json.JSONDecoder().raw_decode(text, args.end())
        except json.JSONDecodeError:
            return None
        return {"name": name.group(1), "arguments": value} if isinstance(value, dict) else None

    def _to_call(self, obj: Any) -> ToolCall | None:
        if isinstance(obj, dict) and "name" not in obj:
            obj = self._infer_name(obj)
        if not isinstance(obj, dict) or "name" not in obj:
            self.errors.append("tool_call without 'name'")
            return None
        args = obj.get("arguments", obj.get("parameters", obj.get("input", {})))
        if isinstance(args, str):
            try:
                args = json.loads(args)
            except json.JSONDecodeError:
                self.errors.append(f"tool_call {obj['name']!r} dropped: arguments are not valid JSON: {args[:300]!r}")
                self.rejected.append(json.dumps(obj, ensure_ascii=False)[:4000])
                return None
        if args is not None and not isinstance(args, dict):
            self.errors.append(f"tool_call {obj['name']!r} dropped: arguments are not a JSON object")
            return None
        return ToolCall(new_call_id(), str(obj["name"]), args if args is not None else {})

    def _infer_name(self, obj: dict) -> dict:
        """Smaller models sometimes drop the envelope. Accept {"function": {"name", "arguments"}} (OpenAI shape),
        {"tool"|"tool_name": ..., ...}, and bare arguments when exactly one declared tool matches them (CAVEAT: inference)."""
        fn = obj.get("function")
        if isinstance(fn, dict) and "name" in fn:
            return {"name": fn["name"], "arguments": fn.get("arguments", {})}
        for key in ("tool", "tool_name", "function_name"):
            if isinstance(obj.get(key), str):
                rest = {k: v for k, v in obj.items() if k != key}
                return {"name": obj[key], "arguments": rest.get("arguments", rest)}
        if "arguments" in obj and len(self.tools) == 1:
            return {"name": self.tools[0].name, "arguments": obj["arguments"]}
        keys = set(obj)
        candidates = [t for t in self.tools if keys and keys <= set((t.parameters or {}).get("properties") or {})]
        if len(candidates) > 1:
            # {"file_path": ...} fits Read, Edit and Write; only Read has all its required fields present
            complete = [t for t in candidates if set((t.parameters or {}).get("required") or []) <= keys]
            candidates = complete if len(complete) == 1 else candidates
        if len(candidates) == 1 or (not candidates and len(self.tools) == 1):
            tool = candidates[0] if candidates else self.tools[0]
            self.errors.append(f"tool_call without 'name': inferred {tool.name!r} from the arguments")
            return {"name": tool.name, "arguments": obj}
        return obj

