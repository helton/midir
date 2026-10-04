# DeepSeek Harness (`dsh`)

OpenAI Chat Completions (`/v1/chat/completions`), streaming. Validated with `@deepseek-ai/dsh` 0.2.0-rc.2 on the
three models (file edits, shell, tests, commits).

1. Copy `cordis.patch.yml` (this folder) to `$DSH_HOME` (default `~/.dsh`).
2. `export GATEWAY_API_KEY=gateway` (any value, or Midir's `MIDIR_API_KEY` when it sets one) and, optionally,
   `DSH_MODEL=gpt-4.1` or `flex`.
3. Run `dsh`, or one-shot: `dsh --profile headless --json "<task>"`.

Privacy: dsh sends telemetry and session logs to DeepSeek by default. The patch disables the session-log upload; for
telemetry also set `DSH_TELEMETRY_MODE=DISABLED`. dsh loads a `.env` from the directory it starts in. Its built-in
`web_search` needs a DeepSeek API key and fails without one.
