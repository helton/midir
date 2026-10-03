# Architecture

Midir has three layers. Each request crosses them in this order, and only canonical types travel between them.

```
client ──HTTP──▶ protocols ──CanonicalRequest──▶ gateway (route by model) ──▶ runner ──▶ backend ──▶ LLM service
       ◀─SSE/JSON─ protocols ◀──Events / CanonicalResponse────────────────── runner ◀── backend ◀──
```

1. **Protocols** (`midir/protocols/`): OpenAI Chat Completions, OpenAI Responses and Anthropic Messages, translated
   to and from one canonical request and response (`midir/canonical.py`). They know nothing about backends and have
   no client-specific logic.
2. **Gateway** (`midir/gateway.py`): resolves the requested `model` to a configured model (`midir/config.py`:
   name, alias, regex, provider prefix, substring, default) and hands the request to that model's runner.
3. **Runners and backends**: a runner turns a canonical request into a stream of events (`run`) or one response
   (`complete`). Today every backend is a **text backend** (one prompt in, text out), and its runner is the
   **emulation engine** (`midir/emulation/`), which builds what such a backend lacks:

   | Module | Role |
   |---|---|
   | `emulation/prompt.py` | conversation, tools and output format rendered as one prompt (oldest turns dropped above the cap) |
   | `emulation/parser.py` | incremental parser of `<tool_call>` blocks in the model's text |
   | `emulation/followups.py` | detectors for replies that need one automatic follow-up (announce and stop, false incapacity, redundant confirmation) |
   | `emulation/jsonmode.py` | JSON output validation and repair |
   | `emulation/output.py` | stop sequences and `max_tokens` applied to the streamed text |
   | `emulation/engine.py` | the runner: one backend call per step, follow-ups, input-too-long retry, usage |

Around them: `midir/limiter.py` (one queue per backend: concurrency, requests per minute, adaptive budget after 429),
`midir/store.py` (the Responses API store for `previous_response_id`), `midir/telemetry.py` (OpenTelemetry span and
metrics per request, client and session labels), `midir/app.py` (FastAPI routes, errors in each protocol's format)
and `midir/cli.py` (the `midir` command).

## Backends

| Type | Module | Kind | Target of a model |
|---|---|---|---|
| `stackspot` | `midir/backends/stackspot.py` | text | StackSpot AI agent id |

A backend is configured as `[backends.<name>]` with a `type`, its own options and `[backends.<name>.limits]`; each
`[[models]]` entry names its `backend` and `target`.

### Adding a text backend

For an API that takes one prompt and returns text (another agent platform, a completion endpoint):

1. Subclass `TextBackend` (`midir/backends/base.py`) and implement `stream(prompt, target, meta)` (yield text
   deltas, then one `Completion` with usage when known) and `ready()` (credentials and network, without spending
   model quota). Optionally `validate()`, `input_limit_exceeded()` and `describe_target()`.
2. Register its `type` in `BACKEND_TYPES` (`midir/backends/__init__.py`).
3. Add tests with a scripted stand-in, like `tests/conftest.py` does for StackSpot.

The emulation engine, the queue, retries, telemetry and all three protocols then work unchanged.

### Adding a backend with a native chat API

For a service that already has messages and tools (OpenAI, Anthropic, Bedrock, Ollama), the emulation layer should
be skipped: implement the `Runner` protocol (`midir/gateway.py`: `run` and `complete` over canonical requests) by
mapping the canonical request to the service's own API, and let the gateway build that runner instead of an
`EmulationEngine` for backends of that kind. This is the planned extension point; no native backend exists yet.
