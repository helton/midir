"""Helpers shared by the protocol adapters: content parts as text, tool arguments, ignored parameters, and the guard
that turns malformed input into a 400."""
from __future__ import annotations

import json
import logging
from typing import Any

from midir.canonical import CanonicalRequest
from midir.errors import ClientError

log = logging.getLogger(__name__)

SSE_HEADERS = {"Cache-Control": "no-cache", "X-Accel-Buffering": "no"}

MEDIA_TYPES = {"image_url", "input_image", "image", "input_audio", "audio", "file", "input_file", "document"}


def text_of(content: Any, where: str) -> str:
    """OpenAI-style content (string or list of parts) as text. Media parts become a placeholder (CAVEAT: text-only API)."""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return str(content)
    parts: list[str] = []
    for p in content:
        if isinstance(p, str):
            parts.append(p)
            continue
        if not isinstance(p, dict):
            continue
        t = p.get("type", "")
        if t in ("text", "input_text", "output_text"):
            parts.append(str(p.get("text", "")))
        elif t == "refusal":
            parts.append(str(p.get("refusal", "")))
        elif t in MEDIA_TYPES:
            log.warning("%s: '%s' content replaced by a placeholder (the StackSpot Agent API is text-only)", where, t)
            parts.append(f"[{t} omitted: this model only receives text]")
        else:
            parts.append(json.dumps(p, ensure_ascii=False))
    return "\n".join(parts)


def parse_arguments(raw: Any) -> Any:
    if isinstance(raw, str):
        try:
            return json.loads(raw) if raw.strip() else {}
        except json.JSONDecodeError:
            return raw
    return raw if raw is not None else {}


def arguments_str(a: Any) -> str:
    return a if isinstance(a, str) else json.dumps(a, ensure_ascii=False)


def ignored_params(body: dict, names: tuple[str, ...]) -> list[str]:
    return [k for k in names if body.get(k) is not None and body.get(k) is not False and body.get(k) != [] and body.get(k) != {}]


def to_canonical(adapter: Any, body: dict, **kwargs: Any) -> CanonicalRequest:
    """Adapter call where malformed input (a string where an object or list belongs, ...) is a 400, never a 500."""
    try:
        req = adapter.to_canonical(body, **kwargs)
    except ClientError:
        raise
    except (AttributeError, TypeError, KeyError, ValueError) as e:
        log.warning("malformed %s request: %r", adapter.__name__, e)
        raise ClientError(f"malformed request ({type(e).__name__}: {e}); check the types of messages/input, content, tools and system", "invalid_request") from None
    if req.max_tokens is not None and (isinstance(req.max_tokens, bool) or not isinstance(req.max_tokens, int) or req.max_tokens <= 0):
        raise ClientError(f"max_tokens must be a positive integer, got {req.max_tokens!r}", "invalid_request")
    return req


def tool_params(p: Any) -> dict:
    return p or {"type": "object", "properties": {}}
