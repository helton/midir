"""HTTP layer: the OpenAI and Anthropic endpoints, errors in each protocol's own format, health and readiness."""
from __future__ import annotations

import asyncio
import json
import logging
import time
import uuid
from contextlib import asynccontextmanager
from typing import Any, AsyncIterator

import httpx
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, StreamingResponse

from midir import __version__ as VERSION
from midir.canonical import estimate_tokens
from midir.config import DEFAULT_MODEL_NAME
from midir.emulation.prompt import render_prompt
from midir.errors import BackendError, ClientError
from midir.gateway import Gateway
from midir.protocols import ChatCompletions, Messages, Responses
from midir.protocols.common import SSE_HEADERS, to_canonical
from midir.telemetry import request_meta

log = logging.getLogger(__name__)

ERROR_TYPES = {400: "invalid_request_error", 401: "authentication_error", 403: "permission_error", 404: "not_found_error", 429: "rate_limit_error"}


def openai_error(status: int, message: str, typ: str, code: str | None) -> JSONResponse:
    return JSONResponse(status_code=status, content={"error": {"message": message, "type": typ, "code": code, "param": None}})


def anthropic_error(status: int, message: str, typ: str) -> JSONResponse:
    return JSONResponse(status_code=status, content={"type": "error", "error": {"type": typ, "message": message}})


def stream_error(flavor: str, message: str, typ: str) -> str:
    """An error in the middle of a stream becomes an error event in the protocol's format instead of a dropped connection."""
    if flavor == "anthropic":
        return f"event: error\ndata: {json.dumps({'type': 'error', 'error': {'type': typ, 'message': message}}, ensure_ascii=False)}\n\n"
    if flavor == "responses":
        return f"event: error\ndata: {json.dumps({'type': 'error', 'code': typ, 'message': message, 'param': None, 'sequence_number': 0}, ensure_ascii=False)}\n\n"
    return f"data: {json.dumps({'error': {'message': message, 'type': typ, 'code': None, 'param': None}}, ensure_ascii=False)}\n\ndata: [DONE]\n\n"


async def guarded(gen: AsyncIterator[str], rid: str, flavor: str) -> AsyncIterator[str]:
    try:
        async for chunk in gen:
            yield chunk
    except BackendError as e:
        log.error("%s backend error mid-stream: %s", rid, e)
        yield stream_error(flavor, e.message(), "api_error")
    except ClientError as e:
        yield stream_error(flavor, e.message, "invalid_request_error")
    except httpx.HTTPError as e:
        log.error("%s network error mid-stream: %r", rid, e)
        yield stream_error(flavor, f"error talking to the backend: {e!r}", "api_error")
    except Exception as e:  # a bug of ours: the client gets an error event instead of a silently dropped connection
        log.exception("%s internal error mid-stream", rid)
        yield stream_error(flavor, f"midir internal error: {e!r}", "api_error")


def build_app(gateway: Gateway) -> FastAPI:
    store, telemetry = gateway.store, gateway.telemetry

    @asynccontextmanager
    async def lifespan(_app: FastAPI):
        async def purge_hourly() -> None:
            while True:
                await asyncio.sleep(3600)
                await asyncio.to_thread(store.purge)

        task = asyncio.create_task(purge_hourly()) if store.dir else None
        yield
        if task:
            task.cancel()
        await gateway.aclose()

    app = FastAPI(title="midir", version=VERSION, lifespan=lifespan)

    def is_anthropic(request: Request) -> bool:
        return request.url.path.startswith("/v1/messages")

    @app.exception_handler(ClientError)
    async def _client_error(request: Request, e: ClientError):
        log.warning("%s %s: %s", request.url.path, e.code, e.message)
        if is_anthropic(request):
            return anthropic_error(e.status, e.message, ERROR_TYPES.get(e.status, "invalid_request_error"))
        return openai_error(e.status, e.message, ERROR_TYPES.get(e.status, "invalid_request_error"), e.code)

    @app.exception_handler(BackendError)
    async def _backend_error(request: Request, e: BackendError):
        log.error("%s %s %s: %s", e.backend, e.where, e.status, str(e.body)[:500])
        status = e.http_status()
        resp = anthropic_error(status, e.message(), ERROR_TYPES.get(status, "api_error")) if is_anthropic(request) else openai_error(status, e.message(), ERROR_TYPES.get(status, "api_error"), f"upstream_{e.status}")
        backend = gateway.backend_of(e.backend)
        if status == 429 and backend:  # SDK clients (OpenAI, Anthropic) wait this long before their own retry
            resp.headers["retry-after"] = str(backend.limiter.retry_after())
        return resp

    @app.exception_handler(httpx.HTTPError)
    async def _network_error(request: Request, e: httpx.HTTPError):
        status = 504 if isinstance(e, httpx.TimeoutException) else 502
        message = f"error talking to the backend: {e!r}"
        return anthropic_error(status, message, "api_error") if is_anthropic(request) else openai_error(status, message, "api_error", "upstream_network")

    async def read_json(request: Request) -> dict:
        try:
            body = await request.json()
        except Exception:
            raise ClientError("request body is not valid JSON", "invalid_json")
        if not isinstance(body, dict):
            raise ClientError("request body must be a JSON object", "invalid_json")
        return body

    def prepare(request: Request, body: dict, adapter: Any, protocol: str, rid: str, **kwargs: Any) -> tuple[Any, Any, str, bool]:
        """Canonical request routed to its model: (request, runner, requested model name, stream?)."""
        req = to_canonical(adapter, body, **kwargs)
        model_name, stream = body.get("model") or DEFAULT_MODEL_NAME, bool(body.get("stream"))
        req.route, runner = gateway.route(model_name)
        req.meta.update(request_meta(request.headers, body, protocol, model_name, req.route, req, rid, store.session_of))
        m = req.meta
        log.info("%s %s model=%s->%s/%s stream=%s tools=%d choice=%s json=%s%s client=%s session=%s", rid, protocol, model_name, req.route.backend, req.route.name, stream,
                 len(req.tools), req.tool_choice, req.json_schema is not None, f" previous={body.get('previous_response_id')}" if body.get("previous_response_id") else "",
                 m["client"], m["session"][:12])
        return req, runner, model_name, stream

    @app.get("/health")
    async def health():
        cfg = gateway.config
        return {"ok": True, "version": VERSION, "config": cfg.source, "default": cfg.default.name, "models": gateway.describe_models(),
                "backends": {n: {"type": b.type, "queue": b.limiter.state()} for n, b in gateway.backends.items()}, "protocols": ["chat-completions", "responses", "messages"]}

    @app.get("/ready")
    async def ready():
        """Readiness of every backend without spending model quota (StackSpot: gets the cached token, so this reaches
        the identity server at most once per 20 minutes)."""
        status = await gateway.ready()
        failed = {n: why for n, why in status.items() if why}
        body = {"ok": not failed, "version": VERSION, "backends": {n: {"ok": why is None, **({"error": why} if why else {}), "queue": gateway.backends[n].limiter.state()} for n, why in status.items()}}
        if failed:
            return JSONResponse(status_code=503, content={**body, "error": "; ".join(failed.values())})
        return body

    @app.get("/v1/models")
    async def models():
        now = int(time.time())
        return {"object": "list", "data": [{"id": m.name, "object": "model", "created": now, "owned_by": m.backend, "description": m.description} for m in gateway.config.exposed_models]}

    @app.get("/v1/models/{model_id}")
    async def model(model_id: str):
        return {"id": model_id, "object": "model", "created": int(time.time()), "owned_by": gateway.config.resolve(model_id).backend}

    @app.post("/v1/embeddings")
    async def embeddings():
        raise ClientError("no configured backend provides embeddings", "unsupported_endpoint", 404)

    # ---- OpenAI Chat Completions ----
    @app.post("/v1/chat/completions")
    async def chat_completions(request: Request):
        body = await read_json(request)
        cid, created = f"chatcmpl-{uuid.uuid4().hex[:24]}", int(time.time())
        req, runner, model_name, stream = prepare(request, body, ChatCompletions, "chat", cid)
        if not stream:
            return ChatCompletions.response(await telemetry.observe_complete(runner.complete(req, cid), req, cid), cid, created, model_name)
        include_usage = bool((body.get("stream_options") or {}).get("include_usage", True))
        events = telemetry.observe(runner.run(req, cid), req, cid)
        return StreamingResponse(guarded(ChatCompletions.stream(events, cid, created, model_name, include_usage), cid, "openai"), media_type="text/event-stream", headers=SSE_HEADERS)

    # ---- OpenAI Responses ----
    @app.post("/v1/responses")
    async def responses(request: Request):
        body = await read_json(request)
        rid, created = f"resp_{uuid.uuid4().hex[:24]}", int(time.time())
        req, runner, model_name, stream = prepare(request, body, Responses, "responses", rid, store=store)
        if not stream:
            return Responses.complete_response(body, rid, created, model_name, req, await telemetry.observe_complete(runner.complete(req, rid), req, rid), store)
        events = telemetry.observe(runner.run(req, rid), req, rid)
        return StreamingResponse(guarded(Responses.stream(events, body, rid, created, model_name, req, store), rid, "responses"), media_type="text/event-stream", headers=SSE_HEADERS)

    @app.get("/v1/responses/{rid}")
    async def get_response(rid: str):
        stored = store.load(rid)
        if not stored:
            raise ClientError(f"response '{rid}' not found (expired or never existed)", "not_found", 404)
        ts, req, r = stored
        return Responses.envelope({}, rid, int(ts), DEFAULT_MODEL_NAME, "completed", Responses.output_items(r, "msg_" + uuid.uuid4().hex[:24], req.custom_tool_names()), Responses.usage(r.usage), r)

    # ---- Anthropic Messages ----
    @app.post("/v1/messages")
    async def messages(request: Request):
        body = await read_json(request)
        mid = f"msg_{uuid.uuid4().hex[:24]}"
        req, runner, model_name, stream = prepare(request, body, Messages, "messages", mid)
        if not stream:
            return Messages.response(await telemetry.observe_complete(runner.complete(req, mid), req, mid), mid, model_name)
        events = telemetry.observe(runner.run(req, mid), req, mid)
        return StreamingResponse(guarded(Messages.stream(events, mid, model_name), mid, "anthropic"), media_type="text/event-stream", headers=SSE_HEADERS)

    @app.post("/v1/messages/count_tokens")
    async def count_tokens(request: Request):
        body = await read_json(request)
        prompt, _ = render_prompt(to_canonical(Messages, body), 10**9)
        return {"input_tokens": estimate_tokens(prompt)}  # CAVEAT: character-based estimate of the emulated prompt

    return app
