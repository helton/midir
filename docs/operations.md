# Operations

Running Midir day to day: ports, images, observability, logs and troubleshooting.

## Ports and data

Everything the Docker stack publishes lives in the block **18880-18889**, bound to 127.0.0.1:

| Port | What |
|---|---|
| 18880 | Midir; point clients here (through mitmproxy when the observability overlay is on) |
| 18881 | Midir directly (overlay only) |
| 18882 | mitmweb, `http://127.0.0.1:18882/?token=gateway`: every client request and every backend call |
| 18883 | Grafana, dashboard **midir** (no login) |
| 18884 | OTLP/HTTP, for Midir running outside Docker |

```bash
docker compose -f docker/compose.yml up -d                                         # Midir alone
docker compose -f docker/compose.yml -f docker/compose.observability.yml up -d     # + mitmproxy and Grafana LGTM
cargo xtask deploy standalone|full                                                 # the same, image built here
```

Container data lives in `docker/data/`, one folder per service: `gateway` (the `previous_response_id` store), `mitm`
(proxy CA) and `lgtm` (Grafana, Prometheus, Tempo). The Midir container runs as your user, never root: compose reads
`UID` and `GID` from `docker/.env` (`printf 'UID=%s\nGID=%s\n' "$(id -u)" "$(id -g)" > docker/.env`; `cargo xtask
deploy` passes them itself), falling back to 1000:1000. When the data folder is not writable, Midir keeps responses in
memory only, says so in the log, and `/ready` lists it under `warnings`.

## Images and versions

`ghcr.io/helton/midir` is public (no login), for amd64 and arm64, about 5 MB compressed. Tags:

| Tag | What | Moves? |
|---|---|---|
| `X.Y.Z` | a release | no |
| `X.Y`, `latest` | the newest release (of that minor line) | yes |
| `sha-<commit>` | any build of `main` | no |
| `dev` | the newest build of `main` | yes |

`docker/compose.yml` runs the release it was bumped to; another build runs with `MIDIR_IMAGE`, e.g.
`MIDIR_IMAGE=ghcr.io/helton/midir:sha-a817822 docker compose -f docker/compose.yml up -d`. Prefer a version or
`sha-<commit>` to `dev` and `latest`: those move, and a registry mirror (a corporate Artifactory, for one) can keep
serving an old build under them for hours. For a mirror or a fork, set `MIDIR_IMAGE=<registry>/midir:<tag>` in the
shell or `docker/.env`.

Every build says what it is (`midir --version`, the startup banner, `GET /health`): a release reports the bare version
(`0.1.1`); anything else carries the commit as semver build metadata: `0.1.1+dev.a817822` for another `main` build,
`0.1.1+local.a817822` for an image built on your machine (`.dirty` with uncommitted changes) and `0.1.1+src.a817822`
for a binary built from a git checkout.

## Telemetry

With `OTEL_EXPORTER_OTLP_ENDPOINT` set (the observability overlay does it), each request produces an OpenTelemetry
span `chat <model>` (kind SERVER, the child of the client's `traceparent` when it sends one; none when that trace is
not sampled) with a CLIENT child span per backend call, and metrics (`midir.*`, `gen_ai.client.token.usage`, and
`gen_ai.server.request.duration` and `gen_ai.server.time_to_first_token` in seconds): tokens, duration, time to first byte, model and backend, client (`claude-code`,
`copilot`, `hermes`, `openclaw`, `deepseek-harness`, ...), session, tool calls, follow-ups, retries, queue waits,
truncations. Metric labels are bounded: the session label is "-" for a request that carries no session of its own
(a stateless Responses request), and the requested model name stays on the spans. Series idle for an hour are
dropped. Without a collector, `[telemetry] prometheus = true` serves the
metrics at `GET /metrics` (behind `MIDIR_API_KEY` when one is set: give the scraper a `bearer_token`). Prompt
content is never exported. The exporter reads `OTEL_EXPORTER_OTLP_ENDPOINT` (or `_TRACES_ENDPOINT` and
`_METRICS_ENDPOINT`), `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` (the service
name: `OTEL_SERVICE_NAME`, else the attributes', else `[telemetry] service_name`); it speaks OTLP over HTTP/protobuf
only and logs a warning when `OTEL_EXPORTER_OTLP_PROTOCOL` asks for another. The Grafana dashboard is provisioned from
[docker/grafana/](../docker/grafana/).

## Logs

Responses carry the request id (`x-request-id`, or `request-id` for Anthropic clients), which the log lines and the
spans also show. `MIDIR_LOG=<filter>` sets the level per module (for example `info,midir::store=debug`),
`MIDIR_LOG_FORMAT=json` writes one JSON object per line, and `--debug` turns on DEBUG, where prompts and model output
appear. Containers log in UTC unless `TZ` is set in `.env` (the example sets `TZ=America/Sao_Paulo`); recreate the
container after changing it.

## Troubleshooting

Start with **`midir check`** (in Docker: `docker exec midir midir check`). It prints every setting with where its value
came from (environment, file or default), the models and their agents, unknown settings, and checks the credentials,
TLS and proxy by fetching a backend token. It serves nothing and leaves the responses store alone, so it is safe next
to a running server. `--no-network` skips the token; `--agents` also sends each model one short prompt (one request
per model from the account's quota), which finds an agent id that is wrong or not shared with the client. It exits 1
when a check fails.

- **`/ready` answers 503**: a backend's readiness check failed; the message says why (StackSpot: realm, client id or
  secret, network, CA).
- **403 with an empty body on requests**: an agent id is wrong or not shared with the client key; `GET /health` shows
  which target each model maps to.
- **429**: the account's 100 requests per minute were exceeded (several clients or Midir instances on one account).
- **401 from Midir**: `MIDIR_API_KEY` is set and the client sent another key (or none).
- **502 with code `upstream_401` or `upstream_403`**: StackSpot refused Midir's own credentials or access to an
  agent (check the client id, secret and agent sharing); the client's key is not the problem.
- **A setting "must be ..." at startup**: a value outside its range (a negative retention, a zero prompt cap); the
  message names the setting.
- **Client says the model does not support tools or images**: declare tool calling on and vision off (see the client
  examples); images become placeholders on purpose.
- **A setting is "unknown ... ignored"**: a typo, or the running build is older than the setting; check the version in
  `GET /health` (and the registry mirror note above).
- **Corporate TLS**: Midir trusts the system's certificate store (and `SSL_CERT_FILE`); for a CA that is not there,
  set `STACKSPOT_CA_BUNDLE` to a PEM bundle with it (added to the trusted roots). Cargo itself honors
  `CARGO_HTTP_CAINFO`.
- **Corporate proxy**: Midir honors `HTTPS_PROXY`/`https_proxy` and `NO_PROXY`/`no_proxy`; when both spellings are
  set, the uppercase one wins.
- **Name resolution behind a corporate proxy**: the image's binary is static (musl's resolver). If names resolve
  differently than on the host, run a binary built for the host from source (`cargo build --release`).
- **VS Code does not offer the custom models**: they work in a **Local** agent session; a Copilot CLI session drops
  models from other providers and falls back to its default.
