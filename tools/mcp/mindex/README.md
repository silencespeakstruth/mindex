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
| `cancel_indexing(project_guid, include?, exclude?)` | `POST /projects/{guid}/cancel` | Best-effort cancel of **in-flight** indexing for files matching the selector (only `indexing` files; already-indexed files are left as-is). Same selector shape as `search`. |
| `drift(project_guid, root?, include?, exclude?)` | `POST /projects/{guid}/drift` (via the `mindex-index` CLI) | Is the index in sync with disk? Returns `stale`/`missing`/`orphaned`/`indexing` (see [Drift](#drift-is-the-index-in-sync)). Needs `mindex-index` on `PATH`. |
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
- **At the repo root**. It ties *this checkout* to *its mindex project* — gitignore
  it when that binding is environment-specific; commit it when the whole team (or a
  single-user setup, as in this repo) shares one index scope.
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

## Drift: is the index in sync?

`drift(project_guid, root?, include?, exclude?)` answers "can I trust search right
now?" — it walks the working tree, hashes each file, and compares against the index,
returning four buckets:

- **`stale`** — indexed but the file changed (search returns old code) → reindex.
- **`missing`** — on disk but not indexed → index it.
- **`orphaned`** — indexed but gone from disk → `delete_files` the path.
- **`indexing`** — being indexed right now → **do nothing**, re-check later;
  re-triggering an in-flight file just races the live job. The one exception: if an
  `indexing` file is one you no longer want indexed, call `cancel_indexing` with a
  selector to abort that in-flight work. Otherwise act only on the first three buckets.

`cancel_indexing(project_guid, include?, exclude?)` is best-effort and only touches
files currently in `indexing`: it drops their chunks and marks the file `cancelled`
(GC reclaims any vectors). A file that already finished indexing is left untouched, so
a too-late cancel is a no-op — use `delete_files` to remove a completed file. The live
`/index` request reconciles against the cancel before its embed pass, and the retry
worker re-checks status before re-driving a file, so a cancelled file is neither
re-embedded nor resurrected.

Unlike the other tools, `drift` shells out to the **`mindex-index` CLI** (`--check`),
which is the single implementation of the tree walk + hashing (so the MCP server never
re-implements globbing/hashing in Python). That means `mindex-index` must be on the
launch environment's `PATH` — e.g. `cargo install --path tools/indexer` or symlink the
release binary. If it is absent, `drift` returns a clear error and the other tools keep
working.

The `paths` in `include`/`exclude` scope the walk and **must match how the project was
indexed** (typically the `exclude_paths`/`include_paths` from `.mindex`) — otherwise
correctly-indexed files show up as `orphaned`. `programming_languages` in a filter is
ignored here (the CLI detects language by file extension).

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
- **Cancel indexing you no longer want.** If `drift` reports a file as `indexing` but
  you've decided it shouldn't be indexed (e.g. it just landed under an exclude), call
  `cancel_indexing` with a selector to abort that in-flight work. It's best-effort and
  only touches files still `indexing`; a file that already finished is left as-is (use
  `delete_files` to remove a completed one).

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
