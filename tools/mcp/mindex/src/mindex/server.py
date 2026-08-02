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

import json as _json
import os
import shutil
import subprocess
from typing import Any

import httpx
from mcp.server.fastmcp import FastMCP

# Hard cap on returned chunks. The context budget lives here, in the adapter —
# the model cannot raise it. Keeps many cheap queries from flooding the context.
TOP_K = 5

# Hard cap on `symbols` rows per role (definitions / references). Same philosophy
# as TOP_K, but symbol rows are one-line pointers, not code chunks, so the cap is
# higher; the server reports full totals so truncation stays visible.
SYMBOL_LIMIT = 10

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
    return os.environ.get("MINDEX_NO_VERIFY", "").lower() not in _TRUTHY


def _headers() -> dict[str, str]:
    """``X-Api-Key`` when ``MINDEX_API_KEY`` is set, nothing otherwise.

    mindex has no authentication of its own and ignores the header; it exists for
    a reverse proxy in front of it (the nginx gate) that refuses requests without
    a known key. Unset means "talking to mindex directly", the local default."""
    key = os.environ.get("MINDEX_API_KEY")
    return {"X-Api-Key": key} if key else {}


def _request(
    method: str, path: str, *, json: Any = None, timeout: float = 30.0
) -> httpx.Response:
    """Single HTTP round trip to mindex. The only place that touches the network."""
    url = f"{SERVER}{path}"
    try:
        return httpx.request(
            method,
            url,
            json=json,
            headers=_headers(),
            verify=_verify(),
            timeout=timeout,
        )
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

FIRST, THE DIVISION OF LABOUR. Understanding this codebase is NOT your job and
NOT these tools' job — it belongs to the `scout` server's `research` tool, which
runs the whole investigation on local hardware and hands you a cited report for
the price of one question. Send every "how does X work / why / what would this
change touch / where does this behaviour come from" question there, trust the
report it returns, and send follow-up questions there too when it falls short.
Never run your own investigation loop instead.

That makes THESE tools the narrow, paid half of the workflow. Use `search` and
`symbols` when you need the byte-exact text of a specific place — typically one
a research report already cited and you are about to edit — not to explore, not
to survey, and not to double-check a report you were given. (Only if the `scout`
server is unavailable in this session do these become your means of
understanding as well; say so when that happens.)

Project identity: there is no stored GUID->project mapping. Read the target
project's GUID from the `.mindex` file at the repo root — YAML, committed, with a
`guid:` key — and pass it to every tool. If it's missing, ask the user for the GUID.

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

Between these two: `symbols` is the exact-name lookup (definitions + references
with kinds and enclosing scopes) — use it when you have the identifier and want
its precise location, e.g. to open the definition a report cited. Expect
candidate lists, not a single answer: an exact name can be defined in several
places; disambiguate by the returned paths/kinds/parents (`anchor_path` ranks
the file you are working in first). `search` is for when you have no name, only
a description — but if you find yourself issuing several searches in a row to
piece something together, stop: that is an investigation, and it belongs to
`scout`'s `research`.

Trusting search: if you suspect the index is out of date (files changed outside
this session, or you're starting a task and want to be sure), call `drift` — it
reports which files are stale/missing/orphaned vs the working tree. Act only on
stale/missing/orphaned; files reported `indexing` are in flight, so leave them —
unless one is a file you no longer want indexed, in which case call
`cancel_indexing` with a selector to abort that in-flight work (best-effort: a
file that already finished is left as-is).

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
    """Fetch verbatim code you already know you need — NOT a way to investigate.

    Returns up to 5 code chunks ranked by relevance, each with its file path,
    line range, and score.

    This is the paid, narrow half of the workflow: use it to pull the exact text
    of a place you can already name — typically one a `scout` `research` report
    cited and you are about to edit. Understanding questions ("how does X work",
    "what touches Y") go to `scout`'s `research` tool, which does the whole
    investigation on local hardware and returns a cited report for the price of
    one question. If you are about to fire a *sequence* of searches to piece
    something together, ask `research` instead — that sequence is exactly what
    it runs for free, and running it here spends your context for nothing.

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
def symbols(
    project_guid: str,
    name: str,
    role: str | None = None,
    kind: str | None = None,
    anchor_path: str | None = None,
    include: dict[str, Any] | None = None,
    exclude: dict[str, Any] | None = None,
) -> dict:
    """Exact-name symbol lookup: where is `name` defined, and who references it.

    Use it when you already have the identifier and want its precise location —
    e.g. to open a definition a `scout` `research` report cited. It reads the
    definitions/references extracted at indexing time (tree-sitter tags), so it
    is exact on the name and far cheaper than semantic ``search`` or grepping.
    It answers *where*, not *how* or *why*: those are research questions and
    belong to `scout`'s `research`, not to a chain of lookups here.

    It is purely syntactic: expect
    *candidate lists*, never a single guaranteed answer — the same name can be
    defined in several modules. Disambiguate via the returned ``kind`` (function,
    method, class, call, ...), ``parent_name``/``parent_kind`` (the enclosing
    definition), ``doc``, and paths; pass ``anchor_path`` to rank candidates in
    the file you are working in (then its directory) first.

    Up to 10 rows per role are returned; ``total_definitions``/``total_references``
    always carry the full counts, so a truncated list is visible — narrow with
    ``role``/``kind``/``anchor_path`` if you need the long tail. Empty lists are a
    definitive "this project has no such symbol" (languages without an upstream
    tags query contribute no symbols). Follow up with ``search`` scoped to the
    returned path when you need the surrounding code itself.

    Args:
        project_guid: The project's mindex GUID (from the repo-root .mindex file).
        name: Exact symbol name, case-sensitive (e.g. ``collection_for``).
        role: Optional ``"definition"`` or ``"reference"`` to fetch one side only.
        kind: Optional tags kind filter (``function``, ``method``, ``class``,
            ``call``, ...).
        anchor_path: Optional repo-root-relative path used ONLY for ranking
            (same file first, then same directory), never for filtering.
        include: Optional selector that FILTERS (unlike ``anchor_path``), as
            ``{"paths": [...], "programming_languages": [...]}`` — same shape as
            ``search``'s. Use it when a common name collides across subtrees.
            Occurrences it drops are still counted, as
            ``out_of_scope_definitions``/``out_of_scope_references``, so a filtered
            empty list stays distinguishable from "no such symbol".
        exclude: Optional selector to drop, same shape.
    """
    body: dict[str, Any] = {
        "name": name,
        "limit": SYMBOL_LIMIT,
        **_filters(include, exclude),
    }
    if role:
        body["role"] = role
    if kind:
        body["kind"] = kind
    if anchor_path:
        body["anchor_path"] = anchor_path
    resp = _request("POST", f"/{PROTOCOL}/{project_guid}/symbols", json=body)
    resp.raise_for_status()
    return resp.json()  # type: ignore[no-any-return]


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
    for paths in resp.json().get("files", {}).values():
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
def cancel_indexing(
    project_guid: str,
    include: dict[str, Any] | None = None,
    exclude: dict[str, Any] | None = None,
) -> dict:
    """Cancel in-flight indexing for the files matching a selector (best-effort).

    Use this when a file you no longer want is being indexed *right now* — e.g.
    ``drift`` reports it in the ``indexing`` bucket but it should be excluded. Only
    files currently in ``indexing`` are affected: their chunks are dropped and the
    file is marked ``cancelled`` (GC reclaims any vectors). A file that already
    finished indexing is left untouched, so a cancellation that arrives too late is
    simply a no-op — there is no way to un-index a completed file (use
    ``delete_files`` for that).

    The selector is the same shape as ``search``/``drift``; at least one of
    ``include``/``exclude`` must be non-empty (an empty body is rejected so it can't
    blanket-cancel the whole project).

    Args:
        project_guid: The project's mindex GUID.
        include: Optional scope to KEEP, e.g. ``{"paths": ["src/**", ...]}``.
        exclude: Optional scope to DROP, e.g. ``{"paths": ["tools/**", ...]}``.

    Returns ``{"cancelled_files": n}``.
    """
    resp = _request(
        "POST", f"/projects/{project_guid}/cancel", json=_filters(include, exclude)
    )
    if resp.status_code == 204:  # nothing was indexing under the selector
        return {"cancelled_files": 0}
    resp.raise_for_status()
    return {"cancelled_files": resp.json().get("cancelled_files", 0)}


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
    """The project's language inventory and file counts by status.

    Returns ``files`` (counts by status) plus ``languages``, keyed by lowercase
    language name, each ``{files, indexed_files, chunks_active, chunks_deleted}``.
    ``chunks_deleted`` is soft-deleted-but-not-yet-collected, not a loss.

    A language with ``files > 0`` and ``chunks_active == 0`` is indexed but
    *unsearchable* — every one of its files failed or sliced to nothing. That is a
    different answer from the language being absent from the map, which means the
    project contains no such file at all.

    Args:
        project_guid: The project's mindex GUID.
    """
    resp = _request("GET", f"/projects/{project_guid}")
    resp.raise_for_status()
    return resp.json()


@mcp.tool()
def drift(
    project_guid: str,
    root: str = ".",
    include: dict[str, Any] | None = None,
    exclude: dict[str, Any] | None = None,
) -> dict:
    """Report whether the indexed copy has drifted from the working tree on disk.

    Use this to know if mindex search results can be trusted before you rely on
    them — e.g. at the start of a task, or after files changed outside this session.
    It walks the local tree, hashes each file, and compares against the index,
    returning four buckets:

        - ``stale``:    indexed but the file changed (search returns old code) →
                        reindex via ``index_files`` (or the ``tools/indexer`` CLI).
        - ``missing``:  on disk but not indexed → index it.
        - ``orphaned``: indexed but gone from disk → ``delete_files`` the path.
        - ``indexing``: being indexed right now → **do nothing**, re-check later
                        (re-triggering it races the in-flight job). The exception:
                        if the file is one you no longer want indexed, call
                        ``cancel_indexing`` with a selector to abort it. Otherwise
                        act only on stale/missing/orphaned.

    This shells out to the ``mindex-index`` CLI (``--check``), which is the single
    implementation of the walk+hash, so it must be on ``PATH``. The ``paths`` in
    ``include``/``exclude`` scope the walk and **must match how the project was
    indexed**, otherwise correctly-indexed files look orphaned. Passing neither is
    usually right: the CLI then applies the repo-root ``.mindex`` scope by itself,
    which is the same scope the indexing run used. An ``exclude`` here *replaces*
    ``exclude_paths`` rather than adding to it. ``programming_languages`` in a
    filter is ignored here (the CLI detects language by extension).

    Args:
        project_guid: The project's mindex GUID (from the repo-root .mindex file).
        root: Repo root to walk (default the current directory).
        include: Optional ``{"paths": ["src/**", ...]}`` to KEEP.
        exclude: Optional ``{"paths": ["tools/**", ...]}`` to DROP.
    """
    binary = shutil.which("mindex-index")
    if binary is None:
        raise RuntimeError(
            "the `mindex-index` CLI is not on PATH — build tools/indexer and add it "
            "to PATH to use drift (search/index_files do not need it)."
        )

    cmd = [
        binary,
        "--check",
        "--json",
        "--project",
        project_guid,
        "--root",
        root,
        "--server",
        SERVER,
    ]
    # The CLI must reach the server on the same terms this process does, or `drift`
    # is the one tool that fails on a deployment where the others work.
    verify = _verify()
    if verify is False:
        cmd.append("--no-verify")
    elif isinstance(verify, str):
        cmd += ["--ca-cert", verify]
    for glob in (include or {}).get("paths") or []:
        cmd += ["--include", glob]
    for glob in (exclude or {}).get("paths") or []:
        cmd += ["--exclude", glob]

    try:
        # check=False: a non-zero exit is `--check`'s way of reporting drift, not a
        # failure. The exit code is deliberately ignored below in favour of stdout.
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=120, check=False
        )
    except subprocess.TimeoutExpired as e:
        raise RuntimeError("drift check timed out after 120s") from e

    # `--check` exits non-zero when it finds actionable drift, which is NOT an
    # error — so trust stdout: valid JSON means success regardless of exit code;
    # absent/garbage stdout means a real failure (use stderr for the reason).
    out = proc.stdout.strip()
    if not out:
        reason = proc.stderr.strip() or f"exit code {proc.returncode}"
        raise RuntimeError(f"drift check failed: {reason}")
    try:
        return _json.loads(out)  # type: ignore[no-any-return]
    except _json.JSONDecodeError as e:
        reason = proc.stderr.strip() or out[:200]
        raise RuntimeError(f"drift check produced unparseable output: {reason}") from e


@mcp.tool()
def health() -> dict:
    """Check whether mindex is reachable right now.

    Call this if a previous tool failed with a connection error, or before a batch
    of work, to confirm mindex is up. Returns the server's health report
    (sqlite/qdrant/embedder/ollama checks, status, files currently indexing).

    `status` is one of three words and they mean different things to you:
    `ok` — everything works. `degraded` — only the optional Ollama is down, so
    search and indexing still work and `/research` does not; keep using the other
    tools. `unhealthy` — a required dependency failed (or a research run is
    wedged) and nothing will work; tell the user which check reads `error`.

    Each check is exactly `"ok"` or `"error"`. The *reason* is deliberately not
    returned — it is in the server's own log — so do not speculate about the
    cause and do not paste the check value at the user as if it were one.

    If mindex itself is unreachable, this raises a clear error — treat that as
    "mindex is down": stop calling the other tools and tell the user.
    """
    resp = _request("GET", "/health", timeout=10.0)
    resp.raise_for_status()
    return resp.json()


def main() -> None:
    mcp.run()  # stdio transport (default)


if __name__ == "__main__":
    main()
