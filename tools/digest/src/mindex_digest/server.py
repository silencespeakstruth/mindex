"""mindex-digest MCP server — one tool, ``digest``, that keeps raw code off the
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
``tools/mcp`` (raw search) — it does not replace it. When the caller needs exact
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

# ── mindex (search source) — same MINDEX_* conventions as tools/mcp & search.sh ──
SERVER = os.environ.get("MINDEX_SERVER", "https://127.0.0.1:11111").rstrip("/")
PROTOCOL = os.environ.get("MINDEX_PROTOCOL", "v0")

# ── Ollama (the cheap digester) ──
OLLAMA_HOST = os.environ.get("OLLAMA_HOST", "http://localhost:11434").rstrip("/")
DIGEST_MODEL = os.environ.get("DIGEST_MODEL", "qwen2.5:14b")

# Chunks pulled per sub-query from mindex (raw, never seen by the caller)...
PER_QUERY_K = int(os.environ.get("DIGEST_PER_QUERY_K", "6"))
# ...then capped (after dedup, by score) before going to the local LLM, so a wide
# decomposition can't blow the digester's context or stall generation.
MAX_CHUNKS = int(os.environ.get("DIGEST_MAX_CHUNKS", "12"))

_TRUTHY = {"1", "true", "yes", "on"}


def _verify() -> bool | str:
    """TLS verification for mindex, mirroring tools/mcp: a CA-bundle path if
    ``MINDEX_CACERT`` is set, else off when ``MINDEX_NO_VERIFY`` is truthy (the
    self-signed cert), else on. (Ollama is plain HTTP, so this only affects mindex.)"""
    cacert = os.environ.get("MINDEX_CACERT")
    if cacert:
        return cacert
    if os.environ.get("MINDEX_NO_VERIFY", "").lower() in _TRUTHY:
        return False
    return True


async def _search_one(client: httpx.AsyncClient, project_guid: str, query: str) -> list[dict]:
    """One mindex search. 404 = empty candidate set (a normal 'no results')."""
    url = f"{SERVER}/{PROTOCOL}/{project_guid}/search"
    try:
        resp = await client.post(url, json={"query": query, "top_k": PER_QUERY_K})
    except httpx.RequestError as e:
        raise RuntimeError(f"mindex search {url} failed ({e}) — is mindex reachable?") from e
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
        "options": {"temperature": 0.1},
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
mindex-digest is a TOKEN-ECONOMY optimisation. Its entire purpose is to let you
understand parts of a codebase while spending as few of your own (expensive) tokens
as possible: you send a few short sub-queries, a cheap local LLM reads the matching
code for you, and only a compact summary plus `file:line` pointers come back. The
raw code never enters your context — that is where the saving comes from.

You are the orchestrator. The cost/quality trade-off is YOUR call to make on every
question, not a fixed rule:
  - Reach for `digest` first when the goal is orientation or breadth — "how does X
    work", "where is Y configured", "what touches Z", surveying an unfamiliar area.
    You pay for crumbs in and a digest out; the bulk reading runs free on local
    hardware.
  - Escalate to verbatim detail only where it earns the tokens. The digest cites
    `[path:start-end]` sources; when you actually need exact code to reason about or
    EDIT, follow those pointers with the raw `search` tool — and pull in only the
    specific chunks that matter, not the whole area.
Treat it as a funnel: digest broad and cheap to locate what matters, then spend
real tokens narrowly. A vague digest is a signal to refine your sub-queries, not to
immediately fall back to reading raw files.

Use it IN TANDEM with the mindex `search` tool — they are two halves of one
workflow, and the token saving only materialises when you actually route the cheap,
broad part here instead of reading raw chunks by reflex. `digest` = cheap breadth;
raw `search` = paid precision.

How to call it: break your intent into 2-4 short natural-language sub-queries (query
decomposition — the cheap part, which you are good at) and pass them as `queries`.
Pass the project's GUID from the repo-root `.mindex` file, same as the mindex search
tools. If a call returns a connection error, a dependency (mindex or Ollama) is
down — report it and stop, don't retry blindly.
"""

mcp = FastMCP("mindex-digest", instructions=_INSTRUCTIONS)


@mcp.tool()
async def digest(project_guid: str, queries: list[str]) -> dict:
    """Cheap codebase orientation — spend a few tokens instead of many.

    A TOKEN-SAVING optimisation: a local LLM reads the matching code for you and
    returns only a compact summary plus source pointers, so raw code stays OUT of
    your context. Use it as the cheap, broad first pass — then, where detail earns
    the tokens, follow the cited `sources` with the raw `search` tool for verbatim
    code (e.g. before editing). digest = cheap breadth; raw search = paid precision;
    use the two together. You decide the cost/quality balance per question.

    Pass 2-4 short sub-queries (your decomposition of one intent). The server
    searches mindex for each concurrently, dedups and glues the raw chunks in a
    local process, and the local model summarises them.

    Args:
        project_guid: The project's mindex GUID (from the repo-root .mindex file).
        queries: A few short natural-language sub-queries (your query decomposition).

    Returns ``{"summary", "sources": [{path, start_line, end_line, score}], "queries"}``.
    ``summary`` is empty and ``sources`` is ``[]`` when nothing matched.
    """
    clean = [q.strip() for q in queries if q and q.strip()]
    if not clean:
        raise ValueError("queries must contain at least one non-empty string")

    async with httpx.AsyncClient(verify=_verify(), timeout=60.0) as client:
        groups = await asyncio.gather(
            *(_search_one(client, project_guid, q) for q in clean)
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
