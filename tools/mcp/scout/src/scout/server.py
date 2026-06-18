"""scout MCP server — one tool, ``digest``, that keeps raw code off the
caller's context window.

The caller (a strong model) does the cheap part — **query decomposition**: it
sends a handful of short natural-language sub-queries (a few dozen tokens total).
This server does the expensive bulk part **off the caller's path**:

    decomposed queries  →  concurrent mindex /search  →  dedup + glue (in Python)
                        →  local LLM (Ollama) summarises  →  digest + source list

The raw chunks travel mindex → this process → the local LLM; they **never** cross
the MCP boundary back to the caller. Only the compact summary plus ``file:line``
source pointers return. That is the whole point: the model pays for crumbs in and
a compact digest out, while the slow/large work runs on local hardware for free.

Division of labour: the strong model *plans* the retrieval (decomposition); the
cheap local model *digests* the mass. This server is deliberately a sibling of
``tools/mcp/mindex`` (raw search) — it does not replace it. When the caller needs exact
code to *edit*, it should use the raw ``search`` tool; ``digest`` is for cheap
orientation ("how does X work / where does Y live"), and its source pointers are
the path back to ground truth.

mindex is untouched: this only consumes its existing HTTP ``/search`` API.
"""

from __future__ import annotations

import asyncio
import os
from typing import Any

import httpx
from mcp.server.fastmcp import FastMCP

# ── mindex (search source) — same MINDEX_* conventions as tools/mcp/mindex & search.sh ──
SERVER = os.environ.get("MINDEX_SERVER", "https://127.0.0.1:11111").rstrip("/")
PROTOCOL = os.environ.get("MINDEX_PROTOCOL", "v0")

# ── Ollama (the cheap digester) ──
OLLAMA_HOST = os.environ.get("OLLAMA_HOST", "http://localhost:11434").rstrip("/")
DIGEST_MODEL = os.environ.get("DIGEST_MODEL", "qwen2.5:14b")

# Chunks pulled per sub-query from mindex (raw, never seen by the caller)...
PER_QUERY_K = int(os.environ.get("DIGEST_PER_QUERY_K", "6"))
# ...then capped (after dedup, by score) before going to the local LLM, so a wide
# decomposition can't blow the digester's context or stall generation.
MAX_CHUNKS = int(os.environ.get("DIGEST_MAX_CHUNKS", "32"))
# Ollama context window for the digest pass. It MUST hold MAX_CHUNKS chunks (each
# ≤512 tokens by the slicer) plus system/query/answer overhead, or Ollama silently
# truncates the prompt — and truncation drops the *lowest-scored* chunks, exactly
# the long-tail must-haves that raising MAX_CHUNKS was meant to keep. Budget rule:
# ≈ MAX_CHUNKS × ~540 + ~1.5k headroom (32 → ~24k). Larger = more RAM/VRAM and
# slower generation (markedly so on CPU). For MAX_CHUNKS=64, raise this to ~32768.
NUM_CTX = int(os.environ.get("DIGEST_NUM_CTX", "24576"))

_TRUTHY = {"1", "true", "yes", "on"}


def _verify() -> bool | str:
    """TLS verification for mindex, mirroring tools/mcp/mindex: a CA-bundle path if
    ``MINDEX_CACERT`` is set, else off when ``MINDEX_NO_VERIFY`` is truthy (the
    self-signed cert), else on. (Ollama is plain HTTP, so this only affects mindex.)"""
    cacert = os.environ.get("MINDEX_CACERT")
    if cacert:
        return cacert
    if os.environ.get("MINDEX_NO_VERIFY", "").lower() in _TRUTHY:
        return False
    return True


def _filters(
    include: dict[str, Any] | None, exclude: dict[str, Any] | None
) -> dict[str, Any]:
    """Build the optional ``include``/``exclude`` portion of a ``/search`` body.

    Each is a SearchFilter dict — ``{"paths": [...], "programming_languages": [...]}``
    — passed straight through to mindex (the backend already supports both). Sent only
    when truthy, so an unscoped digest is byte-for-byte unchanged."""
    out: dict[str, Any] = {}
    if include:
        out["include"] = include
    if exclude:
        out["exclude"] = exclude
    return out


async def _search_one(
    client: httpx.AsyncClient, project_guid: str, query: str, filters: dict[str, Any]
) -> list[dict]:
    """One mindex search. 404 = empty candidate set (a normal 'no results')."""
    url = f"{SERVER}/{PROTOCOL}/{project_guid}/search"
    try:
        resp = await client.post(
            url, json={"query": query, "top_k": PER_QUERY_K, **filters}
        )
    except httpx.RequestError as e:
        raise RuntimeError(
            f"mindex search {url} failed ({e}) — is mindex reachable?"
        ) from e
    if resp.status_code == 404:
        return []
    resp.raise_for_status()
    return resp.json().get("results", [])


def _dedup(results: list[list[dict]]) -> list[dict]:
    """Flatten the per-query result lists into one set, deduped on chunk identity
    (path + line span — the same chunk surfaces under several sub-queries), keeping
    the highest score seen, then sorted best-first and capped to MAX_CHUNKS."""
    best: dict[tuple[str, int, int], dict] = {}
    for group in results:
        for r in group:
            key = (r["path"], r["start_line"], r["end_line"])
            if key not in best or r["score"] > best[key]["score"]:
                best[key] = r
    ordered = sorted(best.values(), key=lambda r: r["score"], reverse=True)
    return ordered[:MAX_CHUNKS]


_SYSTEM = (
    "You are a code-comprehension assistant for a senior engineer. You are given a "
    "set of code chunks retrieved from a repository, each tagged with its source as "
    "[path:start-end]. Write a concise, factual briefing that answers the engineer's "
    "queries using ONLY these chunks. Explain how the relevant pieces fit together. "
    "Cite the source tag [path:start-end] after each claim so the engineer can open "
    "the exact code. Do not invent code, APIs, or behaviour that is not in the chunks; "
    "if the chunks don't cover something, say so. Be terse — no preamble, no restating "
    "the queries."
)


def _build_user_prompt(queries: list[str], chunks: list[dict]) -> str:
    parts = ["Queries:"]
    parts.extend(f"  - {q}" for q in queries)
    parts.append("\nRetrieved chunks:\n")
    for c in chunks:
        tag = f"{c['path']}:{c['start_line']}-{c['end_line']}"
        parts.append(f"[{tag}] (score {c['score']:.3f})\n{c['code']}\n")
    return "\n".join(parts)


async def _ollama_digest(prompt: str) -> str:
    """Send the glued chunks to the local model and return its summary text."""
    url = f"{OLLAMA_HOST}/api/chat"
    payload = {
        "model": DIGEST_MODEL,
        "messages": [
            {"role": "system", "content": _SYSTEM},
            {"role": "user", "content": prompt},
        ],
        "stream": False,
        # Low temperature: this is extraction/summarisation, not creative writing.
        # num_ctx must fit MAX_CHUNKS worth of code or Ollama truncates the prompt
        # (silently dropping the lowest-scored, long-tail chunks).
        "options": {"temperature": 0.1, "num_ctx": NUM_CTX},
    }
    # Generation on a 14B local model is the slow leg — be generous.
    try:
        async with httpx.AsyncClient(timeout=600.0) as client:
            resp = await client.post(url, json=payload)
    except httpx.RequestError as e:
        raise RuntimeError(
            f"Ollama digest {url} failed ({e}) — is Ollama running and is "
            f"DIGEST_MODEL '{DIGEST_MODEL}' pulled? (check `ollama list`)"
        ) from e
    resp.raise_for_status()
    return resp.json().get("message", {}).get("content", "").strip()


_INSTRUCTIONS = """\
scout is a TOKEN-ECONOMY optimisation. Its whole purpose is to let you
understand parts of a codebase while spending as few of your own (expensive) tokens
as possible: you send a few short sub-queries, a cheap local LLM reads the matching
code for you, and only a compact summary plus `file:line` pointers come back. The
raw code never enters your context — that is where the saving comes from. In typical
use this offload cuts the context spent on a survey by roughly an order of magnitude
versus pulling the equivalent raw chunks.

You are the orchestrator; the cost/quality call is YOURS. Two regimes, from real use:

  - ORIENTATION / BREADTH — "how does X work", "where is Y configured", "what touches
    Z", surveying unfamiliar code. Reach for `digest` first; this is where it pays off
    most and usually needs no follow-up. Crumbs in, one briefing out, bulk reading
    free on local hardware.

  - IMPLEMENTATION / EDIT — you must hit a *complete* set of call-sites and copy exact
    patterns. Digest first to map the area, but do NOT treat the summary as complete:
    the score-capped glue can drop a long-tail must-have chunk, and the cheap model
    can misattribute (e.g. cite a test helper as the production pattern). So escalate
    raw `search` for every required aspect the summary did not explicitly cover; treat
    a "the chunks don't cover X" admission as a precise escalation cue, not noise; and
    never copy code structure from the digest prose — confirm exact code with raw
    `search` before editing.

Treat it as a funnel: digest broad and cheap to locate what matters, then spend real
tokens narrowly via raw `search`. digest = cheap breadth; raw `search` = paid
precision — two halves of one workflow. The saving only materialises when you route
the broad part here instead of reading raw chunks by reflex. A vague digest means
refine your sub-queries, not fall back to raw files wholesale.

How to call it: break your intent into 2-4 short natural-language sub-queries (query
decomposition — the cheap part, which you are good at) and pass them as `queries`.
Pass the project's GUID from the repo-root `.mindex` file, same as the mindex search
tools. If a call returns a connection error, a dependency (mindex or Ollama) is
down — report it and stop, don't retry blindly.
"""

mcp = FastMCP("scout", instructions=_INSTRUCTIONS)


@mcp.tool()
async def digest(
    project_guid: str,
    queries: list[str],
    include: dict[str, Any] | None = None,
    exclude: dict[str, Any] | None = None,
) -> dict:
    """Cheap codebase orientation — spend a few tokens instead of many.

    A TOKEN-SAVING optimisation: a local LLM reads the matching code for you and
    returns only a compact summary plus source pointers, so raw code stays OUT of
    your context. Use it as the cheap, broad first pass — then, where detail earns
    the tokens, follow the cited `sources` with the raw `search` tool for verbatim
    code (e.g. before editing). digest = cheap breadth; raw search = paid precision;
    use the two together. You decide the cost/quality balance per question.

    For pure understanding one digest usually suffices. For an edit/implementation
    task treat the summary as a map, not the full answer: it can omit a long-tail
    must-have chunk or misattribute a pattern, so escalate raw `search` for any
    required detail it doesn't explicitly cover (a "not shown" admission is a cue),
    and confirm exact code before copying it.

    Pass 2-4 short sub-queries (your decomposition of one intent). The server
    searches mindex for each concurrently, dedups and glues the raw chunks in a
    local process, and the local model summarises them.

    Args:
        project_guid: The project's mindex GUID (from the repo-root .mindex file).
        queries: A few short natural-language sub-queries (your query decomposition).
        include: Optional scope to KEEP, applied to every sub-query, as
            ``{"paths": ["src/**", ...], "programming_languages": ["rust", ...]}``;
            either key may be omitted. Standing scope can live in the repo-root
            `.mindex` file. Omit entirely (the default) to survey the whole project.
        exclude: Optional scope to DROP, same shape as ``include`` (e.g.
            ``{"paths": ["tools/**"]}``).

    Returns ``{"summary", "sources": [{path, start_line, end_line, score}], "queries"}``.
    ``summary`` is empty and ``sources`` is ``[]`` when nothing matched.
    """
    clean = [q.strip() for q in queries if q and q.strip()]
    if not clean:
        raise ValueError("queries must contain at least one non-empty string")

    filters = _filters(include, exclude)
    async with httpx.AsyncClient(verify=_verify(), timeout=60.0) as client:
        groups = await asyncio.gather(
            *(_search_one(client, project_guid, q, filters) for q in clean)
        )

    chunks = _dedup(groups)
    if not chunks:
        return {"summary": "", "sources": [], "queries": clean}

    summary = await _ollama_digest(_build_user_prompt(clean, chunks))
    sources = [
        {
            "path": c["path"],
            "start_line": c["start_line"],
            "end_line": c["end_line"],
            "score": c["score"],
        }
        for c in chunks
    ]
    return {"summary": summary, "sources": sources, "queries": clean}


def main() -> None:
    mcp.run()  # stdio transport (default)


if __name__ == "__main__":
    main()
