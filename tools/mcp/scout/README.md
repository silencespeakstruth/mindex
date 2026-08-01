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

The machinery lives in mindex itself (`src/research.rs`); this server is a thin SSE
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
research(project_guid, question, effort="medium", model=None,
         include=None, exclude=None, budget=None, context_run_ids=None,
         include_excerpts=False)
  → {"report", "steps": [{n, action, query|name|path, hits, spans}], "elapsed_ms",
     "done_reason", "binding", "usage": {spent vs. granted, "binding", "shares"},
     "run_id", "seq",                      # the stored run, when it was stored
     "citations": {...}, "citations_verified", "citations_total",
     "excerpts": [...] | "excerpts_hint",  # small sets come back unasked
     "scope": {...}}                       # when include/exclude was given
```

- `question` — one full question in plain language, not keywords: the local model does
  its own decomposition.
- `effort` — `low` / `medium` / `high` selects a server-configured budget
  (`[research.effort.*]`: wall-clock, local tokens, index lookups — whichever runs out
  first). The numbers are deliberately not repeated here; `GET /config` serves them,
  including `research.observed` — what each level has actually *cost* lately, as
  opposed to what it grants. Costs the caller no tokens, only wall-clock (~1 min at
  `low`, several to fifteen at `high`, model-dependent).
- `budget` — per-axis override of the preset (`max_seconds`, `max_tokens`, `max_steps`,
  plus the report-shape keys `max_report_sections`, `max_report_words`,
  `checkpoint_every_steps`, `evidence_width`). The server owns the ceilings and 400s an
  over-large value.
- `context_run_ids` — earlier `run_id`s whose reports are handed to this run as
  background (never as citable evidence). The chaining path for follow-ups.
- `include_excerpts` — force the verbatim indexed code into the result. A *small*
  excerpt set arrives without it; a large one is withheld behind `excerpts_hint`
  (threshold: `RESEARCH_EXCERPT_AUTO_BYTES`).
- `done_reason` / `usage` — whether the report was cut short, and how close the run came
  to each budget. `binding` names the axis with the **largest share spent**, and
  `usage.shares` gives all four percentages — read them together, since `binding` alone
  names a maximum rather than a problem.
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
`RESEARCH_READ_TIMEOUT` (`120`), `RESEARCH_TOTAL_TIMEOUT` (`4200`) — seconds — and
`RESEARCH_EXCERPT_AUTO_BYTES` (`32768`), the size below which excerpts come back
unasked.

`RESEARCH_TOTAL_TIMEOUT` must outlast the server's own worst case,
`effort.high.max_seconds + report_timeout_ms` (3600 + 300 s at the shipped defaults,
published per level as `research.effort.*.worst_case_seconds`). It was `1800` here
once, which killed every high-effort run in flight.

## What else you should know

- **Client-side timeout.** A run outlasts some MCP clients' default tool timeout. For
  Claude Code, export `MCP_TOOL_TIMEOUT=4200000` (ms) in the environment that launches
  the client — it is a client setting, not a server one, and it should match
  `RESEARCH_TOTAL_TIMEOUT`.
- **Cancellation is the disconnect — usually.** Abandon the tool call, the HTTP stream
  closes, and the job is cancelled server-side. The gap is a call this end stops
  *waiting* for without the socket closing: the run keeps going and keeps its slot. The
  result then carries `live_run_id` and `still_running`, and
  `DELETE /research/active/{run_id}` ends it. `GET /research/active` lists what is
  holding the slots.
- **One run at a time.** `research.max_concurrent` (in `GET /config`) is typically 1 on
  a single-GPU host; a second call is refused with 429 rather than queued. Plan
  investigations serially.
- **Previously** this server ran its own decompose→search→summarise loop against Ollama
  (the `digest` tool). That moved into mindex, where it became iterative and cancellable;
  `research` replaces it.
