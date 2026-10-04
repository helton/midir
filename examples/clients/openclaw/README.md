# OpenClaw

OpenAI Chat Completions (`api: "openai-completions"`) or Responses (`"openai-responses"`), streaming. Validated with
the `openclaw/openclaw` image on the three models (file edits, shell, tests, commits); `openclaw.json` passes
`openclaw config validate`.

1. Merge `openclaw.json` (this folder) into `~/.openclaw/openclaw.json`, or start from it.
2. Run `openclaw agent --local -m "<task>"`, or the gateway and channels as usual.

What each setting is for
- `agents.defaults.model.primary`: `stackspot/gpt-5.1`, `stackspot/flex` or `stackspot/gpt-4.1` (provider id / model id).
- `agents.defaults.subagents.maxConcurrent: 2`: the StackSpot account allows 100 requests per minute in total.
- `models.providers.stackspot.apiKey`: any value, unless the gateway sets `MIDIR_API_KEY` (then that key).
- `models[].input: ["text"]` and `reasoning: false`: the gateway has no image input and never returns reasoning blocks.
- `memory.search.provider: "none"`: there is no embeddings endpoint.
- `tools.toolSearch.mode: "tools"`: tool schemas are deferred, so prompts are smaller and steps faster.

Notes
- From a container or another WSL distro, `127.0.0.1` is not the gateway's host: use `host.docker.internal` (Docker,
  with `--add-host=host.docker.internal:host-gateway` on Linux) or the host's address in `baseUrl`.
- OpenClaw adds an internal-context user message after every tool round; the gateway keeps it after the tool results.
