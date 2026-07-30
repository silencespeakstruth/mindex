# mindex — MCP server for search and symbols

A thin [MCP](https://modelcontextprotocol.io) stdio server putting the mindex index in a
coding agent's hands. **This is mindex's intended primary mode:** the agent asks the
index instead of reading files, so understanding the codebase costs a few chunks rather
than a few thousand lines of context.

Its sibling [`scout`](../scout/README.md) covers the other half — *research*, where a
local model investigates and the agent pays nothing. Rule of thumb the servers' own
instructions enforce: **understanding → `scout`; byte-exact code you are about to edit →
here.**

| Tool | Notes |
| --- | --- |
| `search(project_guid, query, include?, exclude?)` | Up to **5** ranked chunks. The cap is the context budget and the model cannot raise it. |
| `symbols(project_guid, name, role?, kind?, anchor_path?)` | Exact-name definitions/references (tree-sitter tags): kinds, enclosing scope, docs. Returns ranked **candidates** + full totals — a name can legitimately live in several places. `anchor_path` ranks your current file first. |
| `index_files(project_guid, files)` | Reindex the files you just touched, bodies **verbatim**. Unchanged content is hash-skipped server-side, so it is cheap to call often. |
| `delete_files(project_guid, paths)` | Soft-delete after a delete/rename (pass the OLD paths). |
| `drift(project_guid, root?, include?, exclude?)` | Is the index in sync? Returns `stale` / `missing` / `orphaned` / `indexing`. Needs `mindex-index` on `PATH`. |
| `cancel_indexing(project_guid, include?, exclude?)` | Abort in-flight indexing for a selector (best-effort; only `indexing` files). |
| `list_projects()` · `project_stats(guid)` · `health()` | Read-only introspection. |

Whole-project delete and `POST /gc` are deliberately **not** exposed.

## Setup

1. **mindex is running and the project is indexed once** (root README). Bulk indexing is
   the `mindex-index` CLI's job — never a loop of `index_files`, which carries full file
   bodies through the model.
2. **Record the GUID** in a `.mindex` file at the repo root — mindex has no
   GUID→project mapping, so every tool call takes it from there:
   ```sh
   printf 'guid: %s\n' "$(uuidgen)" > .mindex
   ```
   Optional scope lists in that YAML (`exclude_paths:`, `include_paths:`, `languages:`)
   are read by the agent and passed as `include`/`exclude` filters.
3. **Install and register:**
   ```sh
   cd tools/mcp/mindex && poetry install
   claude mcp add mindex \
     -- poetry -C /abs/path/tools/mcp/mindex run mindex
   ```
   `claude mcp list` should show it connected.

## Configuration (env vars)

| Variable | Default | Meaning |
| --- | --- | --- |
| `MINDEX_SERVER` | `https://127.0.0.1:11111` | mindex server URL |
| `MINDEX_PROTOCOL` | `v0` | API version in the URL path |
| `MINDEX_CACERT` | *(unset)* | extra CA bundle to trust, on top of the OS store |
| `MINDEX_NO_VERIFY` | *(off)* | truthy → verify nothing (self-signed cert) |

Neither is needed when the server's CA is installed system-wide, which is what
mkcert and corporate roots do. Name the CA with `MINDEX_CACERT` when it is not;
reach for `MINDEX_NO_VERIFY` only for the self-signed certificate the container
generates on first start, which no CA vouches for. `MINDEX_CACERT` also reaches the
`mindex-index` process the `drift` tool shells out to, so all of this server's tools
succeed or fail together.

## What else you should know

- **Path contract.** `index_files`/`delete_files` paths are repo-root-relative with
  forward slashes, *exactly* as the indexer stored them. A different spelling creates a
  duplicate instead of updating in place.
- **`chunk_count == 0`** means the file sliced to no chunks (under the token floor) —
  not that it was unchanged. Unchanged files are absent from the response entirely.
- **No network at handshake.** The server connects even when mindex is down; a call then
  fails fast and cleanly. Check liveness on demand with `health()`.
- The agent-facing rules (when to use what, and not to investigate by hand) live in the
  server's `instructions`, sent at handshake — read `server.py` if you want to tune them.
