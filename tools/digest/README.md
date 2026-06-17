# mindex-digest — MCP server for cheap, compressed codebase orientation

A thin [MCP](https://modelcontextprotocol.io) stdio server with a **single tool,
`digest`**, that lets a coding agent understand part of a codebase *without pulling
raw code into its context window*. It is a sibling of `tools/mcp` (raw search) and
`tools/indexer` — it does **not** touch the Rust service; it only consumes mindex's
existing HTTP `/search` API and a local LLM via [Ollama](https://ollama.com).

## Why it exists

The expensive resource is the calling model's context. The trick:

```
your decomposed queries  →  concurrent mindex /search  →  dedup + glue (Python)
                         →  local LLM (Ollama) digests  →  summary + source pointers
```

- **You send crumbs.** A few short sub-queries (your *query decomposition* of one
  intent) — a few dozen tokens. That's the cheap part, and you (the strong model)
  are good at it.
- **Raw chunks never reach you.** They travel mindex → this process → the local LLM
  and die there. Only the compact summary plus `file:line` source pointers cross the
  MCP boundary back.
- **The bulk runs for free on local hardware.** A cheap local model (default
  `qwen2.5:14b`) does the reading/summarising; the strong model only plans the
  retrieval and reads the digest.

This is the inverse of `tools/mcp`'s raw `search`: that returns verbatim chunks (for
code you intend to *edit*); `digest` returns a briefing (for *understanding*). Use
`digest` to orient, then follow its `sources` with raw `search` when you need exact
code. In practice this is roughly an **order-of-magnitude context saving** on a
survey — e.g. orienting in a multi-file mechanism through one `digest` returns a
short briefing + pointers instead of the ~20 full code chunks several raw searches
would dump into the agent's window.

### Two regimes (from real use)

- **Orientation** ("how does X work", "where is Y") — one `digest` usually suffices;
  this is where the order-of-magnitude saving lands.
- **Implementation** (you must touch a *complete* set of call-sites and copy exact
  patterns) — treat the digest as a *map, not the answer*. The score-capped glue can
  drop a long-tail must-have chunk and the cheap model can misattribute a pattern, so
  escalate raw `search` for anything the summary doesn't explicitly cover — and when
  it admits a chunk "isn't shown", take that as a precise escalation cue. Recall here
  is governed by `DIGEST_MAX_CHUNKS` / `DIGEST_NUM_CTX` (raise them together).

## The `digest` tool

```
digest(project_guid: str, queries: list[str]) -> dict
```

- `project_guid` — from the repo-root `.mindex` file (same identity contract as the
  other mindex tools).
- `queries` — 2-4 short natural-language sub-queries.

Returns `{"summary", "sources": [{path, start_line, end_line, score}], "queries"}`.
`summary` is `""` and `sources` is `[]` when nothing matched.

## Prerequisites

1. **mindex is up and the project is indexed** (see the root README and
   `tools/mcp/README.md`). This server reuses the same `MINDEX_*` config.
2. **Ollama is running** with the digest model pulled:
   ```sh
   ollama pull qwen2.5:14b      # or set DIGEST_MODEL to one you have
   ```

## Setup

```sh
cd tools/digest
poetry install
```

Register with Claude Code (run through Poetry so it uses this venv; absolute path):

```sh
claude mcp add mindex-digest \
  --env MINDEX_NO_VERIFY=1 \
  -- poetry -C /data/silencespeakstruth/Projects/mindex/tools/digest run mindex-digest
```

`claude mcp list` should then show `mindex-digest` connected. As with `tools/mcp`,
there is **no network at handshake** — the server lists its tool even if mindex or
Ollama are down; a call made while a dependency is down returns a clean error.

## Configuration (env vars)

mindex side mirrors `tools/mcp` / `tools/search/mindex-search.sh`:

| Variable             | Default                   | Meaning                                          |
| -------------------- | ------------------------- | ------------------------------------------------ |
| `MINDEX_SERVER`      | `https://127.0.0.1:11111` | mindex server URL                                |
| `MINDEX_PROTOCOL`    | `v0`                      | API version in the URL path                      |
| `MINDEX_NO_VERIFY`   | *(off)*                   | truthy → skip TLS verify (self-signed cert)      |
| `MINDEX_CACERT`      | *(unset)*                 | path to a CA bundle for the self-signed cert     |

Digest side:

| Variable               | Default                  | Meaning                                                       |
| ---------------------- | ------------------------ | ------------------------------------------------------------ |
| `OLLAMA_HOST`          | `http://localhost:11434` | Ollama base URL                                              |
| `DIGEST_MODEL`         | `qwen2.5:14b`            | model used to summarise (must be pulled in Ollama)           |
| `DIGEST_PER_QUERY_K`   | `6`                      | chunks pulled from mindex per sub-query (raw, unseen by you); raise it (e.g. `10`) so a wider cap can actually be filled |
| `DIGEST_MAX_CHUNKS`    | `32`                     | cap after dedup, before the local LLM. Higher = better recall (keeps long-tail must-have chunks) at the cost of a bigger local prompt — **must be matched by `DIGEST_NUM_CTX`** |
| `DIGEST_NUM_CTX`       | `24576`                  | Ollama context for the digest pass. Must hold `DIGEST_MAX_CHUNKS` chunks (each ≤512 tok) or Ollama **silently truncates** the prompt, dropping the lowest-scored long-tail chunks. ≈ `MAX_CHUNKS × 540 + 1.5k`; for 64 chunks use ~`32768` |

Set **one** of `MINDEX_NO_VERIFY=1` or `MINDEX_CACERT` (mindex serves a self-signed cert).

## Design notes

- **mindex is untouched.** No new endpoint, no schema change — the search engine
  stays a search engine. The compression layer lives entirely in this adapter, in
  front of mindex, and is fully removable.
- **Sub-queries run concurrently** (`asyncio.gather`) and results are deduped on
  chunk identity (`path` + line span), keeping the best score, then capped by score
  to `DIGEST_MAX_CHUNKS` so a wide decomposition can't flood the digester.
- **Source pointers, always.** The digest cites `[path:start-end]` and the response
  carries a structured `sources` list — that's the path back to ground truth via raw
  `search`, and it keeps the cheap model honest.
