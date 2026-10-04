# StackSpot AI backend

The only backend today: StackSpot AI agents, called through the Agent API, which takes one text prompt and streams
text back. Everything a native LLM API offers beyond that (roles, tools, structured output, stop sequences) is
emulated by the gateway on top of it.

## Agent setup

Create one agent in the StackSpot AI portal with exactly this configuration and put its id in
`STACKSPOT_DEFAULT_AGENT_ID` and `STACKSPOT_GPT_5_1_AGENT_ID`. It is the winner of the configuration benchmark
(2026-09-29, re-validated 2026-10-01); any deviation changes latency and tool-call fidelity.

Optional second agent: the same configuration with **LLM = Open AI - gpt-4.1**, id in `STACKSPOT_GPT_4_1_AGENT_ID`.
Clients then see two models, `gpt-5.1` and `gpt-4.1`, on the same Midir instance (models and routing: `config/midir.example.toml`).

| Field | Value |
|---|---|
| Name | anything (Midir only uses the id); suggested: `midir-gpt-5.1` (and `midir-gpt-4.1`) |
| LLM | **Open AI - GPT 5.1** |
| System prompt | the literal text below |
| Tools | none |
| Multi-Agent | none |
| Knowledge sources | none |
| Suggested prompts | none |
| Advanced: Conversational mode | **OFF** |
| Advanced: Planner type | **Simple - Fast - Smart** |
| Advanced: Maximum number of interactions | 50 (irrelevant without tools) |
| Advanced: Autonomy mode | not available with the Simple planner |
| Advanced: Chat memory management | Buffer Memory (irrelevant with conversational mode off) |
| Advanced: Structure output | **OFF** |

System prompt (copy literally; Portuguese and English versions performed identically):

```
Você é um modelo de linguagem de uso geral. Siga estritamente as instruções contidas na mensagem do usuário, incluindo instruções de formato. Não adicione saudações, preâmbulos ou explicações sobre si mesmo. Não use Knowledge Sources.
```

Notes

- Changing the LLM in the portal resets Tools, Multi-Agent and Structured Responses. This configuration uses none of them.
- Midir always sends `stackspot_knowledge: false`; with `true` the platform attaches a cross-account knowledge source on its own.
- Measured alternatives (2026-10-01, Claude Code and Copilot CLI, easy and hard tasks): GPT 4.1 makes the same tool
  choices as 5.1, 4-5x faster, half the input tokens, but stops before the final commit in 1 of 3 hard tasks (asks
  instead of acting), so it is the interactive agent, not the autonomous one. GPT 5 is 2-3x slower than 5.1 with no
  quality gain. o4-mini, gpt-4.1-mini and o3-mini fail the hard task. The Tool-Oriented planner adds 50% latency.
- StackSpot's own agents of the account (named "flex" and "ai codegen" here) run GPT 4.1 with their own system prompts
  and can be exposed too (`STACKSPOT_FLEX_AGENT_ID`). Measured: "flex" finishes agent tasks as well as GPT 5.1 at
  about twice the speed; "ai codegen" is less reliable.
- The "API integration" tab of the portal (Return Knowledge Sources, Keep conversation, ...) only affects StackSpot's
  own chat; it does not change the Agent API Midir uses.
- Credentials: a Client Key (client id/secret) of the realm, stored in `.env`.

## Measured API contract

What the gateway relies on, measured against the live API (2026-09/10):

| Item | Value |
|---|---|
| Token | `POST https://idm.stackspot.com/{realm}/oidc/oauth/token`, client credentials; valid 20 minutes (renewed 60 s before expiry; a 401 from the agent forces one renewal; the call gives up after 30 s) |
| Chat | `POST https://genai-inference-app.stackspot.com/v1/agent/{agent_id}/chat`, body `{"streaming": true, "user_prompt": ..., "stackspot_knowledge": false, "return_ks_in_response": false}` |
| Stream | SSE; deltas carry only `message`; a final event carries `stop_reason`, `message_id` and `tokens` (`input`, `output`) |
| Input limit | 272,000 tokens: above it, `400 INFERENCE_6001_LLM_MODEL_BAD_REQUEST` ("Your messages resulted in N tokens"), never 413. The gateway caps prompts at 1M characters (oldest turns dropped, then the largest tool results cut in the middle) and, on that error, retries once with a proportionally smaller prompt |
| Rate limit | 100 requests per minute per account (realm + client id), shared by every agent: `429 INFERENCE_3008_CHAT_RATE_LIMIT_EXCEEDED`, no `Retry-After` |
| Output tokens | GPT 5.x counts its reasoning as output (a one-word answer costs 20-40 tokens; 1 on GPT 4.1) |
| Latency | time to first byte 1.5-2 s for small prompts; 4-8 s with 30-40k-token prompts on GPT 5.1, about 2 s on GPT 4.1 |
| Not available | images, audio and files (text only), embeddings, logprobs, n > 1, prompt caching, native tool calling |
