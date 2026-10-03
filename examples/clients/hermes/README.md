# Hermes Agent

OpenAI Chat Completions (`/v1/chat/completions`), streaming. Validated with Hermes Agent 0.21.5: file edits, shell,
tests and commits on the three models; its session-title call (non-streaming JSON mode) validates on the first try.

1. Put `config.yaml` (this folder) in `$HERMES_HOME` (default `~/.hermes`), or merge its `model`/`providers` blocks.
2. Run `hermes`, or one-shot: `hermes -z "<task>" -m gpt-5.1`.

Notes
- Hermes sends `reasoning_effort`; the gateway accepts and ignores it (logged once per request).
- Every tool schema is sent on every turn and there is no prompt cache: fewer enabled toolsets means faster steps.
- Memory providers that need embeddings do not work (the gateway has no embeddings endpoint); the built-in file memory does.
- The gateway labels Hermes requests as `hermes` in telemetry (recognized by its system prompt).
