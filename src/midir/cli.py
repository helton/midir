"""midir: OpenAI- and Anthropic-compatible gateway for LLM backends.

Endpoints (http://127.0.0.1:{port})
  POST /v1/chat/completions       OpenAI Chat Completions (streaming or not, tools, response_format)
  POST /v1/responses              OpenAI Responses (streaming or not, function/custom/namespace tools, previous_response_id)
  GET  /v1/responses/{id}         stored response
  POST /v1/messages               Anthropic Messages (streaming or not, tools)
  POST /v1/messages/count_tokens  Anthropic count_tokens (estimate)
  GET  /v1/models                 configured models (unknown names go to the default model)
  GET  /health, GET /ready        liveness (mapping, queues) and readiness (backend credentials and network)

Configuration: config/midir.toml under the working directory (or MIDIR_CONFIG=<path>); format in
config/midir.example.toml. Secrets live in .env (working directory) and are referenced as ${NAME}. Environment
variables win over the file: MIDIR_PORT, MIDIR_MAX_PROMPT_CHARS, MIDIR_TAIL_REMINDER, MIDIR_TOOL_DESC_MAX,
MIDIR_RESPONSES_DIR, MIDIR_RESPONSES_RETENTION_DAYS, MIDIR_RESPONSES_MAX_MB, MIDIR_KEEPALIVE_S, MIDIR_MAX_CONCURRENT,
MIDIR_REQUESTS_PER_MINUTE, MIDIR_QUEUE_TIMEOUT, MIDIR_COOLDOWN_ON_429, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME,
and per backend (StackSpot): STACKSPOT_REALM, STACKSPOT_CLIENT_ID, STACKSPOT_CLIENT_SECRET, STACKSPOT_CA_BUNDLE.
Without [[models]], STACKSPOT_DEFAULT_AGENT_ID and STACKSPOT_<MODEL>_AGENT_ID define the models
(STACKSPOT_GPT_5_1_AGENT_ID -> model "gpt-5.1"). MIDIR_NO_BANNER=1 skips the startup banner.

No authentication: local use only. Do not expose on a network without something in front of it.
"""
from __future__ import annotations

import argparse
import logging
import os
import sys
from pathlib import Path

from dotenv import load_dotenv

from midir.build import BUILD, BuildInfo

VERSION = BUILD.full_version  # 0.0.1 for a release; 0.0.1+dev.<commit> and the like otherwise (midir.build)

log = logging.getLogger("midir")

def banner(build: BuildInfo) -> str:
    """The startup banner: the name, a vertical rule, and the version (with the kind of build when it is not a release),
    what Midir is and what it translates. Spaces only; the only ambiguous-width character (the arrow) ends its line."""
    name = "M  I  D  I  R"
    left = [" " * len(name), name, " " * len(name)]
    version = f"v{build.full_version}" + (f" · {build.label}" if build.label else "")
    side = [version, "an LLM gateway for agent platforms", "OpenAI · Anthropic ⇄ text-only agents"]
    return "\n" + "\n".join(f"  {n}  │  {t}" for n, t in zip(left, side)) + "\n\n"


def _print_banner() -> None:
    if os.environ.get("MIDIR_NO_BANNER", "").lower() in ("1", "true", "yes", "on"):
        return
    try:
        sys.stderr.write(banner(BUILD))
    except UnicodeEncodeError:  # a console without Unicode: a plain line instead
        sys.stderr.write(f"\n  MIDIR v{VERSION} - an LLM gateway for agent platforms\n\n")
    sys.stderr.flush()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="midir", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, default=None, help="default: MIDIR_PORT, [server] port or 18880")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--debug", action="store_true", help="log the full rendered prompt to stderr (contains everything clients send)")
    parser.add_argument("--version", action="version", version=f"midir {VERSION}")
    args = parser.parse_args(argv)
    _print_banner()

    logging.basicConfig(stream=sys.stderr, level=logging.DEBUG if args.debug else logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s", datefmt="%H:%M:%S")
    for name in ("httpx", "httpcore", "uvicorn", "uvicorn.error", "uvicorn.access"):
        logging.getLogger(name).setLevel(logging.WARNING)
    log.info("midir v%s starting (Python %s)", VERSION, sys.version.split()[0])
    load_dotenv(Path.cwd() / ".env")

    import uvicorn

    from midir.app import build_app
    from midir.config import Config
    from midir.gateway import Gateway
    from midir.telemetry import Telemetry

    cfg = Config()
    gateway = Gateway(cfg, Telemetry(cfg.telemetry.otlp_endpoint, cfg.telemetry.service_name))
    gateway.validate()
    port = args.port or cfg.server.port
    models = ", ".join(f"{m.name}->{m.backend}:{gateway.backends[m.backend].describe_target(m.target)}" for m in cfg.exposed_models)
    log.info("midir %s at http://%s:%d/v1 (chat/completions, responses, messages); config %s; backends %s; default %s; models: %s (max prompt %d chars)",
             VERSION, args.host, port, cfg.source, ", ".join(f"{n} ({b.type})" for n, b in gateway.backends.items()), cfg.default.name, models, cfg.server.max_prompt_chars)
    uvicorn.run(build_app(gateway), host=args.host, port=port, log_level="warning")
    return 0
