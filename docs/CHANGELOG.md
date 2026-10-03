# Changelog

All notable changes are listed here. Versions follow [Semantic Versioning](https://semver.org/); before 1.0.0 a minor
release may change configuration or behavior.

## Unreleased

- **SSE keepalive** while a stream waits for the backend's first content: an SSE comment (`: keepalive`) in Chat
  Completions and Responses, a `ping` event in Anthropic Messages, every `[server] keepalive_s` (15 s;
  `MIDIR_KEEPALIVE_S`, 0 turns it off), and nothing once content flows. Backends took up to 97 s to start in the
  2026-10-03 harness runs; clients and proxies with shorter idle timeouts would abort the stream.
- **Quieter log**: parameters accepted without effect (Hermes sends `reasoning_effort` on every request: 64 of 74
  warnings in that run) are reported once per client and set of parameters, at INFO; repeats go to DEBUG.
- **No crash without the data volume**: when the Responses store folder cannot be created (for example `docker run`
  without mounting `/data`), Midir logs a warning and keeps `previous_response_id` in memory instead of exiting.
- **Image 64% smaller**: two-stage build on Alpine; the final image carries only Python and the locked dependencies
  (no uv, no pip): 222 MB -> 81 MB, 85 MB -> 30 MB to download.
- **Image description on ghcr.io**: the multi-arch images carry the description as manifest and index annotations
  (the package page read "No description provided").

## 0.0.1 (2026-10-03)

First public release, as **Midir**. The gateway was used daily before this, under other names (stackpilot,
llm-gateway); this version starts a clean history.

- **Pluggable backends**: one package (`src/midir/`) in layers: protocol adapters, a gateway that routes each model to
  a backend, and backends; text-only backends get the emulation layer (`midir/emulation/`). StackSpot AI is the
  first backend (`midir/backends/stackspot.py`); docs/architecture.md explains how to add one.
- **Configuration** in `config/midir.toml`: `[backends.<name>]` (type, options, `limits`) and `[[models]]` (name,
  backend, target, aliases, regex, knobs); environment variables `MIDIR_*` and, for StackSpot, `STACKSPOT_*`. The
  earlier layout (`[stackspot]`, `[limits]`, `[[agents]]`, `default`) is still read, with a warning.
- **Three APIs, one core**: OpenAI Chat Completions, OpenAI Responses (with `previous_response_id`, stored on disk for
  30 days, capped at 500 MB) and Anthropic Messages (with `count_tokens`), streaming or not, translated to one
  canonical request and sent to a text-only backend.
- **StackSpot AI backend**: client-credentials token cache, Agent API SSE stream, retries (1, 2, 4 s), error mapping
  in each protocol's format, input-too-long retry with a smaller prompt.
- **Emulated tool calling**: tools are described in the prompt and `<tool_call>` blocks are parsed incrementally into
  real tool calls (parallel calls, several calls in one block, name inference, one repair follow-up for invalid JSON
  with deduplicated output). Automatic follow-ups for replies that only announce an action, deny an ability a tool
  provides, ask to confirm or leave pending an action the request already ordered.
- **Structured output** (JSON mode and JSON Schema): prompt, validation and one repair attempt.
- **Models**: several agents exposed as models, routed by exact name, alias, regex, provider prefix or substring,
  with per-model knobs.
- **Upstream queue**: concurrency and requests-per-minute limits in memory, a pause after a 429 and a budget that
  halves on 429s and recovers by one request per minute; `Retry-After` on the gateway's own 429s.
- **Operations**: `GET /health`, `GET /ready` (credentials and network, no agent call), optional OpenTelemetry (one
  span and metrics per request, never prompt content), Docker image and compose files with mitmproxy and a Grafana
  dashboard, non-root container. The observability overlay sets the proxy variables in lowercase (`https_proxy`,
  `no_proxy`): Python prefers them, and Docker setups behind a corporate proxy inject lowercase values that would win.
- **Clients validated end to end**: Claude Code, GitHub Copilot (VS Code), Hermes Agent, DeepSeek Harness and
  OpenClaw (configs in `examples/clients`).
- **Tests**: offline regression suite (`uv run poe test`) with a scripted backend and the official OpenAI and
  Anthropic SDKs; dependency groups `test` (pytest and the SDKs) and `dev` (adds poethepoet).
- MIT license.
