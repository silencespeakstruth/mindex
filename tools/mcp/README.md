# mindex-mcp — MCP server for mindex search

A thin [MCP](https://modelcontextprotocol.io) stdio server that exposes mindex
semantic code search to an MCP client (e.g. Claude Code). It is a sibling tool
like `tools/indexer` and `tools/search`: it does **not** touch the Rust service —
each tool call just hits the existing `POST /v0/{project}/search` endpoint.

Tools exposed:

| Tool                                  | mindex endpoint                  | Notes                                                        |
| ------------------------------------- | -------------------------------- | ------------------------------------------------------------ |
| `search(project_guid, query)`         | `POST /v0/{guid}/search`         | Up to **5** ranked chunks. The top-5 cap (`TOP_K`) is the context budget; the model can't raise it. |
| `index_files(project_guid, files)`    | `POST /v0/{guid}/index`          | Reindex changed files on the fly; `files` = `[{path, language, code}]`. |
| `delete_files(project_guid, paths)`   | `DELETE /projects/{guid}/files`  | Soft-delete stale chunks for removed/renamed files.          |
| `list_projects()`                     | `GET /projects`                  | Summary counts per project (GUID-only identity).             |
| `project_stats(project_guid)`         | `GET /projects/{guid}`           | Per-language file/chunk stats.                               |
| `health()`                            | `GET /health`                    | On-demand liveness check; errors if mindex is unreachable.   |

Whole-project hard delete (`DELETE /projects/{guid}`) and `POST /gc` are
deliberately **not** exposed — too easy to misfire / not needed for search
correctness.

**Path contract:** `index_files`/`delete_files` paths must be repo-root-relative
with forward slashes, *exactly* as the indexer stored them (it strips `--root`).
A mismatched path reindexes to a duplicate instead of updating in place.

**No network at startup:** the MCP handshake spawns the process and lists tools
without touching mindex — the server connects fine even when mindex/qdrant/the
embedder are down. A tool call made while mindex is unreachable returns a clean
error fast (connection refused is immediate, not a timeout); nothing is mutated.
The handshake is one-time, so it is deliberately *not* gated on mindex liveness —
that wouldn't protect against a mid-session shutdown and would hide the tools if
the IDE starts before mindex. Mid-session liveness is checked on demand via
`health()`; the server's `instructions` (sent at handshake) tell the client to
stop and report on a connection error rather than retry.

**Finding the GUID:** there is no stored GUID→project mapping, so keep the GUID
in a repo-root `.mindex` file (gitignored) and read it at the start of a session;
pass it to the tools above.

## Install

Uses Poetry, like `embedder/`:

```sh
cd tools/mcp
poetry install
```

## Configuration (env vars)

Same `MINDEX_*` conventions as `tools/search/mindex-search.sh`:

| Variable           | Default                   | Meaning                                              |
| ------------------ | ------------------------- | ---------------------------------------------------- |
| `MINDEX_SERVER`    | `https://127.0.0.1:11111` | mindex server URL                                    |
| `MINDEX_PROTOCOL`  | `v0`                      | API version in the URL path                          |
| `MINDEX_NO_VERIFY` | *(off)*                   | truthy (`1`/`true`/`yes`/`on`) → skip TLS verify     |
| `MINDEX_CACERT`    | *(unset)*                 | path to a CA bundle for the self-signed cert         |

Because mindex serves a self-signed cert, set **one** of `MINDEX_NO_VERIFY=1`
or `MINDEX_CACERT=/path/to/cert` — otherwise TLS verification fails.

## Register with Claude Code

`claude mcp add` wraps the launch command; run the server through Poetry so it
uses this project's venv. Use an absolute path to `tools/mcp`:

```sh
claude mcp add mindex \
  --env MINDEX_NO_VERIFY=1 \
  -- poetry -C /data/silencespeakstruth/Projects/mindex/tools/mcp run mindex-mcp
```

Then in a Claude Code session the `search` tool is available; pass the project's
GUID and a query. Verify the connection with `claude mcp list`.
