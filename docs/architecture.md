# Architecture

Midir is one Rust binary (`src/`, about 9k lines with the unit tests; tokio + axum for HTTP, reqwest with rustls for
backends, jemalloc as the allocator). It has three layers. Each request crosses them in this order, and only canonical types travel between them.

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
   | `emulation/prompt.rs` | conversation, tools and output format rendered as one prompt; above the cap the oldest turns are dropped, then the largest tool results and messages cut in the middle (computed in linear time) |
   | `emulation/parser.rs` | incremental parser of `<tool_call>` blocks in the model's text (tolerant of raw control characters and trailing commas) |
   | `emulation/followups.rs` | detectors for replies that need an automatic follow-up (announce and stop, false incapacity, redundant confirmation), and what the user's latest instructions order or forbid |
   | `emulation/jsonmode.rs` | JSON output validation and repair |
   | `emulation/output.rs` | stop sequences and `max_tokens` applied to the streamed text |
   | `emulation/engine.rs` | the runner: one backend call per step, each follow-up a function that returns its request (or none), input-too-long retry, usage |

Around them:

| Module | Role |
|---|---|
| `limiter.rs` | one queue per backend: concurrency, requests per minute, adaptive budget after a 429 |
| `store.rs` | the Responses API store for `previous_response_id`: a memory cache capped by bytes, where a response shares its history with the one it continues, and one JSON file per response with content-addressed blobs (disk work on the blocking pool) |
| `telemetry.rs`, `otlp.rs` | a SERVER span per request with a CLIENT child per backend call, and a few metrics, exported over OTLP/HTTP protobuf (hand-written encoder, no SDK) or served at `/metrics` |
| `app.rs` | HTTP routes, errors in each protocol's format, the optional API key, SSE keepalive, `/health`, `/ready` and `/metrics`, panics turned into a 500 or an error event |
| `main.rs`, `buildinfo.rs`, `log.rs` | the `midir` command (clap): arguments, banner, `.env` (dotenvy), build identification, logging (tracing, `MIDIR_LOG`), graceful stop |
| `json.rs` | what JSON clients send made decodable (lone UTF-16 surrogates, `NaN`), tolerant decoding of model JSON, and the JSON text Midir writes into prompts |
| `text.rs` | character-based string helpers |

## Backends

| Type | Module | Kind | Target of a model |
|---|---|---|---|
| `stackspot` | `src/backends/stackspot.rs` | text | StackSpot AI agent id |

A backend is configured as `[backends.<name>]` with a `type`, its own options and `[backends.<name>.limits]`; each
`[[models]]` entry names its `backend` and `target`.

### Adding a text backend

For an API that takes one prompt and returns text (another agent platform, a completion endpoint), implement the
`TextBackend` trait (`src/backends/mod.rs`) as `StackSpotBackend` does: `stream(prompt, target, meta)` (text items,
then one `Completion` with usage when known), `ready()` (credentials and network, without spending model quota),
`validate()`, `input_limit_exceeded()`, `describe_target()` and the queue in front of it (`limiter()`); then dispatch
on its `type` in `create_backend` and list it in `BACKEND_TYPES`. The emulation engine holds an
`Arc<dyn TextBackend>`, so telemetry and all three protocols work unchanged; the queue (`UpstreamLimiter`) and the
retry policy are building blocks the backend calls itself, as `StackSpotBackend::open` does. Cover it in `tests/`
with a scripted stand-in, as `tests/common/mod.rs` does for StackSpot.

### Adding a backend with a native chat API

For a service that already has messages and tools (OpenAI, Anthropic, Bedrock, Ollama), the emulation layer should
be skipped: give the gateway a second kind of runner that maps the canonical request to the service's own API, next
to `EmulationEngine` (`Gateway::runners`). This is the planned extension point; no native backend exists yet.

## Requests and prompts

Each protocol adapter decodes the request body into its own typed request (`serde`) in one pass, straight from the
bytes: a field of the wrong type is a 400 that names it (`invalid request: messages[2].content: ...`), and unknown
fields are ignored. Shapes with alternatives (a string or a list of parts, a mode or a named tool) have hand-written
visitors, because serde's `untagged` and `flatten` would buffer every message first. Before decoding, `json.rs` turns
a lone UTF-16 surrogate escape into U+FFFD and `NaN`/`Infinity` into `null`; numbers keep their digits
(`arbitrary_precision`). The Responses settings a response echoes are kept as the client's raw JSON. Parameters a text backend cannot honor are accepted and reported once per client in the log
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

`tests/` is a black-box regression suite (`cargo test --profile ci`, as CI runs it): each test starts the binary
Cargo just built as a process, in front of a scripted StackSpot stand-in (an axum server inside the test,
`tests/common/mod.rs`), and talks to it only over HTTP, so it checks exactly what clients see: request and response
shapes of the three protocols, SSE event sequences and keepalives, follow-ups, retries and error mapping, the
Responses store on disk, the CLI, configuration errors and graceful stops, and the OTLP export (decoded with the
official protobuf definitions). Unit tests sit next to the code they test, with property tests (proptest) for the
parsers of untrusted input: any chunking of a model's output gives the same calls and text, any JSON body is answered
without a panic. Opt-in tests, for material kept out of the repository: `--test mitm -- --ignored` (the
observability proxy streams SSE), `--test replay -- --ignored` (real client requests, `MIDIR_CAPTURES=<dir>`) and
`--test followups_corpus -- --ignored` (labeled model replies, `MIDIR_PROMISE_CORPUS=<dir>`).
