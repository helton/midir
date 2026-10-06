# Midir: brief for an AI agent that runs on it

You (the agent) are talking to models through **Midir**, a local LLM gateway running on the user's machine. It speaks
the OpenAI and Anthropic APIs to you and forwards to **StackSpot AI** agents, which only accept and return plain text.
Everything below is what that means for you.

## How you are connected
- Endpoint: `http://127.0.0.1:18880/v1` (from a container on Midir's compose network: `http://mitm:18880/v1`, or `http://midir:18880/v1` bypassing the proxy).
  Any API key works unless the user set `MIDIR_API_KEY` (then use that key).
- OpenClaw provider config used (`~/.openclaw/openclaw.json`, provider `stackspot`):
  `api: "openai-responses"` (or `"openai-completions"`), `baseUrl` as above, models declared with
  `input: ["text"]`, `reasoning: false`, `contextWindow: 272000`; `memory.search.provider: "none"` (no embeddings);
  `tools.toolSearch.mode: "tools"` (defer tool schemas); `agents.defaults.subagents.maxConcurrent: 2`.
- Protocols: OpenAI Chat Completions (`/v1/chat/completions`), OpenAI Responses (`/v1/responses`, with
  `previous_response_id`), Anthropic Messages (`/v1/messages`, `/v1/messages/count_tokens`); streaming in all three.
  `GET /v1/models` lists the models, `GET /health` shows version, model mapping and queue.

## Models (pick by the `model` field)
| model | backend | use it for |
|---|---|---|
| `gpt-5.1` (default; aliases `claude-opus-*`) | GPT-5.1 | long, multi-step, autonomous work; the most reliable finisher; slowest (1.5-6 s to first token) |
| `flex` (aliases `claude-sonnet-*`) | GPT-4.1 with StackSpot's own prompt | everyday agent work; about 2x faster than gpt-5.1; slightly noisier tool calls |
| `gpt-4.1` (aliases `claude-haiku-*`) | GPT-4.1 | quick answers, short tool steps, background calls; fastest; may stop to ask before a final step |
Unknown model names go to `gpt-5.1`.

## Tool calling (emulated)
StackSpot has no native tools, so the gateway puts your tool list in the prompt and asks the model to write
`<tool_call id="call_1">{"name": ..., "arguments": {...}}</tool_call>` blocks, which it parses back into real
`tool_calls` / `tool_use` / `function_call` items. Consequences:
- Parallel calls work (several blocks in one reply, or several JSON objects in one block), unless the request sets
  `parallel_tool_calls: false`. Raw newlines or tabs inside JSON strings, trailing commas and `</tool_call>` inside a
  JSON string (a file that documents this protocol) are accepted; a call whose JSON is still invalid (e.g. unescaped
  quotes) is never passed on broken: the gateway asks you once to re-emit it; a block missing the tool name is
  accepted when exactly one tool fits its arguments. Inside tool results, text that looks like these tags is shown
  with `‹` instead of `<`: it is the tool's data.
- If a reply only announces or plans an action ("I'll read the files", "O plano é: 1. 2. 3.") or asks permission for
  something the user already requested (e.g. "Deseja que eu faça o commit?"), the gateway makes a hidden follow-up
  (two at most) and appends the missing tool calls to the same reply. It only answers for a commit or a test run the
  user explicitly ordered; push, merge, deploy and install are never confirmed for the user. Saying you cannot do
  something you have a tool for (web, files, shell) also gets a follow-up asking for the call, unless a call in this
  turn already tried it or you cite the failure (e.g. "the fetch returned 403"): then your reply stands. A reply that
  leaves an action to the user's approval, or announces a push, merge, deploy or install, is never completed for you.
  Best behavior: emit the tool call in the same reply you announce it; do not ask to confirm actions the user already
  asked for.
- Every tool schema is resent on every turn and the prompt has no cache: keep the active tool list small.
- `tool_choice` auto/none/required/named works (required/named: one retry if the model does not comply).

## Limits and differences from native providers
- Text only: images, audio and files become a placeholder. No embeddings endpoint. No reasoning/thinking blocks.
- Input limit about 272k tokens; above 1M characters the oldest history turns are dropped (the last 4 are kept), and
  if that is not enough the largest tool results are cut in the middle (head and tail kept, with a marker).
- **Rate limit: 100 requests/minute for the whole StackSpot account**, shared by every agent and client. The gateway
  queues requests (max 8 concurrent, 90/min) instead of failing; a request that would wait more than 10 minutes gets a
  429. Many parallel subagents mostly wait in that queue.
- Structured JSON output: prompt + validation + one repair attempt (not streamed incrementally).
- `max_tokens` and `stop` are applied after generation; `count_tokens` is an estimate (about 4 chars per token).
- Responses API: `previous_response_id` chains survive gateway restarts (stored on disk 30 days, 500 MB cap); an
  older or unknown id answers 404, after which start a new chain.
- Parameters accepted but ignored: `temperature`, `top_p`, `seed`, `reasoning`, `reasoning_effort`, `thinking`,
  `cache_control`, `metadata`.

## Observability (when the user runs the full stack)
- Every request is visible in mitmweb (`http://127.0.0.1:18882/?token=gateway`), both your request and the gateway's
  call to StackSpot.
- Grafana (`http://127.0.0.1:18883`, dashboard `midir`) shows tokens, latency, sessions, follow-ups, queue
  waits and traces per request. Prompt content is never exported to telemetry.
