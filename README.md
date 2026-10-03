# Midir

**An LLM gateway for agent platforms.** One local endpoint that speaks **OpenAI Chat Completions**, **OpenAI
Responses** and **Anthropic Messages**, so agent and coding clients (Claude Code, GitHub Copilot, Hermes Agent,
DeepSeek Harness, OpenClaw) use the models behind it as if they were native providers.

Backends are pluggable. Today the one backend is **StackSpot AI**: its Agent API takes one text prompt and streams text
back, so Midir emulates everything else on top of it: conversation roles, streaming in each protocol's own events,
tool calling (including parallel calls), `tool_choice`, structured JSON output, stop sequences, `previous_response_id`.
The request's `model` picks the model, and each model points to a backend and a target (a StackSpot agent), so
several LLMs (GPT 5.1, GPT 4.1, StackSpot's Flex) are served side by side.

> **Why "Midir"?** In Irish mythology, Midir is a king of the Tuatha Dé Danann who lives in the Otherworld and crosses
> into the world of mortals; one of the tasks set for him is building a causeway across a bog, a road where there was
> none. Midir does the same between clients and agent platforms that do not speak the same language.

## Quick start

You need a StackSpot client key (client id and secret of your realm) and at least one agent configured as in
[docs/backends/stackspot.md](docs/backends/stackspot.md).

```bash
git clone https://github.com/helton/midir && cd midir
cp .env.example .env                                   # credentials and agent ids
cp config/midir.example.toml config/midir.toml         # backends and models
mkdir -p docker/data/gateway
docker compose -f docker/compose.yml up -d             # pulls ghcr.io/helton/midir (public, ~3 MB, amd64 and arm64)
curl -s http://127.0.0.1:18880/ready                   # {"ok": true, ...} once the credentials work
```

Then point a client at `http://127.0.0.1:18880/v1` (Anthropic clients: `http://127.0.0.1:18880`) with any API key; see
[Clients](#clients).

**From source** (Rust 1.81+; Midir is a single static binary):

```bash
cargo build --release      # target/release/midir
./target/release/midir     # reads .env and config/midir.toml from the current directory; --help for the options
```

**Docker, building the image locally** (with or without the observability stack):

```bash
cargo xtask deploy standalone  # Midir only
cargo xtask deploy full        # Midir + mitmproxy + Grafana LGTM
```

## Ports

Everything the stack publishes lives in the block **18880-18889**, bound to 127.0.0.1:

| Port | What |
|---|---|
| 18880 | Midir; point clients here (through mitmproxy when the observability overlay is on) |
| 18881 | Midir directly (overlay only) |
| 18882 | mitmweb, `http://127.0.0.1:18882/?token=gateway`: every client request and every backend call |
| 18883 | Grafana, dashboard **midir** (no login) |
| 18884 | OTLP/HTTP, for Midir running outside Docker |

Container data lives in `docker/data/`, one folder per service: `gateway` (the `previous_response_id` store),
`mitm` (proxy CA) and `lgtm` (Grafana, Prometheus, Tempo). The Midir container runs as your user, never root.

## Configuration

Two files at the repository root (or the working directory):

- **`config/midir.toml`** (copy [config/midir.example.toml](config/midir.example.toml)): `[server]` (port, prompt
  cap, tool-protocol knobs, Responses store), `[telemetry]`, the **backends** (`[backends.<name>]` with its `type`,
  its options and `[backends.<name>.limits]`) and the **models** clients see (`default_model` and one `[[models]]`
  entry each).
- **`.env`** (copy [.env.example](.env.example)): secrets and agent ids only, referenced from the TOML. Never committed.

```toml
default_model = "gpt-5.1"

[backends.stackspot]
type = "stackspot"
realm = "${STACKSPOT_REALM}"
client_id = "${STACKSPOT_CLIENT_ID}"
client_secret = "${STACKSPOT_CLIENT_SECRET}"

[[models]]
name = "gpt-5.1"
backend = "stackspot"                     # optional while there is one backend
target = "${STACKSPOT_GPT_5_1_AGENT_ID}"  # StackSpot: the agent id
aliases = ["claude-opus-4-5"]             # extra exact names some clients send
# match = "^(openai/)?gpt-5"              # optional regex
# tail_reminder = true                    # per-model knobs override [server]
```

**Routing** of a requested `model`: exact name or alias (case-insensitive) > `match` regex (file order) > exact after
a provider prefix (`openai/gpt-4.1`, `stackspot-gpt-4.1`) > longest configured name contained in it > `default_model`.
`GET /v1/models` lists the models; `GET /health` shows the mapping, the loaded file and each backend's queue.

**Environment wins over the file**, so Docker and other machines override without editing it: `MIDIR_PORT`,
`MIDIR_CONFIG` (path of the TOML), `MIDIR_REQUESTS_PER_MINUTE`, `MIDIR_MAX_CONCURRENT`, `OTEL_EXPORTER_OTLP_ENDPOINT`,
the StackSpot backend's `STACKSPOT_REALM`, `STACKSPOT_CLIENT_ID`, `STACKSPOT_CLIENT_SECRET`, `STACKSPOT_CA_BUNDLE`, and
the rest listed in `midir --help`. In Docker, `config/midir.toml` is injected as a compose config: after editing it,
run `docker compose -f docker/compose.yml up -d --force-recreate midir`. A configuration in the pre-0.0.1 layout
(`[stackspot]`, `[[agents]]`) is still read, with a warning.

## Clients

Ready configurations in [examples/clients/](examples/clients/) (with a README where there is more to say):

| Client | API used | Files |
|---|---|---|
| Claude Code | Anthropic Messages | `claude-code/settings.json`: run `claude --settings examples/clients/claude-code/settings.json` (Opus, Sonnet and Haiku slots mapped to gpt-5.1, flex and gpt-4.1; also overrides a corporate Bedrock/Vertex setting) |
| GitHub Copilot (VS Code) | OpenAI Responses | `copilot/chatLanguageModels.json` (one provider, all models) |
| Hermes Agent | Chat Completions | `hermes/config.yaml` |
| DeepSeek Harness (`dsh`) | Chat Completions | `deepseek-harness/cordis.patch.yml` |
| OpenClaw | Chat Completions or Responses | `openclaw/openclaw.json` |

Validated end to end on the three models with Claude Code, GitHub Copilot CLI, Hermes Agent, DeepSeek Harness and
OpenClaw (multi-file edits, tests, commits, and tool arguments with quotes, backslashes and tabs); GitHub Copilot in
VS Code uses the same OpenAI APIs and is in daily use with the configuration above. Midir needs no API key; clients
may send any value. For agents running on top of it, [docs/agent-brief.md](docs/agent-brief.md) explains what the
emulation means for them.

## Endpoints

| Method and path | What |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions, streaming or not |
| `POST /v1/responses`, `GET /v1/responses/{id}` | OpenAI Responses, streaming or not, `previous_response_id` |
| `POST /v1/messages`, `POST /v1/messages/count_tokens` | Anthropic Messages, streaming or not |
| `GET /v1/models` | configured models |
| `GET /health` | version, model mapping, backends and their queues (liveness) |
| `GET /ready` | each backend's readiness: credentials, network and TLS, without spending model quota (503 on failure) |

## How faithful the emulation is

**Native-like**: all message roles; function tools with schemas, parallel calls and results; `tool_choice`
auto/none/required/named; streaming in each protocol's own event format; real token usage; errors in each protocol's
format; `previous_response_id` chains that survive restarts (30 days, 500 MB cap).

**With caveats** (search the code for `CAVEAT`):
- Tool calling is prompt-based: `<tool_call>` blocks are parsed from the model's text. A call with invalid JSON gets
  one hidden follow-up and is never passed on broken; a reply that only announces an action, denies an ability a tool
  provides, or leaves pending an action the request ordered gets one follow-up appended to the same response.
- Structured JSON is prompt + validation + one repair, not streamed incrementally.
- `max_tokens` and `stop` are applied after generation; `count_tokens` is an estimate (4 chars per token).
- While a stream waits for the backend's first content (it can take a minute), an SSE keepalive goes out every 15 s
  (`[server] keepalive_s`) so clients and proxies with idle timeouts do not abort.
- Above the prompt cap the oldest turns are dropped and the model is told; a backend refusal for input length is
  retried once with a proportionally smaller prompt.
- Images, audio and files become a text placeholder; built-in provider tools (web search, ...) are omitted.
- **Refused**: `logprobs`, `n > 1`, `/v1/embeddings`. **Accepted and ignored** (logged once per client): `temperature`,
  `top_p`, `seed`, `reasoning`, `reasoning_effort`, `thinking`, `cache_control`, `metadata`.

How the pieces fit, and how to add a backend: [docs/architecture.md](docs/architecture.md).

## Limits (StackSpot)

- Input up to **272,000 tokens**.
- **100 requests per minute per account**, shared by every agent and client. Midir queues instead of failing
  (`[backends.stackspot.limits]`: 8 concurrent, 90 per minute by default); a backend 429 pauses new requests and
  halves the local budget, which recovers by one request per minute, so a second Midir or StackSpot's own chat on the
  same account is absorbed. Midir's own 429s carry `Retry-After`.
- GPT 5.x counts its reasoning as output tokens; time to first byte is 1.5-8 s depending on prompt size and model.

More in [docs/backends/stackspot.md](docs/backends/stackspot.md).

## Telemetry

With `OTEL_EXPORTER_OTLP_ENDPOINT` set (the observability overlay does it), each request produces one OpenTelemetry
span and a few metrics (`midir.*`): tokens, duration, time to first byte, model and backend, client (`claude-code`,
`copilot`, `hermes`, `openclaw`, `deepseek-harness`, ...), session, tool calls, follow-ups, retries, queue waits,
truncations. Prompt content is never exported. The Grafana dashboard is provisioned from [docker/grafana/](docker/grafana/).

## Security

Binds to `127.0.0.1` and has **no authentication**: local use only; put something in front of it before exposing it.
Everything a client sends (open files, terminal output) goes to the backend, as it would to any hosted LLM. Prompts
are logged only with `--debug`, to stderr. The only data written to disk is the Responses store
(`docker/data/gateway/responses`, or `data/gateway/responses` from source): conversation content in owner-only files
(0600, folders 0700), kept 30 days, capped at 500 MB, not encrypted. `responses_dir = ""` keeps it in memory only, and
so does a folder that cannot be created (a container started without its data volume), with a warning in the log.

## Development

Everything is Rust: you need Rust 1.81+ (and Docker for the images). The tests are black-box: each starts the
binary Cargo built as a process, in front of a scripted StackSpot (nothing leaves the machine), and checks what clients
see over HTTP. Repository tasks are a workspace member, `xtask`, run through a Cargo alias.

```bash
cargo run --release                         # build and run from source (reads .env and config/midir.toml)
cargo test --release                        # unit tests + the offline regression suite (~1 min)
cargo clippy --all-targets -- -D warnings   # lints, as CI runs them (also: cargo fmt --check)
cargo xtask smoke                           # live acceptance against a running Midir (25 checks, real backend requests)
cargo xtask check-leaks                     # scan what git would publish for secrets, agent ids, home paths, e-mails
cargo xtask                                 # all tasks
```

The one file in another language is `docker/mitm/sse_stream.py`: a mitmproxy addon for the observability stack
(mitmproxy only loads Python addons); `cargo test --release --test mitm -- --ignored` checks it.

| Path | Contents |
|---|---|
| `src/` | the gateway: `protocols/`, `emulation/`, `backends/`, `gateway.rs`, `app.rs`, `config.rs`, `store.rs`, `telemetry.rs`, `main.rs` ([architecture](docs/architecture.md)) |
| `tests/` | black-box regression suite: the binary as a process, a scripted backend (`tests/common/`) |
| `config/` | `midir.example.toml` (your `midir.toml` is not versioned) |
| `docker/` | `Dockerfile`, compose files, Grafana provisioning, `data/` (runtime, not versioned) |
| `examples/clients/` | client configurations |
| `docs/` | architecture, backend notes, agent brief, changelog |
| `xtask/` | repository tasks (`cargo xtask`): version, bump, check-leaks, smoke, deploy |
| `.github/workflows/` | CI with snapshots (`develop`) and releases (`main`) to ghcr.io |

**Branches and images**: work happens on `develop`; `main` only receives releases. Tags `vX.Y.Z` are created by the
release workflow, never by hand.

| Push to | Workflow | Image on `ghcr.io/helton/midir` (amd64 + arm64) |
|---|---|---|
| `develop` | [ci.yml](.github/workflows/ci.yml): tests, then a snapshot | `dev` (the latest develop build, may break) and `sha-<commit>` |
| `main` | [release.yml](.github/workflows/release.yml): tests, image, tag `vX.Y.Z`, GitHub release from the CHANGELOG | `X.Y.Z`, `X.Y`, `latest` (the latest release) |

A push to `main` whose version was already released publishes nothing. To release, on `develop`:

```bash
cargo xtask bump [patch|minor|major|X.Y.Z]  # Cargo.toml, Cargo.lock and the default image tag in docker/compose.yml
# rename the "## Unreleased" section of docs/CHANGELOG.md to "## X.Y.Z (date)", then commit
git push                                     # snapshot of the release candidate
git checkout main && git merge --ff-only develop && git push && git checkout develop
```

Every build says what it is (`midir --version`, the startup banner, `GET /health`): a release reports the bare version
(`0.1.0`); anything else carries the commit as semver build metadata, `0.1.0+dev.a817822` for a `develop` snapshot,
`0.1.0+local.a817822` for an image built here (`.dirty` with uncommitted changes) and `0.1.0+src.a817822` for a
binary built from a git checkout.

`docker/compose.yml` runs the release it was bumped to; a snapshot runs with
`MIDIR_IMAGE=ghcr.io/helton/midir:dev docker compose -f docker/compose.yml up -d`. The version lives only in
`Cargo.toml` (`midir --version`, `GET /health`, first log line); changes are listed in
[docs/CHANGELOG.md](docs/CHANGELOG.md).

## Troubleshooting

- **`/ready` answers 503**: a backend's readiness check failed; the message says why (StackSpot: realm, client id or
  secret, network, CA).
- **403 with an empty body on requests**: an agent id is wrong or not shared with the client key; `GET /health` shows
  which target each model maps to.
- **429**: the account's 100 requests per minute were exceeded (several clients or Midir instances on one account).
- **Client says the model does not support tools or images**: declare tool calling on and vision off (see the client
  examples); images become placeholders on purpose.
- **Log times in UTC**: containers run in UTC; set `TZ` in `.env` (for example `TZ=America/Sao_Paulo`) and recreate
  the container. Running from source uses the machine's time zone.
- **Corporate TLS**: set `STACKSPOT_CA_BUNDLE` to a PEM bundle with the corporate CA (it replaces the built-in roots);
  Cargo itself honors `CARGO_HTTP_CAINFO`.
- **Corporate proxy**: Midir honors `HTTPS_PROXY`/`https_proxy` and `NO_PROXY`/`no_proxy`; when both spellings are
  set, the uppercase one wins.
- **Pulling the image**: `ghcr.io/helton/midir` is public, no login needed; for a registry mirror or a fork, set
  `MIDIR_IMAGE=<registry>/midir:<tag>` in the shell or `docker/.env`.
- **Name resolution behind a corporate proxy**: the image's binary is static (musl's resolver). If names resolve
  differently than on the host, run a binary built for the host from source (`cargo build --release`).

## License

[MIT](LICENSE).
