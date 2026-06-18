# mindex — MCP server for mindex search

A thin [MCP](https://modelcontextprotocol.io) stdio server that exposes mindex
semantic code search to an MCP client (e.g. Claude Code). It is a sibling tool
like `tools/indexer` and `tools/search`: it does **not** touch the Rust service —
each tool call just hits the existing mindex HTTP API.

**This is mindex's intended primary mode.** Instead of reading whole files into its
context window, a coding agent queries the index and gets back the few chunks that
actually matter — so it understands the codebase *cheaply*, without burning tokens.
Search becomes the agent's default way to navigate the repo, and keeping the index
live as it edits is a deliberately cheap, routine operation (see
[How an agent should use it](#how-an-agent-should-use-it)).

Tools exposed:

| Tool                                  | mindex endpoint                  | Notes                                                        |
| ------------------------------------- | -------------------------------- | ------------------------------------------------------------ |
| `search(project_guid, query, include?, exclude?)` | `POST /v0/{guid}/search` | Up to **5** ranked chunks. The top-5 cap (`TOP_K`) is the context budget; the model can't raise it. Optional `include`/`exclude` scope the search by path glob / language (see [Scoping](#scoping-search-includeexclude)). |
| `index_files(project_guid, files)`    | `POST /v0/{guid}/index`          | Reindex changed files on the fly; `files` = `[{path, language, code}]`. |
| `delete_files(project_guid, paths)`   | `DELETE /projects/{guid}/files`  | Soft-delete stale chunks for removed/renamed files.          |
| `list_projects()`                     | `GET /projects`                  | Summary counts per project (GUID-only identity).             |
| `project_stats(project_guid)`         | `GET /projects/{guid}`           | Per-language file/chunk stats.                               |
| `health()`                            | `GET /health`                    | On-demand liveness check; errors if mindex is unreachable.   |

Whole-project hard delete (`DELETE /projects/{guid}`) and `POST /gc` are
deliberately **not** exposed — too easy to misfire / not needed for search
correctness.

## Prerequisites

The MCP server is a thin adapter; the rest of mindex must already be running and the
project must be indexed once:

1. **mindex is up** — the Rust server plus its Qdrant and embedder are running and
   reachable at `MINDEX_SERVER` (default `https://127.0.0.1:11111`). See the
   [root README → Running](../../README.md#running) for bringing those up.
2. **The project is indexed at least once** — you've run `tools/indexer` over the
   repo, so there are chunks to search. The first full index is a CLI job;
   reindexing as you edit is then handled live through the `index_files` tool.

## Setup

**1 — Record the project GUID in a `.mindex` file.** mindex has no stored
GUID→project mapping; a project *is* just the GUID you chose when indexing. The MCP
tools need it on every call, so keep it in a `.mindex` file at the repo root and let
the client read it at the start of a session:

```sh
# in the repo you want searchable — generate a GUID and save it
uuidgen | tr -d '\n-' > .mindex
echo '.mindex' >> .gitignore        # per-checkout, environment-specific — don't commit
```

Then index the repo **with that same GUID** so `.mindex` and the index stay in sync:

```sh
tools/indexer/target/release/mindex-index \
    --project "$(cat .mindex)" --root . --no-verify \
    --include 'src/**' --exclude 'tools/**'
```

If `.mindex` is missing at session start, the server's instructions tell the agent to
ask you for the GUID rather than guess.

**2 — Install the server** (Poetry, like `embedder/`):

```sh
cd tools/mcp/mindex
poetry install
```

**3 — Register it with your MCP client.** For Claude Code, `claude mcp add` wraps the
launch command; run it through Poetry so it uses this project's venv, and use an
absolute path to `tools/mcp/mindex`:

```sh
claude mcp add mindex \
  --env MINDEX_NO_VERIFY=1 \
  -- poetry -C /data/silencespeakstruth/Projects/mindex/tools/mcp/mindex run mindex
```

**4 — Verify.** `claude mcp list` should show `mindex` connected. In a session the
`search` tool is then available — the agent reads the GUID from `.mindex` and passes
it with each query.

## The `.mindex` file

- **First non-comment, non-blank line:** the project's GUID in the simple,
  un-hyphenated form the indexer stores (`uuidgen | tr -d '\n-'`). `#`-comment and
  blank lines are ignored.
- **At the repo root**, and **gitignored** — it ties *this checkout* to *its mindex
  project*, which is environment-specific, not shared.
- The GUID in `.mindex` **must equal** the one passed to `tools/indexer --project`.
  A mismatch points the tools at a different (likely empty) project and search
  silently returns nothing.
- It is the single source of truth for identity: every MCP tool call takes the GUID
  from here.
- **Optional standing scope** (lines after the GUID), read by the client and passed as
  `include`/`exclude` on each `search` call:

  ```
  c2d7e2c1316542f593660ff1492b4bab
  exclude_paths: tools/**
  include_paths: src/**, embedder/**
  languages:     rust
  ```

  `exclude_paths`/`include_paths` are comma-separated globs; `languages` are
  comma-separated mindex language ids. All are optional — a bare GUID file behaves
  exactly as before.

## Scoping search (`include`/`exclude`)

`search` (and `scout`'s `digest`) take optional `include`/`exclude` filters,
each a `{"paths": [...], "programming_languages": [...]}` object passed straight
through to mindex's `/search`:

- `include={"programming_languages": ["rust"]}` — only Rust chunks.
- `exclude={"paths": ["tools/**"]}` — skip the CLI/tooling tree.

They're optional and additive to the query, so omitting them searches the whole
project. The natural home for project-standing scope is the `.mindex` file above.

## Configuration (env vars)

Same `MINDEX_*` conventions as `tools/search/mindex-search.sh`. Pass them via
`claude mcp add --env …` so the client launches the server with them set:

| Variable           | Default                   | Meaning                                              |
| ------------------ | ------------------------- | ---------------------------------------------------- |
| `MINDEX_SERVER`    | `https://127.0.0.1:11111` | mindex server URL                                    |
| `MINDEX_PROTOCOL`  | `v0`                      | API version in the URL path                          |
| `MINDEX_NO_VERIFY` | *(off)*                   | truthy (`1`/`true`/`yes`/`on`) → skip TLS verify     |
| `MINDEX_CACERT`    | *(unset)*                 | path to a CA bundle for the self-signed cert         |

Because mindex serves a self-signed cert, set **one** of `MINDEX_NO_VERIFY=1`
or `MINDEX_CACERT=/path/to/cert` — otherwise TLS verification fails.

## How an agent should use it

The whole point is cheap understanding, so the workflow is:

- **Search first, read second.** To understand part of the codebase, issue a few
  focused `search` queries and read the returned chunks — don't pull whole files into
  context. Refine follow-up queries from what earlier ones surfaced.
- **Keep the index live, cheaply.** After editing a file, call `index_files` for just
  that file with its current contents, passed **verbatim**. Reindexing is
  intentionally cheap — mindex skips unchanged files by hash, server-side — so it is
  fine to call often and "without thinking", with no investigation first. After
  deleting or renaming, call `delete_files` with the **old** paths (for a rename, also
  `index_files` the new path).
- **Don't bulk-reindex through this tool.** `index_files` carries full file bodies in
  the request, so it is for the handful of files you just touched — *not* for
  "(re)index the whole tree". For a full (re)index, or to apply path excludes (e.g.
  `--exclude 'tools/**'`), use the `tools/indexer` CLI, which walks the tree and
  hash-skips server-side without sending file bodies through the model.

The top-5 result cap (`TOP_K`) is fixed in the adapter — it is the context budget and
the model cannot raise it; prefer several focused queries over one broad one.

## Robustness: no network at startup

The MCP handshake spawns the process and lists tools without touching mindex — the
server connects fine even when mindex / Qdrant / the embedder are down. A tool call
made while mindex is unreachable returns a clean error fast (connection refused is
immediate, not a timeout); nothing is mutated. The handshake is one-time, so it is
deliberately *not* gated on mindex liveness — that wouldn't protect against a
mid-session shutdown and would hide the tools if the IDE starts before mindex.
Mid-session liveness is checked on demand via `health()`; the server's `instructions`
(sent at handshake) tell the client to stop and report on a connection error rather
than retry.

## Path contract

`index_files`/`delete_files` paths must be repo-root-relative with forward slashes,
*exactly* as the indexer stored them (it strips the `--root` prefix). A mismatched
path reindexes to a duplicate instead of updating in place.
