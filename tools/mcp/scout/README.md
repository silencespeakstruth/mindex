# scout — MCP server that investigates so your model doesn't have to

One tool, `research`. The calling model sends **a question**; a **local** model runs the
entire investigation — searching the index, looking up symbols, reading the code — and
sends back a cited Markdown report.

```
your question (a few dozen tokens)  →  mindex POST /research (SSE)
                                    →  local model loops search/symbols, reads code
                                    →  report + step trace come back
```

**The code it read never reaches you. Neither does its thinking** (thousands of tokens,
dropped on the floor by this adapter). That is the whole product: the investigation runs
on your hardware at zero token cost, and the expensive model pays for one question in
and one briefing out.

The machinery lives in mindex itself (`src/research.rs`); this server is a ~200-line SSE
client and remains fully removable.

## The contract with the caller

The server's `instructions` are deliberately blunt, because the saving only exists if the
agent actually delegates:

- **You do not investigate this codebase yourself.** scout does.
- **Trust the report.** It cites `path:start-end`. Don't re-derive it, don't spot-check
  it against the files, don't "confirm" it with extra searches.
- **If it isn't enough, ask scout again** — a sharper, narrower follow-up. That is the
  intended path, never a fallback to your own search loop.
- The raw [`mindex`](../mindex/README.md) tools are for pulling the byte-exact text of a
  place the report already cited, immediately before editing it.

## The tool

```
research(project_guid, question, effort="medium", model=None, include=None, exclude=None)
  → {"report", "steps": [{n, action, query|name, hits}], "elapsed_ms",
     "done_reason", "usage": {spent vs. granted, "binding"}}
```

- `question` — one full question in plain language, not keywords: the local model does
  its own decomposition.
- `effort` — `low` / `medium` / `high` selects a server-configured budget
  (`[research.effort.*]`: wall-clock, local tokens, index lookups — whichever runs out
  first). The numbers are deliberately not repeated here; `GET /config` serves them.
  Costs the caller no tokens, only wall-clock (~1 min at `low`, several at `high`,
  model-dependent).
- `done_reason` / `usage` — whether the report was cut short, and how close the run came
  to each budget. `usage.binding` names the axis that was closest to exhausted.
- `include`/`exclude` — `{"paths": [...], "programming_languages": [...]}`, applied to
  every lookup. Standing project scope belongs in `.mindex`.

## Setup

Needs a running mindex whose **`[research]` section points at a local Ollama** with the
model pulled, plus the embedder (research issues real searches, and those embed):

```toml
# ~/.config/mindex/config.toml
[research]
ollama_url = "http://127.0.0.1:11434"
default_model = "glm-4.7-flash:latest"
```

```sh
cd tools/mcp/scout && poetry install
claude mcp add scout \
  -- poetry -C /abs/path/tools/mcp/scout run scout
```

Register it from the **repo root** — MCP scope is per-directory. `claude mcp list`
should show it connected; there is no network at handshake.

## Configuration (env vars)

`MINDEX_SERVER`, `MINDEX_PROTOCOL`, `MINDEX_NO_VERIFY`, `MINDEX_CACERT` — same meanings
and defaults as [`tools/mcp/mindex`](../mindex/README.md). Research-specific, all
optional: `RESEARCH_DEFAULT_EFFORT` (`medium`), `RESEARCH_CONNECT_TIMEOUT` (`10`),
`RESEARCH_READ_TIMEOUT` (`120`), `RESEARCH_TOTAL_TIMEOUT` (`1800`) — seconds.

## What else you should know

- **Client-side timeout.** A run outlasts some MCP clients' default tool timeout. For
  Claude Code, export `MCP_TOOL_TIMEOUT=1800000` (ms) in the environment that launches
  the client — it is a client setting, not a server one.
- **Cancellation is the disconnect.** Abandon the tool call and the HTTP stream closes,
  which cancels the job server-side. Nothing to clean up.
- **Previously** this server ran its own decompose→search→summarise loop against Ollama
  (the `digest` tool). That moved into mindex, where it became iterative and cancellable;
  `research` replaces it.
