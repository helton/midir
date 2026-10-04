# Architecture

Midir is one Rust binary (`src/`, about 7k lines; tokio + axum for HTTP, reqwest with rustls for backends). It has
three layers. Each request crosses them in this order, and only canonical types travel between them.

```
client ──HTTP──▶ protocols ──CanonicalRequest──▶ gateway (route by model) ──▶ runner ──▶ backend ──▶ LLM service
       ◀─SSE/JSON─ protocols ◀──Events / CanonicalResponse────────────────── runner ◀── backend ◀──
```

1. **Protocols** (`src/protocols/`): OpenAI Chat Completions, OpenAI Responses and Anthropic Messages, translated
   to and from one canonical request and response (`src/canonical.rs`). They know nothing about backends and have
   no client-specific logic.
2. **Gateway** (`src/gateway.rs`): resolves the requested `model` to a configured model (`src/config.rs`: name,
   alias, regex, provider prefix, substring, default) and hands the request to that model's runner.
3. **Runners and backends**: a runner turns a canonical request into a stream of events or one response. Today every
   backend is a **text backend** (one prompt in, text out), and its runner is the **emulation engine**
   (`src/emulation/`), which builds what such a backend lacks:

   | Module | Role |
   |---|---|
   | `emulation/prompt.rs` | conversation, tools and output format rendered as one prompt (oldest turns dropped above the cap) |
   | `emulation/parser.rs` | incremental parser of `<tool_call>` blocks in the model's text |
   | `emulation/followups.rs` | detectors for replies that need one automatic follow-up (announce and stop, false incapacity, redundant confirmation) |
   | `emulation/jsonmode.rs` | JSON output validation and repair |
   | `emulation/output.rs` | stop sequences and `max_tokens` applied to the streamed text |
   | `emulation/engine.rs` | the runner: one backend call per step, follow-ups, input-too-long retry, usage |

Around them:

| Module | Role |
|---|---|
| `limiter.rs` | one queue per backend: concurrency, requests per minute, adaptive budget after a 429 |
| `store.rs` | the Responses API store for `previous_response_id` (one JSON file per response, content-addressed blobs) |
| `telemetry.rs`, `otlp.rs` | one OpenTelemetry span and a few metrics per request, exported over OTLP/HTTP protobuf (hand-written encoder, no SDK) |
| `app.rs` | HTTP routes, errors in each protocol's format, SSE keepalive, `/health` and `/ready` |
| `main.rs`, `buildinfo.rs`, `log.rs` | the `midir` command (clap): arguments, banner, `.env` (dotenvy), build identification, logging (tracing), graceful stop |
| `text.rs` | character-based string helpers and the one-line JSON style used in prompts |

## Backends

| Type | Module | Kind | Target of a model |
|---|---|---|---|
| `stackspot` | `src/backends/stackspot.rs` | text | StackSpot AI agent id |

A backend is configured as `[backends.<name>]` with a `type`, its own options and `[backends.<name>.limits]`; each
`[[models]]` entry names its `backend` and `target`.

### Adding a text backend

For an API that takes one prompt and returns text (another agent platform, a completion endpoint), the backend needs
what `StackSpotBackend` provides: `stream(prompt, target, meta)` (text items, then one `Completion` with usage when
known), `ready()` (credentials and network, without spending model quota), `validate()`, `input_limit_exceeded()`
and `describe_target()`. With a second backend, extract those methods into a `TextBackend` trait, make the emulation
engine hold an `Arc<dyn TextBackend>`, and dispatch on `type` in `create_backend` (`src/backends/mod.rs`, which also
lists `BACKEND_TYPES`). The emulation engine, the queue, retries, telemetry and all three protocols then work
unchanged. Cover it in `tests/` with a scripted stand-in, as `tests/common/mod.rs` does for StackSpot.

### Adding a backend with a native chat API

For a service that already has messages and tools (OpenAI, Anthropic, Bedrock, Ollama), the emulation layer should
be skipped: give the gateway a second kind of runner that maps the canonical request to the service's own API, next
to `EmulationEngine` (`Gateway::runners`). This is the planned extension point; no native backend exists yet.

## Requests and prompts

Each protocol adapter decodes the request body into its own typed request (`serde`): a field of the wrong type is a
400 that names it (`invalid request: messages[2].content: a string or a list of content parts`), and unknown fields
are ignored. Parameters a text backend cannot honor are accepted and reported once per client in the log
(`temperature`, `reasoning_effort`, ...); the few that would change the answer's meaning are refused (`n > 1`,
`logprobs`).

What the model reads is plain text built by `emulation/prompt.rs`: the client's system prompt, the tool protocol with
one compact JSON line per tool, the output format for JSON mode, the conversation, and the last turn. JSON values
written into the conversation (earlier tool calls, schemas) use one-line JSON with a space after `:` and `,`
(`{"name": "read_file", "arguments": {"path": "a.py"}}`), the same form the tool protocol shows the model. Lengths
(the prompt cap, description limits, the four-characters-per-token estimates) count characters, not bytes.

`match` patterns in the configuration use the `regex` crate's syntax (case-insensitive; no lookaround or
backreferences, matching in linear time).

## Tests

`tests/` is a black-box regression suite (`cargo test --release`): each test starts the binary Cargo just built as a
process, in front of a scripted StackSpot stand-in (an axum server inside the test, `tests/common/mod.rs`), and talks
to it only over HTTP, so it checks exactly what clients see: request and response shapes of the three protocols,
SSE event sequences and keepalives, follow-ups, retries and error mapping, the Responses store on disk, the CLI and
configuration errors, and the OTLP export (decoded with the official protobuf definitions). Opt-in tests:
`--test mitm -- --ignored` (the observability proxy streams SSE) and `--test replay -- --ignored` (real client
requests, kept out of the repository; `MIDIR_CAPTURES=<dir>`).
