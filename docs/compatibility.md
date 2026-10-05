# API compatibility

What clients can expect from each endpoint, and where Midir's emulation differs from a native provider. How the
emulation works inside: [architecture.md](architecture.md). What it means for an agent running on top of Midir:
[agent-brief.md](agent-brief.md).

## Endpoints

| Method and path | What |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions, streaming or not |
| `POST /v1/responses`, `GET /v1/responses/{id}` | OpenAI Responses, streaming or not, `previous_response_id` |
| `POST /v1/messages`, `POST /v1/messages/count_tokens` | Anthropic Messages, streaming or not |
| `GET /v1/models` | configured models |
| `GET /health` | version, model mapping, backends and their queues, the Responses memory cache (liveness) |
| `GET /ready` | each backend's readiness: credentials, network and TLS, without spending model quota (503 on failure) |
| `GET /metrics` | the metrics in Prometheus format, with `[telemetry] prometheus = true` |

OpenAI clients use `http://127.0.0.1:18880/v1`; Anthropic clients use `http://127.0.0.1:18880`.

## Native-like

All message roles; function tools with schemas, parallel calls (or one call with `parallel_tool_calls: false`) and
results; `tool_choice` auto/none/required/named; streaming in each protocol's own event format; real token usage;
errors in each protocol's format; `previous_response_id` chains that survive restarts (30 days, 500 MB cap) and
`item_reference` to their output items.

## With caveats

Search the code for `CAVEAT` to find each of these.

- **Tool calling is prompt-based**: `<tool_call>` blocks are parsed from the model's text. JSON with raw newlines,
  tabs or trailing commas is read as meant, and `</tool_call>` inside a JSON string does not end the call; a call
  whose JSON is still invalid gets one hidden follow-up and is never passed on broken.
- **Automatic follow-ups**: a reply that only announces an action, denies an ability a tool provides, or leaves
  pending an action the user ordered gets a hidden follow-up (two at most) whose calls are appended to the same
  response. A follow-up only confirms a commit or a test run the user explicitly ordered (and did not forbid,
  condition or keep for themselves); push, merge, deploy and install are never confirmed for the user. A reply that
  reports a failed call (a 403 after trying the fetch tool) is left as it is, and no follow-up makes again a call
  whose last two runs returned the same result. `followups = false` in `[server]` or a model (`MIDIR_FOLLOWUPS=0`)
  turns these follow-ups off; a forced `tool_choice` is still asked for again.
- Structured JSON is prompt + validation + one repair, not streamed incrementally.
- `max_tokens` and `stop` are applied after generation; `count_tokens` is an estimate (4 characters per token).
- Whenever a stream is idle (the backend can take a minute to start, a follow-up runs after the text), an SSE
  keepalive goes out every 15 s (`[server] keepalive_s`) so clients and proxies with idle timeouts do not abort.
- Above the prompt cap the oldest turns are dropped and the model is told; if the recent turns alone are still too
  big, the largest tool results (then user messages) are cut in the middle, keeping head and tail. A backend refusal
  for input length is retried once with a proportionally smaller prompt.
- Images, audio and files become a text placeholder; built-in provider tools (web search, ...) are omitted.
- **Refused**: `logprobs`, `n > 1`, `/v1/embeddings`. **Accepted and ignored** (logged once per client):
  `temperature`, `top_p`, `seed`, `reasoning`, `reasoning_effort`, `thinking`, `cache_control`, `metadata`.

## Backend limits (StackSpot)

- Input up to **272,000 tokens**.
- **100 requests per minute per account**, shared by every agent and client. Midir queues instead of failing
  (`[backends.stackspot.limits]`: 8 concurrent, 90 per minute by default); a backend 429 pauses new requests and
  halves the local budget, which recovers by one request per minute, so a second Midir or StackSpot's own chat on the
  same account is absorbed. Midir's own 429s carry `Retry-After`.
- GPT 5.x counts its reasoning as output tokens; time to first byte is 1.5-8 s depending on prompt size and model.

More in [backends/stackspot.md](backends/stackspot.md).
