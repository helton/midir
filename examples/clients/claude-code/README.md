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

## Web search and fetch through Midir

Claude Code's built-in WebSearch runs on Anthropic's servers and never works through Midir; WebFetch does, but
only in Claude Code. With `[mcp] enabled = true` on the Midir side (and a StackSpot agent with the Web Search and
Web Extraction tools in `STACKSPOT_WEB_TOOLS_AGENT_ID`, see `docs/configuration.md`), Midir serves the `web_search`
and `web_fetch` tools over MCP on its own port. Connect once:

```bash
claude mcp add --transport http midir http://127.0.0.1:18880/mcp
# with MIDIR_API_KEY set on Midir:
claude mcp add --transport http midir http://127.0.0.1:18880/mcp --header "Authorization: Bearer <key>"
```

Servers cannot go in `settings.json`; that command stores it in `~/.claude.json` for the current project
(`--scope user`: for every project, since Midir is always at the same address). Per project instead: `.mcp.json`
from this folder at the project root, with `"enableAllProjectMcpServers": true` in the project's
`.claude/settings.local.json` so it loads without the approval prompt; or `--mcp-config .mcp.json` in the alias,
for one session at a time. The tools appear as `mcp__midir__web_search` and `mcp__midir__web_fetch`; the six-tools
alias above keeps working, since `--tools` filters the built-in tools only. Two short tool descriptions are added
to every prompt: connect the server for the projects whose work needs the web.

Measured 2026-10-30 (gpt-5.1 through Midir): "find the reqwest 0.13 release notes and read them" took 3 turns and 40 s,
with one `web_search` (28 s) and one `web_fetch` (0.6 s).

## Which model for what

- **gpt-5.1** (Opus): long autonomous and multi-file tasks. The quality reference.
- **flex** (Sonnet): everyday work, about twice as fast, GPT-4.1 with StackSpot's own prompt; from Midir 0.2.1 its
  cut tags and dropped replies are recovered. Its output is capped at 4096 tokens by the agent: a module written
  in one `Write` with its tests can hit it.
- **gpt-4.1** (Haiku): Claude Code's background calls and subagents.
