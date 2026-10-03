"""Runs canonical requests on a text-only backend: renders the prompt, streams the backend's text through the tool-call
parser and the output limiter, and makes the automatic follow-ups (invalid tool JSON, announce-and-stop, false
incapacity, tool_choice retry, JSON-mode repair). This is the runner the HTTP layer calls for every model whose
backend is a TextBackend."""
from __future__ import annotations

import json
import logging
import time
from typing import AsyncIterator

from midir.backends.base import Completion, TextBackend
from midir.canonical import CHARS_PER_TOKEN, CanonicalRequest, CanonicalResponse, Event, ToolCall, estimate_tokens, sum_usage
from midir.config import Config
from midir.emulation.followups import FollowUps
from midir.emulation.jsonmode import check_json
from midir.emulation.output import OutputLimiter
from midir.emulation.parser import ToolCallParser
from midir.emulation.prompt import render_prompt
from midir.errors import BackendError

log = logging.getLogger(__name__)


class EmulationEngine:
    """One per text backend. `run` streams events; `complete` collects them (non-streaming requests)."""

    def __init__(self, backend: TextBackend, config: Config) -> None:
        self.backend = backend
        self.config = config
        self.learned_max_chars: dict[str, int] = {}  # target -> prompt cap learned from an input-too-long refusal

    # ---- public runner contract ----
    async def run(self, req: CanonicalRequest, rid: str) -> AsyncIterator[Event]:
        """Event stream. JSON mode is fully buffered (CAVEAT: no incremental streaming) so it can be validated and repaired."""
        if req.ignored:
            log.warning("%s accepted parameters without effect: %s", rid, ", ".join(req.ignored))
        if req.json_schema is not None:
            resp = await self._json_mode(req, rid)
            if resp.text:
                yield Event("text", text=resp.text)
            yield Event("done", response=resp)
            return
        text_parts: list[str] = []
        calls: list[ToolCall] = []
        final: CanonicalResponse | None = None
        async for ev in self._stream_once(req, rid):
            if ev.kind == "done":
                final = ev.response
                break
            if ev.kind == "text":
                text_parts.append(ev.text)
            elif ev.call:
                calls.append(ev.call)
            yield ev
        resp = final or CanonicalResponse()
        text = "".join(text_parts)
        if resp.rejected_calls and req.tools and req.tool_choice != "none" and resp.finish in ("stop", "tool_calls"):
            async for ev in self._repair_calls(req, rid, resp, text, calls):
                yield ev
            calls = resp.tool_calls or calls
        if FollowUps.false_incapacity(req, resp, text, calls):
            # CAVEAT: the model denied having web/file/shell access although a listed tool provides it; one follow-up
            # names the tools and asks for the call, appended to the same response.
            names = ", ".join(t.name for t in req.tools if FollowUps.TOOL_ABILITY_RE.search(t.name + " " + t.description))[:300]
            log.warning("%s response denies an ability that a tool provides (%s); requesting the call (1 follow-up)", rid, names[:80])
            prompt = (f"You do have that ability through your tools ({names}). Use the appropriate tool now: reply with the <tool_call> block(s) only, "
                      "inferring the URL or path if the user did not give one.")
            async for ev in self._ask_for_calls(req, rid + "/ability", resp, text, calls, prompt):
                yield ev
        elif FollowUps.promise_only(req, resp, text, calls):
            # CAVEAT: the model announced an action ("I will read the files...") and stopped without a <tool_call>.
            # One follow-up asks for the calls and appends them to the SAME response (text + tool calls is valid in all protocols).
            confirm = FollowUps.redundant_confirmation(req, text)
            log.warning("%s response only announces an action without a tool call; requesting the calls (1 follow-up)%s", rid,
                        f" [asked to confirm '{confirm}', already requested]" if confirm else "")
            prompt = (f"The request already asks for this ({confirm}); do not ask for confirmation. Do it now: reply with the <tool_call> block(s) only." if confirm else
                      "You announced an action but emitted no <tool_call>. Do it now: reply with the <tool_call> block(s) for what you just announced, and nothing else.")
            async for ev in self._ask_for_calls(req, rid + "/act", resp, text, calls, prompt):
                yield ev
        yield Event("done", response=resp)

    async def complete(self, req: CanonicalRequest, rid: str) -> CanonicalResponse:
        """Non-streaming: collect everything; retry once when tool_choice is required/named and nothing was called (CAVEAT)."""
        resp = await self._collect(self.run(req, rid))
        if req.tools and req.tool_choice not in ("auto", "none") and req.json_schema is None and not resp.tool_calls:
            log.warning("%s tool_choice=%s but no tool call; retrying once", rid, req.tool_choice)
            req.meta["followups"] = req.meta.get("followups", 0) + 1
            follow = req.derive(json_schema=None, max_tokens=req.max_tokens)
            follow.add("assistant", resp.text)
            follow.add("user", "You did not call a tool. You MUST respond with a <tool_call> block now, and nothing else.")
            retry_resp = await self._collect(self._stream_once(follow, rid + "/retry"))
            if retry_resp.tool_calls:
                retry_resp.usage = sum_usage(resp.usage, retry_resp.usage)
                return retry_resp
        return resp

    # ---- follow-ups ----
    async def _ask_for_calls(self, req: CanonicalRequest, rid: str, resp: CanonicalResponse, text: str, calls: list[ToolCall], prompt: str) -> AsyncIterator[Event]:
        """One hidden follow-up whose tool calls are appended to the same response."""
        req.meta["followups"] = req.meta.get("followups", 0) + 1
        follow = req.derive(json_schema=None, max_tokens=None)
        follow.add("assistant", text)
        follow.add("user", prompt)
        extra: list[ToolCall] = []
        extra_usage: dict = {}
        async for ev in self._stream_once(follow, rid):
            if ev.kind == "tool_call" and ev.call:
                extra.append(ev.call)
                yield ev
            elif ev.kind == "done" and ev.response:
                extra_usage = ev.response.usage
        if extra:
            resp.tool_calls, resp.finish = calls + extra, "tool_calls"
        resp.usage = sum_usage(resp.usage, extra_usage)

    async def _repair_calls(self, req: CanonicalRequest, rid: str, resp: CanonicalResponse, text: str, calls: list[ToolCall]) -> AsyncIterator[Event]:
        """CAVEAT: a <tool_call> had JSON that could not be decoded (typically unescaped quotes in a shell command). It is
        never passed on as a broken string; one follow-up asks for only the broken call(s), and its reply is filtered:
        at most as many calls as were broken, never a repeat of a call already streamed (same tool and target)."""
        log.warning("%s %d tool call(s) with invalid JSON; asking for corrected calls (1 follow-up)", rid, len(resp.rejected_calls))
        req.meta["followups"] = req.meta.get("followups", 0) + 1
        follow = req.derive(json_schema=None, max_tokens=None)
        follow.add("assistant", text, tool_calls=list(calls))
        bad = "\n".join(f"<tool_call>\n{b}\n</tool_call>" for b in resp.rejected_calls)
        done = "; ".join(f"{c.name} {FollowUps.call_target(c)}" for c in calls)
        follow.add("user", f"These {len(resp.rejected_calls)} <tool_call> block(s) could not be parsed as JSON, so they were not executed:\n{bad}\n"
                   "Re-emit only these call(s) as valid JSON, with the same tool and the same intended values (file contents and "
                   "commands unchanged). Reply with the <tool_call> block(s) only." + (f" Do not repeat the calls that already went through ({done[:500]})." if calls else ""))
        extra: list[ToolCall] = []
        extra_usage: dict = {}
        seen = {FollowUps.call_key(c) for c in calls}
        exact = {(c.name, json.dumps(c.arguments, sort_keys=True)) for c in calls}
        rejected_text = "\n".join(resp.rejected_calls)
        dropped = 0
        async for ev in self._stream_once(follow, rid + "/repair"):
            if ev.kind == "tool_call" and ev.call:
                key = FollowUps.call_key(ev.call)
                target = key[1].split("=", 1)[-1]
                # same tool + same target as a streamed call is a duplicate, unless the broken block was about that
                # target too (two edits of one file); an identical call is always a duplicate
                dup = (ev.call.name, json.dumps(ev.call.arguments, sort_keys=True)) in exact or (key in seen and json.dumps(target)[1:-1] not in rejected_text)
                if dup or len(extra) >= len(resp.rejected_calls):
                    dropped += 1  # a call already streamed (often re-emitted double-escaped) or more than were asked for
                    continue
                seen.add(key)
                extra.append(ev.call)
                yield ev
            elif ev.kind == "done" and ev.response:
                extra_usage = ev.response.usage
        log.info("%s/repair kept %d call(s), dropped %d (duplicates of streamed calls or beyond the %d requested)", rid, len(extra), dropped, len(resp.rejected_calls))
        if extra:
            resp.tool_calls, resp.finish = calls + extra, "tool_calls"
        resp.usage = sum_usage(resp.usage, extra_usage)

    async def _json_mode(self, req: CanonicalRequest, rid: str) -> CanonicalResponse:
        resp = await self._collect(self._stream_once(req, rid))
        schema = req.json_schema or {}
        normalized, errs = check_json(resp.text, schema)
        if normalized is not None:
            resp.text = normalized
            return resp
        log.warning("%s invalid JSON (%s); one repair attempt", rid, "; ".join(errs)[:200])
        req.meta["repairs"] = req.meta.get("repairs", 0) + 1
        repair = req.derive(tools=[], tool_choice="none", json_schema=req.json_schema, max_tokens=req.max_tokens)
        repair.add("assistant", resp.text)
        repair.add("user", "Your previous response was not valid: " + "; ".join(errs)[:500] + ". Respond again with only the JSON value, no fences, no prose.")
        repaired = await self._collect(self._stream_once(repair, rid + "/repair"))
        normalized, errs = check_json(repaired.text, schema)
        repaired.usage = sum_usage(resp.usage, repaired.usage)
        if normalized is not None:
            repaired.text = normalized
        else:
            log.error("%s JSON still invalid after repair: %s", rid, "; ".join(errs)[:200])  # CAVEAT: returned as produced
        return repaired

    @staticmethod
    async def _collect(events: AsyncIterator[Event]) -> CanonicalResponse:
        text_parts: list[str] = []
        calls: list[ToolCall] = []
        final: CanonicalResponse | None = None
        async for ev in events:
            if ev.kind == "text":
                text_parts.append(ev.text)
            elif ev.kind == "tool_call" and ev.call:
                calls.append(ev.call)
            elif ev.kind == "done":
                final = ev.response
        resp = final or CanonicalResponse()
        text = "".join(text_parts)
        resp.text = text.strip() if calls else text
        resp.tool_calls = calls
        return resp

    # ---- one backend call ----
    def _target(self, req: CanonicalRequest) -> str:
        return req.route.target if req.route else self.config.default.target

    def _max_chars(self, req: CanonicalRequest, configured: int) -> int:
        return min(configured, self.learned_max_chars.get(self._target(req), configured))

    async def _stream_once(self, req: CanonicalRequest, rid: str) -> AsyncIterator[Event]:
        configured, tail_reminder, tool_desc_max = self.config.knobs(req.route)
        max_chars = self._max_chars(req, configured)
        target = self._target(req)
        t0 = time.perf_counter()
        for attempt in (1, 2):
            prompt, info = render_prompt(req, max_chars, tail_reminder=tail_reminder, tool_desc_max=tool_desc_max)
            dropped = f", -{info['dropped_turns']} dropped" if info["dropped_turns"] else ""
            log.info("%s prompt=%d chars (system=%d, history=%d turns, tools=%d%s)", rid, info["chars"], info["system_chars"], info["history_turns"], info["tools"], dropped)
            req.meta["prompt_chars"] = req.meta.get("prompt_chars", 0) + info["chars"]
            req.meta["dropped_turns"] = req.meta.get("dropped_turns", 0) + info["dropped_turns"]
            req.meta["upstream_calls"] = req.meta.get("upstream_calls", 0) + 1
            if info["dropped_turns"]:
                self.backend.telemetry.truncated(req.meta, info["dropped_turns"])
            log.debug("%s PROMPT >>>\n%s\n<<< PROMPT", rid, prompt)
            stream = self.backend.stream(prompt, target, req.meta)
            try:
                head = await stream.__anext__()  # opens the backend call: an input-too-long refusal arrives here
                break
            except StopAsyncIteration:
                head = None
                break
            except BackendError as e:
                limits = self.backend.input_limit_exceeded(e) if attempt == 1 else None
                if not limits or limits[1] <= limits[0]:
                    raise
                cap = int(info["chars"] * limits[0] / max(limits[1], 1) * 0.9)
                log.warning("%s input above the model's token limit at %d chars; retrying once capped at %d chars (kept for this target)", rid, info["chars"], cap)
                self.learned_max_chars[target] = cap
                max_chars = cap
        parser = ToolCallParser(req.tools)
        limiter = OutputLimiter(req.stop, int(req.max_tokens * CHARS_PER_TOKEN) if req.max_tokens else None)
        calls: list[ToolCall] = []
        final = Completion()
        emitted = received = 0
        first = True
        stopped = False

        async def items() -> AsyncIterator[str | Completion]:
            if head is not None:
                yield head
                async for item in stream:
                    yield item

        try:
            async for item in items():
                if isinstance(item, Completion):
                    final = item
                    continue
                received += len(item)
                if first:
                    log.info("%s ttfb %.2fs", rid, time.perf_counter() - t0)
                    first = False
                for kind, value in parser.feed(item):
                    if kind == "text":
                        out, stopped = limiter.apply(value)
                        if out:
                            emitted += len(out)
                            yield Event("text", text=out)
                        if stopped:
                            break
                    else:
                        calls.append(value)
                        yield Event("tool_call", call=value)
                if stopped:
                    break  # CAVEAT: the backend stream is closed; the tokens already generated were billed
        finally:
            await stream.aclose()  # releases the backend slot now, not when the generator is garbage-collected
        if not stopped:
            tail = parser.finish()
            for kind, value in tail:
                if kind == "tool_call":
                    calls.append(value)
                    yield Event("tool_call", call=value)
            out, _ = limiter.apply("".join(v for k, v in tail if k == "text"), flush=True)
            if out:
                emitted += len(out)
                yield Event("text", text=out)
        if parser.errors:
            log.warning("%s parser: %s", rid, "; ".join(parser.errors))
            req.meta["parse_errors"] = req.meta.get("parse_errors", 0) + len(parser.errors)
        finish = "tool_calls" if calls and limiter.finish == "stop" else limiter.finish
        usage = final.usage
        if not usage:
            # CAVEAT: the backend's token counts were not read: a stop/max_tokens cut closed the stream early, or they
            # never came. Estimated from characters so usage and telemetry never report zero.
            usage = {"prompt_tokens": estimate_tokens(prompt), "completion_tokens": max(1, int(received / CHARS_PER_TOKEN)), "total_tokens": 0}
            usage["total_tokens"] = usage["prompt_tokens"] + usage["completion_tokens"]
            req.meta["usage_estimated"] = True
        resp = CanonicalResponse(tool_calls=calls, finish=finish, stop_sequence=limiter.stop_sequence, usage=usage, message_id=final.message_id, rejected_calls=list(parser.rejected))
        log.info("%s ok in %.1fs, %d chars, %d tool calls, finish=%s, usage=%s", rid, time.perf_counter() - t0, emitted, len(calls), finish, resp.usage)
        yield Event("done", response=resp)
