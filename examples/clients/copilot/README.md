# GitHub Copilot (VS Code)

1. Start the gateway (see the repository README).
2. Copilot Chat -> model picker -> Manage Models -> Add Models... -> **Custom Endpoint**. VS Code opens
   `chatLanguageModels.json`; paste the contents of the file in this folder: one provider (`apiType: responses`) with the models `gpt-5.1`, `gpt-4.1`
   and `flex`. To use another API type, change `apiType` for the whole provider (`chat-completions` or `messages`);
   Responses is recommended because its telemetry has a stable session id (`prompt_cache_key`).
   Any value works as the API key: Midir ignores it.
3. Keep `url` as the prefix `http://localhost:18880/v1`; VS Code appends `/responses` (or `/chat/completions`, `/messages`)
   according to `apiType`.
4. `vision: false` and `thinking: false` are deliberate (text-only API; reasoning is not exposed).
   `maxInputTokens: 250000` stays under the measured StackSpot limit of 272k input tokens.

Each model id must be a model name (or alias) in `config/midir.toml` (`GET /health` shows the mapping); remove the entries
you do not run. Switching models in the picker switches agents per request: `gpt-5.1` for long tasks, `gpt-4.1` or
`flex` for fast interactive work (same model, different system prompts).

All three API types were validated with agent mode (5-8 tool-calling steps per task, ~55 s). Pick the one with
the fewest quirks in your VS Code build; Midir behaves the same behind each.
Reference: https://code.visualstudio.com/docs/agent-customization/language-models
