# Configuration

Midir reads two files from its working directory (the repository root when run from source or with Docker Compose):

- **`config/midir.toml`** (copy [config/midir.example.toml](../config/midir.example.toml), which documents every
  setting): `[server]` (port, prompt cap, tool-protocol knobs, Responses store), `[telemetry]`, the **backends**
  (`[backends.<name>]` with its `type`, its options and `[backends.<name>.limits]`) and the **models** clients see
  (`default_model` and one `[[models]]` entry each). Another path: `MIDIR_CONFIG=<path>`.
- **`.env`** (copy [.env.example](../.env.example)): secrets and agent ids only, referenced from the TOML as `${NAME}`.
  Never committed.

## Backends and models

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
# followups = false                       # e.g. no hidden follow-ups for this model
# tool_schema = "compact"                 # this model gets the compact tool listing
```

Each model points to a backend and a target (for StackSpot, an agent), so several LLMs are served side by side and the
request's `model` picks one. Backend-specific setup: [backends/stackspot.md](backends/stackspot.md).

**Routing** of a requested `model`, first match wins:

1. exact name or alias (case-insensitive);
2. a `match` regex, in file order (the `regex` crate's syntax: no lookaround or backreferences);
3. exact after a provider prefix (`openai/gpt-4.1`, `stackspot-gpt-4.1`);
4. the longest configured name contained in it;
5. `default_model`.

`GET /v1/models` lists the models; `GET /health` shows the mapping, the loaded file and each backend's queue.

## Tool listing

Clients send their tools as JSON Schemas, and the prompt lists them for the model. `tool_schema = "json"` (the
default) lists each tool as one line of JSON with its schema. `tool_schema = "compact"` writes each tool as a
`### name` heading, its description and one line per parameter (`- path (string, required): description`, nested
fields indented); what that form cannot say (`$ref`, `allOf`, ...) stays JSON Schema. The descriptions are the same in
both forms, so the gain is the schema syntax: about 10% fewer tokens in the tool listing of Copilot's 75 tools (5-8%
of the whole prompt). Set it in `[server]`, per model, or with `MIDIR_TOOL_SCHEMA`. `tool_desc_max` cuts
descriptions in both forms.

## Environment variables

**The environment wins over the file**, so Docker and other machines override without editing it: `MIDIR_PORT`,
`MIDIR_CONFIG`, `MIDIR_API_KEY`, `MIDIR_READ_TIMEOUT_S`, `MIDIR_REQUESTS_PER_MINUTE`, `MIDIR_MAX_CONCURRENT`, `MIDIR_FOLLOWUPS`,
`OTEL_EXPORTER_OTLP_ENDPOINT`, the StackSpot credentials `STACKSPOT_REALM`, `STACKSPOT_CLIENT_ID`,
`STACKSPOT_CLIENT_SECRET`, `STACKSPOT_CA_BUNDLE`, and the rest listed in `midir --help`. The queue limits and the
`STACKSPOT_*` variables apply to every backend (of that type): a second StackSpot account goes in its own
`[backends.<name>]` options, with those variables unset.

Runtime knobs outside the file: `TOKIO_WORKER_THREADS` (async worker threads, default 4), `MIDIR_LOG` and
`MIDIR_LOG_FORMAT` (see [operations.md](operations.md#logs)), `MIDIR_NO_BANNER=1`.

In Docker, `config/midir.toml` is injected as a compose config: after editing it, run
`docker compose -f docker/compose.yml up -d --force-recreate midir`. A setting Midir does not know is ignored with a
warning in the log (a typo, or a setting newer than the running build). A configuration in the pre-0.0.1 layout
(`[stackspot]`, `[[agents]]`) is still read, with a warning.

## Security

Midir binds to `127.0.0.1`. By default it has **no authentication**: keep it local, or set `MIDIR_API_KEY` (or
`[server] api_key`) and every endpoint but `/health` and `/ready` wants it as `Authorization: Bearer <key>` or
`x-api-key: <key>`; without a key, Midir notes in the log when it listens beyond localhost.

Everything a client sends (open files, terminal output) goes to the backend, as it would to any hosted LLM. Prompts
and model output are logged only at the DEBUG level (`--debug`, or a `MIDIR_LOG` filter that turns it on), to stderr.
Prompt content is never exported to telemetry.

The only data written to disk is the Responses store (`docker/data/gateway/responses`, or `data/gateway/responses`
from source): conversation content in owner-only files (0600, folders 0700), kept 30 days, capped at 500 MB, not
encrypted. `responses_dir = ""` keeps it in memory only, and so does a folder that cannot be created (a container
started without its data volume), with a warning in the log. The container runs as your user, never root.
