"""mindex MCP server — exposes mindex code search + maintenance as MCP tools.

A thin stdio adapter over mindex's HTTP API. The Rust service is untouched; this
is a sibling tool like ``tools/indexer`` and ``tools/search``. Each tool call
maps to one HTTP request — there is **no** network access at import or during the
MCP handshake, so the server connects fine even when mindex is down (a tool call
then simply returns a clean error). Configuration mirrors the ``MINDEX_*`` env
conventions of ``tools/search/mindex-search.sh``.

Path contract (important): paths passed to ``index_files``/``delete_files`` must be
repo-root-relative with forward slashes — *exactly* as the indexer stored them
(it strips the ``--root`` prefix). A mismatched path reindexes to a duplicate
instead of updating in place.
"""

from __future__ import annotations

import os
from typing import Any

import httpx
from mcp.server.fastmcp import FastMCP

# Hard cap on returned chunks. The context budget lives here, in the adapter —
# the model cannot raise it. Keeps many cheap queries from flooding the context.
TOP_K = 5

SERVER = os.environ.get("MINDEX_SERVER", "https://127.0.0.1:11111").rstrip("/")
PROTOCOL = os.environ.get("MINDEX_PROTOCOL", "v0")

_TRUTHY = {"1", "true", "yes", "on"}


def _verify() -> bool | str:
    """TLS verification, mirroring mindex-search.sh: a CA-bundle path if
    ``MINDEX_CACERT`` is set, else off when ``MINDEX_NO_VERIFY`` is truthy (for
    the self-signed cert), else on. Self-signed setups need one of the two."""
    cacert = os.environ.get("MINDEX_CACERT")
    if cacert:
        return cacert
    if os.environ.get("MINDEX_NO_VERIFY", "").lower() in _TRUTHY:
        return False
    return True


def _request(
    method: str, path: str, *, json: Any = None, timeout: float = 30.0
) -> httpx.Response:
    """Single HTTP round trip to mindex. The only place that touches the network."""
    url = f"{SERVER}{path}"
    try:
        return httpx.request(method, url, json=json, verify=_verify(), timeout=timeout)
    except httpx.RequestError as e:
        raise RuntimeError(
            f"mindex {method} {url} failed ({e}) — is the server reachable?"
        ) from e


def _filters(
    include: dict[str, Any] | None, exclude: dict[str, Any] | None
) -> dict[str, Any]:
    """Build the optional ``include``/``exclude`` portion of a ``/search`` body.

    Each is a SearchFilter dict — ``{"paths": [...], "programming_languages": [...]}``
    — passed straight through to mindex, whose ``/search`` already supports both. A
    filter is sent only when truthy, so a bare search is byte-for-byte unchanged and
    empty dicts are dropped."""
    out: dict[str, Any] = {}
    if include:
        out["include"] = include
    if exclude:
        out["exclude"] = exclude
    return out


_INSTRUCTIONS = """\
mindex is a local semantic code-search index. These tools wrap its HTTP API.

Project identity: there is no stored GUID->project mapping. Read the target
project's GUID from a `.mindex` file at the repo root (gitignored) and pass it to
every tool. If it's missing, ask the user for the GUID.

Keeping the index live: after you create or modify source files, call
`index_files` for them so search stays accurate. After deleting or renaming
files, call `delete_files` with the OLD paths (for a rename, also `index_files`
the new path). Paths must be repo-root-relative with forward slashes, exactly as
originally indexed — a different spelling creates a duplicate instead of updating.

Reindexing is deliberately cheap — the server skips unchanged files by hash — so
call `index_files` freely, without preamble: do NOT investigate first (no
`project_stats`, no file-count reconciliation) and never read a file back just to
reindex it. Pass only `code` you already have in context from writing the file,
and pass it VERBATIM — never paraphrase, truncate, or placeholder it, which
overwrites the indexed copy with broken content. Use `index_files` only for the
files you touched this turn; to (re)index a whole tree, or to apply path excludes,
run the `tools/indexer` CLI instead — it walks the tree and hash-skips server-side
without sending file bodies through the model.

Availability: this server stays up even if mindex itself is stopped. If any tool
returns a connection error, mindex is unreachable — call `health()` to confirm,
then STOP issuing calls and tell the user, rather than retrying blindly. Don't
wait on it; report and move on.
"""

mcp = FastMCP("mindex", instructions=_INSTRUCTIONS)


@mcp.tool()
def search(
    project_guid: str,
    query: str,
    include: dict[str, Any] | None = None,
    exclude: dict[str, Any] | None = None,
) -> list[dict]:
    """Semantic code search over an indexed mindex project.

    Returns up to 5 code chunks ranked by relevance, each with its file path,
    line range, and score. Prefer several focused queries over one broad one,
    and refine follow-up queries based on what earlier ones surfaced.

    This is the precision half of the workflow — exact code to reason about, to
    edit, or to complete a `scout` briefing. For broad "how does X work"
    understanding, prefer `scout`'s `digest` tool: it offloads the bulk reading
    to a local model and spends roughly an order of magnitude fewer of your tokens.

    Args:
        project_guid: The project's mindex GUID (e.g. from a repo-root .mindex file).
        query: What to look for, in natural language or code terms.
        include: Optional scope to KEEP, as
            ``{"paths": ["src/**", ...], "programming_languages": ["rust", ...]}``;
            either key may be omitted. Standing scope can live in the repo-root
            `.mindex` file. Omit entirely (the default) to search the whole project.
        exclude: Optional scope to DROP, same shape as ``include`` (e.g.
            ``{"paths": ["tools/**"]}``).
    """
    resp = _request(
        "POST",
        f"/{PROTOCOL}/{project_guid}/search",
        json={"query": query, "top_k": TOP_K, **_filters(include, exclude)},
    )
    # 404 = empty candidate set (no active chunks match) — a normal "no results".
    if resp.status_code == 404:
        return []
    resp.raise_for_status()
    results = resp.json().get("results", [])[:TOP_K]
    return [
        {
            "path": r["path"],
            "start_line": r["start_line"],
            "end_line": r["end_line"],
            "score": r["score"],
            "code": r["code"],
        }
        for r in results
    ]


@mcp.tool()
def index_files(project_guid: str, files: list[dict]) -> list[dict]:
    """Reindex created or changed source files so search reflects the edit.

    Call this after you create or modify files in an already-indexed project
    (this is part of keeping the index live as you work). Unchanged content is
    skipped server-side by hash, so reindexing an untouched file is cheap and
    safe — call it freely, but only with content already in your context and
    passed VERBATIM (a paraphrased or truncated ``code`` overwrites the indexed
    file with broken content). For bulk (re)indexing of a whole tree, use the
    ``tools/indexer`` CLI, not a loop of these calls.

    Each entry in ``files`` is an object:
        - ``path``: repo-root-relative path, forward slashes, EXACTLY as it was
          originally indexed (a different spelling creates a duplicate, not an
          update).
        - ``language``: the mindex language id (e.g. "rust", "python", "go") —
          must match the file's actual language or the request is rejected.
        - ``code``: the file's full current contents.

    Returns one ``{path, chunk_count}`` per file. ``chunk_count == 0`` means the
    file sliced to no chunks (below the slicer's token floor), not that it was
    unchanged.

    Args:
        project_guid: The project's mindex GUID.
        files: List of {path, language, code} objects to (re)index.
    """
    payload: dict[str, dict] = {"files": {}}
    for f in files:
        payload["files"].setdefault(f["language"], {})[f["path"]] = {"code": f["code"]}
    # Embedding runs on the GPU and can take a while for a batch — generous timeout.
    resp = _request(
        "POST", f"/{PROTOCOL}/{project_guid}/index", json=payload, timeout=300.0
    )
    resp.raise_for_status()
    out: list[dict] = []
    for _lang, paths in resp.json().get("files", {}).items():
        for path, chunk_count in paths.items():
            out.append({"path": path, "chunk_count": chunk_count})
    return out


@mcp.tool()
def delete_files(project_guid: str, paths: list[str]) -> dict:
    """Remove stale chunks for files you deleted or renamed (soft delete).

    Call with the OLD paths after deleting or renaming files. Search stops
    returning them immediately (it filters to active chunks); physical removal
    happens later via the GC worker. For a rename, also call ``index_files`` with
    the NEW path.

    Paths are matched exactly (passed as the delete selector's path globs), so
    use the same repo-root-relative forward-slash spelling as when indexed.

    Args:
        project_guid: The project's mindex GUID.
        paths: Repo-root-relative paths to remove.

    Returns ``{"deleted_files": n}``.
    """
    if not paths:
        return {"deleted_files": 0}
    resp = _request(
        "DELETE", f"/projects/{project_guid}/files", json={"include": {"paths": paths}}
    )
    if resp.status_code == 204:  # selector matched nothing
        return {"deleted_files": 0}
    resp.raise_for_status()
    return {"deleted_files": resp.json().get("deleted_files", 0)}


@mcp.tool()
def list_projects() -> list[dict]:
    """List all indexed projects with summary counts.

    Returns one ``{project_guid, files, indexing, active_chunks}`` per project.
    Note: projects are identified only by GUID — there is no stored name/path, so
    use a repo's own .mindex file to know which GUID is the current project.
    """
    resp = _request("GET", "/projects")
    resp.raise_for_status()
    return resp.json().get("projects", [])


@mcp.tool()
def project_stats(project_guid: str) -> dict:
    """Per-language file/chunk statistics for one project.

    Returns file counts by status and active/deleted chunk counts per language.

    Args:
        project_guid: The project's mindex GUID.
    """
    resp = _request("GET", f"/projects/{project_guid}")
    resp.raise_for_status()
    return resp.json()


@mcp.tool()
def health() -> dict:
    """Check whether mindex is reachable right now.

    Call this if a previous tool failed with a connection error, or before a batch
    of work, to confirm mindex is up. Returns the server's health report
    (sqlite/qdrant/embedder checks, status, files currently indexing). If mindex
    itself is unreachable, this raises a clear error — treat that as "mindex is
    down": stop calling the other tools and tell the user.
    """
    resp = _request("GET", "/health", timeout=10.0)
    resp.raise_for_status()
    return resp.json()


def main() -> None:
    mcp.run()  # stdio transport (default)


if __name__ == "__main__":
    main()
