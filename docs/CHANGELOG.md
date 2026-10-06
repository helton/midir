# Changelog

All notable changes are listed here. Versions follow [Semantic Versioning](https://semver.org/); before 1.0.0 a minor
release may change configuration or behavior.

## Unreleased

Fixes from the 2026-10-05 review (`.internal`, findings F-numbered):
- **Follow-ups never act for the user**: an announcement that leaves the action to the user's approval ("I'll push as
  soon as you confirm", "assim que você confirmar"), or that names push, merge, deploy, install or an action the
  user's latest message forbids, no longer gets a follow-up that appends the call (it appended a `git push`). The
  forgotten-commit follow-up only applies to the task of the current instruction (it committed during a later,
  unrelated answer). The loop guard compares whole calls: a second write to the same file with new content is no
  longer dropped as a repeat. A follow-up whose backend call fails leaves the reply it followed (it turned a delivered
  answer into an error); a JSON reply cut by `max_tokens` is no longer repaired under the same cap.
- **Streaming errors carry an HTTP status**: a streaming request that fails before its first byte (a backend 429
  that outlasted the retries, a full queue, a refusal) answers the protocol's HTTP error with `Retry-After`, as a
  non-streaming one does, so SDKs retry; it was a 200 with an error event. The first answer is awaited for up to four
  keepalive intervals; a slower backend gets the 200 and keepalives as before.
- **A burst of 429s halves the local budget once**: requests in flight that hit the account's limit together counted
  as one halving each (90 → 10 requests per minute for over an hour); they now extend the pause. The backend's
  `Retry-After` sets the pause when longer than the cooldown (up to 5 minutes).
- **Connections that never finish their request are closed**: headers must arrive within `[server] read_timeout_s`
  (30 s, `MIDIR_READ_TIMEOUT_S`) and a body may not stall longer between chunks; at most 4096 connections are open at
  once. Before, a half-sent request held its connection forever.
- **Tool output is data**: tags that look like the tool protocol inside a tool result (a file or page that documents
  it, Midir's own sources) are shown to the model with `‹` instead of `<`, and the protocol says so; such a result
  could close itself early or pass for a call. **This changes the prompt** for results that hold those tags.
- **Prompts fit the cap with large recent calls**: when the recent turns alone exceed the prompt cap, the largest
  assistant texts and call arguments (a file's new content, a long command) are cut in the middle too, after tool
  results and before user messages; a 200,000-character argument kept the prompt above the cap.
- **Fewer and better follow-ups**: closings such as "vou ficar à disposição" and answers that open with "let me
  explain" no longer cost a hidden call; a reply that says a file does not exist (or that ran `cat`/`curl` itself) is
  taken as reporting a failure; a numbered plan without a header line is recognized as an announcement (the labeled
  corpus: 59 of 64 promises caught, from 55, still no false alarm).
- **Parser**: a `<tool_call>` tag quoted in the model's prose ("use the `<tool_call>` tag") stays text (the middle of
  the sentence was lost and a repair fired); text before a call is the same whatever the chunking (a trailing newline
  came through only when the text arrived in its own delta); a broken call whose arguments contain a "name" key is
  salvaged under the call's own name.
- **Responses**: text that follows a tool call is its own message item in the stored response and in `GET`, with the
  ids it streamed with, so `item_reference` resolves it (it was merged into the first message, and referencing it was
  a 400); `conversation` (server-side state) is refused with a 400 instead of being ignored; `max_tool_calls` and
  `prompt` are logged as ignored.
- **Errors**: an error in the middle of a stream keeps its type (a 429 is a `rate_limit_error`, not an `api_error`)
  and its code, and Responses' error event continues the stream's sequence numbers; Anthropic error bodies carry
  `request_id`; a body over the size limit is a 413 in the protocol's format (it was plain text); a forced
  `tool_choice` without tools is a 400 (it was answered as plain text); `GET /v1/models/{id}` answers 404 for a name
  no model answers to; an empty reply is an empty content list for Anthropic clients; a request the client leaves is
  logged as cancelled.
- **Operations**: metric labels no longer grow per request for stateless clients (a session made up for one request
  is labeled "-", and the client-sent model name stays on the spans); spans carry the backend's type as
  `gen_ai.provider.name` and its configured name as `midir.backend`, and the dashboard's traces panel no longer
  depends on the backend being called `stackspot`; `cargo xtask deploy` runs the container as the checkout's owner,
  the README shows how for compose, and `/ready` lists a store that fell back to memory under `warnings`; the
  compose service runs with a read-only file system, no capabilities and `no-new-privileges`.
- **Supply chain and releases**: GitHub Actions are pinned to commit SHAs, the build image by digest, Grafana LGTM
  to a version, and Dependabot also watches the compose files; CI checks the pins and that the image builds with the
  Rust of `rust-toolchain.toml`; `cargo xtask check-leaks` catches credentials in any case, JWT- and base64-shaped
  values and random-looking quoted values in configuration files; release notes link to the docs of their version,
  `latest` only moves to the highest version, and a missing CHANGELOG section no longer blocks the `dev` image.
- **Hardening**: settings outside their range stop startup with a message that names them (a negative retention
  deleted every stored response; a zero prompt cap dropped every history turn); at most `limits.max_waiting` requests
  (64) wait for a slot, beyond which a new one gets a 429 with `Retry-After` at once; an SSE body with no line break
  for 8 MB ends the request with a 502 (it was buffered whole); StackSpot's 401 and 403 reach clients as a 502
  `upstream_401`/`upstream_403` instead of a 401, which SDKs read as their own key being wrong.
- **Responses store**: retention applies when a response is read from disk, not only at the hourly purge (an expired
  response was served until then); writes are flushed to disk before the rename, and a blob left empty by a crash is
  written again (it broke every chain that shared it); the size purge removes a response together with its
  continuations (it removed a chain's small first response and left the rest unreadable); ids stay unique even if
  the system's random source fails.
- **Token renewals are shared**: a wave of 401s renews the StackSpot token once, and a failed renewal is answered
  again for 5 s, so an open `/ready` polled in a loop no longer posts the client secret to idm on every hit.
- **Telemetry follows the OpenTelemetry conventions more closely**: the request span is named `chat <model>` and
  carries `http.request.method`, `http.route` and `url.path`; a client trace whose `traceparent` is not sampled gets
  no spans (they were orphans in the collector); `service.name` from `OTEL_RESOURCE_ATTRIBUTES` is kept unless
  `OTEL_SERVICE_NAME` is set; an `OTEL_EXPORTER_OTLP_PROTOCOL` other than `http/protobuf` is logged as unsupported;
  new histograms `gen_ai.server.request.duration` and `gen_ai.server.time_to_first_token` (seconds, few labels) sit
  next to the `midir.*` ones the dashboard reads.
- **Compact tool listing** (`tool_schema = "compact"`, per model or `MIDIR_TOOL_SCHEMA`; default `json`): each tool
  as a `### name` heading, its description and one line per parameter (`- path (string, required): ...`) instead of
  its JSON Schema; what that form cannot say stays JSON Schema. Descriptions are kept whole, so the gain is the schema
  syntax: Copilot's 75 tools take 11.5% fewer tokens (about 2,700 per turn, 8% of the prompt), Claude Code's 10%.
  `count_tokens` estimates with the requested model's settings.
- **Tests**: black-box tests for the queue timeout and the rate window, 405 and trailing slashes, `/metrics` behind
  the API key, escaped metric labels and the `/ready` failure body; the shutdown test waits for its request instead
  of sleeping. The docs and `--help` match the code again (the OTEL variables, metric names, `retry_backoff_s`).

- **Documentation**: a new README (logo, diagram, highlights, quick start, measured performance) and the reference
  material in guides: [configuration](configuration.md), [API compatibility](compatibility.md),
  [operations](operations.md) and [development](development.md).

## 0.1.1 (2026-10-05)

- **No more patches written as text**: the tool protocol now tells every model that text changes nothing: to create
  or edit a file it calls a tool that writes it, never a patch or diff in the reply (`*** Begin Patch`, `diff --git`).
  GPT-4.1 sometimes wrote the change as an apply_patch block, ran the old tests and reported the work done (hermes,
  battery 2026-10-04); three reruns of that task wrote no patch as text.

## 0.1.0 (2026-10-04)

- **Rewritten in Rust**: Midir is now one static binary (tokio, axum, reqwest with rustls) instead of a Python
  package. What clients and models see is unchanged except where this list says so: endpoints, configuration
  (`config/midir.toml`, `.env`, every `MIDIR_*`/`STACKSPOT_*`/`OTEL_*` variable), the prompts sent to the backend
  (byte for byte on 237 captured client requests), SSE event sequences, the Responses store on disk (chains created by
  0.0.1 keep working), telemetry and the log's messages (lines now read `HH:MM:SS LEVEL module: message`). Measured
  against a local mock with the largest captured Copilot request (230 KB): startup 774 ms -> 17 ms, idle memory
  48 MB -> 11 MB, 69 -> 865 requests per second with 32 concurrent clients (p99 1.35 s -> 74 ms); image 81 MB ->
  12 MB (30 MB -> 5 MB to download), `FROM scratch`, health check with `midir --healthcheck`. From source:
  `cargo build --release` (Rust 1.99, pinned in `rust-toolchain.toml`).
- **Security**: an optional API key (`MIDIR_API_KEY` or `[server] api_key`): every endpoint but `/health` and `/ready`
  then wants `Authorization: Bearer <key>` or `x-api-key: <key>`, and Midir notes in the log when it listens beyond localhost
  without one. TLS to backends uses the system's trust store (and `SSL_CERT_FILE`); `STACKSPOT_CA_BUNDLE` adds a
  corporate CA to it instead of replacing the public roots. The StackSpot token call gives up after 30 s instead of
  holding every request. Model output, which may hold secrets, no longer reaches the log at the default level:
  problems are described by size and position, the content goes to DEBUG.
- **What JavaScript and Python clients write**: a lone UTF-16 surrogate escape (JavaScript writes one when a string is
  cut in the middle of an emoji; one truncated tool output made every later request of the session fail with 400)
  becomes U+FFFD, in requests and in the backend's stream; `NaN` and `Infinity` (Python's `json.dumps`) become `null`;
  numbers keep their digits (big integers in tool arguments were rounded); `null` where a list is expected counts as
  absent.
- **Follow-ups never act against the user**: a confirmation question is answered for the user only for a commit or a
  test run their most recent instruction explicitly orders ("faça um commit", "commit the changes", "rode os
  testes"): not one they forbid, before or after the mention ("não faça commit", "commit não", "skip the commit"),
  make conditional ("só se os testes passarem"), keep for themselves ("eu mesmo faço o commit") or only mention
  ("revise o último commit"), in Portuguese, English and Spanish; text clients inject (Claude Code's reminders, VS
  Code Copilot's workspace context) does not count. Push, merge, deploy and install are never confirmed for the user,
  and neither is a question that bundles them with something else. A reply that reports an ordered
  action as next ("Pronto para commit") gets its follow-up even after "Nada pendente"; the ability follow-up fires
  only when a listed tool provides the ability the reply denies (never for "I can't see your bank account"); a reply
  that announces again after a follow-up gets a second one, at most two per turn. Found in the real-client runs:
  git trailers at the end of a reply (`Co-authored-by:`, which Copilot asks for) no longer hide the sentence that
  announces the commit; a question split by colons ("Deseja uma mensagem ou uso algo como: \"feat: ...\"?") and an
  offer of the ordered action ("Se quiser o commit, só avisar") count as asking; an action a tool call already did
  (a `git commit` since the order) is not asked for again; and a final report that leaves out the commit the user
  ordered ("Tudo pronto! ... 14 passed", no commit made, no failure reported) gets a follow-up asking for it.
- **Follow-ups never loop**: a reply that says it cannot reach something after a call in the same turn already tried
  (VS Code Copilot's `fetch_webpage` answering 403 on PyPI), or that cites the failure (a 403, a timeout), reports what
  the tool returned. The ability follow-up treated it as a false incapacity and appended the same fetch to every reply,
  so the client ran it again, round after round, until the user stopped it. And a follow-up never makes again a call
  whose last two runs in the turn returned the same result. `followups = false` in `[server]` or a model
  (`MIDIR_FOLLOWUPS=0`) turns these heuristic follow-ups off; a forced `tool_choice` is still asked for again.
- **Tool calls**: JSON with raw newlines or tabs inside strings, or with trailing commas, is read as the model meant
  it instead of costing a repair round trip; a call whose arguments hold the text `</tool_call>` (writing a file that
  documents the protocol) is no longer cut there: a block ends at the first close tag outside a JSON string; JSON mode with tools returns the model's tool calls (they were dropped);
  a forced `tool_choice` is retried when streaming too; text held back for a stop sequence comes out before a tool
  call, not after it; `parallel_tool_calls: false` and Anthropic's `disable_parallel_tool_use` are honored (the model
  is told, extra calls are dropped); a named `tool_choice` that names no declared tool is a 400; `max_tokens` holds
  even when a stop sequence comes later in the same chunk; with overlapping stop sequences the one completed first
  ends the text, however the text arrives, and an empty stop sequence is ignored.
- **Long conversations**: when dropping old turns is not enough (one huge tool result in the last turn), the largest
  tool results, then the largest user messages, are cut in the middle, keeping head and tail and saying how much
  went, so the session no longer stays stuck on the backend's size limit; the cut is computed in linear time (a 1.8 MB
  conversation took 0.55 s).
- **Responses API**: `GET /v1/responses/{id}` answers the model asked for and the echoed settings (it said `default`
  and `null`), with the same item ids as the original answer (item ids derive from the response id); `item_reference`
  input items are resolved from the store (an unknown one is a 400; they were dropped); a stream cut by
  `max_output_tokens` ends with `response.incomplete`.
- **Responses store**: a response shares its history with the response it continues (memory grew with the square of
  a chain's length: 120 steps of 20 KB took 157 MB); the memory cache is capped at `[server] responses_memory_mb`
  (64 MB, least recently used out first; misses are rebuilt from disk) and `/health` reports it (`responses_cache`);
  disk reads and writes run off the request threads; above `responses_max_mb` the blobs no response uses go first;
  files left by interrupted writes are removed.
- **Chat Completions streaming reports usage as OpenAI does**: only with `stream_options.include_usage`, in a last
  chunk with `"choices": []`, every other chunk carrying `"usage": null`. Before, usage came in the finish chunk by
  default.
- **Anthropic Messages**: `message_start` carries the estimated input tokens (clients track context use from it; the
  backend's count follows in `message_delta`); `GET /v1/models` answers in Anthropic's format to clients that send
  `anthropic-version`.
- **Operations**: a stop (SIGTERM) stops accepting connections, lets in-flight requests finish for up to
  `[server] shutdown_grace_s` (25 s; compose waits 30 s), then flushes telemetry (streams were cut after 2 s); a bug
  (panic) fails only its own request, with a 500 or an error event, instead of every stream of the process; unknown
  keys in the configuration and unknown backend options are reported at startup (`requests_per_minut = 5` was
  silently ignored); the queue's count of waiting requests no longer grows when a client gives up while queued;
  `midir --healthcheck` reads a port written as a string or `${NAME}`.
- **SSE keepalive** whenever a stream is idle: an SSE comment (`: keepalive`) in Chat Completions and Responses, a
  `ping` event in Anthropic Messages, every `[server] keepalive_s` (15 s; `MIDIR_KEEPALIVE_S`, 0 turns it off), while
  the backend thinks and while a follow-up call runs after text was streamed. Backends took up to 97 s to start in
  the 2026-10-03 harness runs; clients and proxies with shorter idle timeouts would abort the stream.
- **Observability**: request ids in `x-request-id` (OpenAI) and `request-id` (Anthropic); a request span of kind
  SERVER, the child of the client's `traceparent` when it sends one, with a CLIENT child span per backend call
  (prompt size, time to first byte, usage); `gen_ai.provider.name`; duration and time-to-first-byte buckets up to
  300 s (they stopped at 10 s); metric series idle for an hour are dropped (they were kept, and exported, for the
  life of the process); `GET /metrics` in Prometheus format with `[telemetry] prometheus = true`
  (`MIDIR_PROMETHEUS`); `OTEL_EXPORTER_OTLP_HEADERS` values are URL-decoded; a log filter in `MIDIR_LOG`
  (`info,midir::store=debug`) and JSON log lines with `MIDIR_LOG_FORMAT=json`.
- **Performance**: request bodies are decoded once, straight into typed requests; the Responses settings are echoed as
  the client sent them, without decoding and encoding them again; history and tool lists are shared between a request,
  its follow-ups and the store; the binary uses jemalloc (built for 4 to 64 KiB memory pages on arm64). Against a local mock with the largest captured Copilot
  request (230 KB) and 32 concurrent clients, the image went from 131 ms of CPU per request (musl's allocator) to about
  8 ms, and from 164 to about 900 requests per second (865 to 961 across runs).
  Memory under load: 4 async worker threads by default (`TOKIO_WORKER_THREADS` changes it) instead of one per core,
  each with its own allocator arena, and jemalloc gives memory back about a second after a burst. With 32 concurrent
  large requests on a 32-core machine the peak went from about 300 to 104 MB and, 15 seconds later, from about 300 to
  32 MB, for 7 to 10% fewer requests per second (still over 640 per second).
- **Clearer errors for malformed requests**: each protocol decodes the body into typed requests, so a field of the
  wrong type is a 400 that names it (`invalid request: messages[2].content: invalid type: integer 5, expected a string
  or a list of content parts`); unknown endpoints and methods answer 404/405 in the protocol's error format; a
  trailing slash is ignored (`/v1/chat/completions/`); an internal error is a JSON error, not a bare 500. JSON output
  (responses, SSE events, JSON mode) is compact.
- **Configuration**: numbers and booleans may also be strings (`port = "${PORT}"`); an invalid value names the key
  and the variable. `match` patterns use the `regex` crate's syntax (no lookaround or backreferences). New settings:
  `[server] api_key`, `shutdown_grace_s` and `responses_memory_mb`, `[telemetry] prometheus`.
- **Tests and tooling in Rust**: the regression suite is black-box (`cargo test`: the binary as a process against a
  scripted StackSpot) plus unit and property tests (any chunking of a model's output, any JSON body), with opt-in
  checks for the observability proxy, a replay of real client requests and a labeled corpus of real model replies.
  Repository tasks moved to `cargo xtask` (version, bump, check-leaks, smoke, deploy). CI runs fmt, clippy, the suite
  and cargo-deny (advisories, licenses, sources); Dependabot proposes updates; images carry an SBOM and provenance.
  The codebase no longer needs Python.
- **Trunk-based releases**: everything lands on `main` (the `develop` branch is retired). Every push publishes
  `ghcr.io/helton/midir:dev` and `:sha-<commit>`; a push whose version has no tag yet publishes that same build as
  `X.Y.Z`, `X.Y` and `latest` and creates `vX.Y.Z` with its GitHub release. Run a version or `sha-<commit>`, not `dev`
  or `latest`: a registry mirror can keep serving an old build under a tag that moves.
- **Streaming through the observability stack fixed**: mitmproxy buffered whole responses, so on port 18880 tokens
  and keepalives reached clients only when generation was over, and StackSpot's stream reached Midir the same way.
  An addon (`docker/mitm/sse_stream.py`) streams `text/event-stream` responses through in both directions and keeps a
  copy for mitmweb; `cargo test --release --test mitm -- --ignored` checks it. The overlay sets both spellings of the
  proxy variables (`HTTPS_PROXY`/`https_proxy`, `NO_PROXY`/`no_proxy`).
- **Telemetry fixed**: request spans and metrics are flushed when the process stops (SIGTERM), and only Midir's own
  spans are exported (0.0.1 also exported a span for every health check and internal step under
  `unknown_service:python`).
- **Every build says what it is**: only a release reports the bare version; another build of `main` is
  `0.1.0+dev.<commit>`, a local image `0.1.0+local.<commit>`, a binary built from a git checkout
  `0.1.0+src.<commit>` (`.dirty` with uncommitted changes), shown by `midir --version`, the startup banner,
  `/health`, `/ready` and telemetry.
- **Quieter log**: parameters accepted without effect (Hermes sends `reasoning_effort` on every request: 64 of 74
  warnings in that run) are reported once per client and set of parameters, at INFO; repeats go to DEBUG.
- **No crash without the data volume**: when the Responses store folder cannot be created (for example `docker run`
  without mounting `/data`), Midir logs a warning and keeps `previous_response_id` in memory instead of exiting.
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
