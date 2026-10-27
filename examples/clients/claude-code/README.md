# Claude Code on Midir

`settings.json` maps Opus, Sonnet and Haiku to the gpt-5.1, flex and gpt-4.1 models of a Midir at
`127.0.0.1:18880` (copy it to `~/.claude/settings.json` or merge the `env` block into yours). Everything Midir
needs is in that block: the base URL, a token (any value: Midir does not check it), the model names.

## Start it with six tools

```bash
claude --tools Read,Edit,Write,Bash,Grep,Glob
# or, once and for all:
alias claude='claude --tools Read,Edit,Write,Bash,Grep,Glob'
```

Claude Code offers the model about 24 tools (web search and fetch, notebooks, subagents, task lists, skills, MCP
...), and every one of them is listed in every prompt of the session; on a text agent like StackSpot's that
listing is paid again on every step, since nothing is cached. A coding task uses six. Measured on 270 sessions per
model (15 tasks, 3 runs each, hidden tests scored):

| model | input tokens | score |
|---|---|---|
| gpt-5.1 | -56% | the same (0.98 vs 0.97) |
| gpt-4.1 | -62% | +11 points of tasks passed |
| flex | -60% | the same (0.87) |

Wall time did not change; gpt-5.1 took about one step more per task (13.6 against 12.1). `tool_schema = "compact"` on the Midir side (per model, see
`docs/configuration.md`) takes another 10-27% off the listing that remains.

Not worth it on these models and tasks, measured the same way: best of three sessions, acceptance tests written by
a first session, skills and hooks with extra instructions, a judge model on the replies. gpt-5.1 is at the ceiling
of fully specified tasks on its own; what it still loses is the odd early stop ("Continuing to inspect the other
modules...") and the fewer tools do not change that.

## Which model for what

- **gpt-5.1** (Opus): long autonomous and multi-file tasks. The quality reference.
- **flex** (Sonnet): everyday work, about twice as fast, GPT-4.1 with StackSpot's own prompt; from Midir 0.2.1 its
  cut tags and dropped replies are recovered. Its output is capped at 4096 tokens by the agent: a module written
  in one `Write` with its tests can hit it.
- **gpt-4.1** (Haiku): Claude Code's background calls and subagents.
