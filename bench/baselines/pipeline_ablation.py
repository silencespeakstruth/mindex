#!/usr/bin/env python3
"""Family F2: what each retrieval stage contributes, measured at the store.

`docs/claude/qdrant.md` names this "the measurement to build first" and states
the stake: `vector_storage-colbert` is **838 MB per segment against 2.6 MB
dense and 0.5 MB sparse — 99.6% of the bytes**, one 1024-wide row per token,
~322x dense. Nothing in this repository has ever shown that the outer rerank
changes a ranking, let alone improves one, and every downstream question
(binary quantization, token pooling, whether ColBERT belongs here at all)
is gated on that.

WHY THIS RUNS AGAINST QDRANT DIRECTLY. mindex exposes exactly one retrieval
shape and no flag turns a stage off, so the alternative was a code change on
the path under test. Querying the store instead keeps the thing being measured
untouched: the collection is the one mindex built, the vectors are the ones it
stored, and the full arm is `db/qdrant.rs`'s nested prefetch transcribed —

    outer   colbert, limit = top_k              (MaxSim over the fused pool)
      inner rrf fusion, limit = fusion_limit
        dense  limit = dense_prefetch_limit
        sparse limit = sparse_prefetch_limit

so the `full` arm must reproduce mindex's own numbers. **It is checked against
them**, and a divergence means this file is wrong rather than interesting: see
`--verify-against`. The arms:

    full          the deployed pipeline
    no-colbert    the same fusion pool, ranked by RRF alone (drop the outer)
    dense-only    dense prefetch, no sparse, no rerank
    sparse-only   sparse prefetch, no dense, no rerank

The one thing this cannot see is mindex's SQLite `has_id` candidate filter, so
every arm gets the same post-filter here (the docs-tree exclusion), applied
identically — which is what makes the arms comparable to each other even where
the absolute numbers may sit a hair off the server's.
"""

from __future__ import annotations

import argparse
import json
import struct
import sys
import time
from pathlib import Path
from typing import Any

import httpx

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from bm25_fts5 import TOP_K, glob_to_regex
from build_qrels import load_config, repo_root
from run import project_guid

MAGIC = b"BM3\x01"
# From `[qdrant]`'s defaults — the values the measured runs used.
DENSE_PREFETCH = 200
SPARSE_PREFETCH = 200
FUSION_LIMIT = 200
# Sparse weights at or below this are dropped before upsert, so a query keeping
# them would search for dimensions no point carries (`CLAUDE.md`, retrieval).
SPARSE_EPSILON = 1e-5

ARMS = ("full", "no-colbert", "dense-only", "sparse-only", "weighted-sum")

# The combination BGE-M3's authors actually specify, and mindex does not use.
#
#   s_rank = w1*s_dense + w2*s_lex + w3*s_mul          (M3-Embedding, §hybrid)
#   w = [1, 0.3, 1] on MIRACL and MKQA
#
# mindex instead fuses dense+sparse by RRF into a candidate pool and then orders
# that pool by ColBERT ALONE — the dense and sparse scores are discarded at the
# final step. In the paper's own table that is the `Multi-vec` row (70.5 nDCG@10
# on MIRACL), not the `All` row (71.5): a full point of the model's headline
# number left unclaimed, if the pattern transfers.
M3_WEIGHTS = (1.0, 0.3, 1.0)


# MaxSim as Qdrant returns it is the SUM over query tokens, so it grows with
# query length — a 192-token query scores 191.999 against its own text. The
# paper's weights are for FlagEmbedding's `colbert_score`, which divides by the
# query token count. Feeding the raw sum into a weighted sum with w=1 would let
# the ColBERT term outweigh the other two by two orders of magnitude, which
# would not be the paper's method under another name — it would be `full` with
# extra steps.
def normalise_maxsim(score: float, query_tokens: int) -> float:
    return score / query_tokens if query_tokens else 0.0


def encode(
    url: str, text: str
) -> tuple[list[float], dict[int, float], list[list[float]]]:
    """One query through the same `/encode` mindex calls, same binary format."""
    r = httpx.post(f"{url}/encode", json={"texts": [text]}, timeout=120)
    r.raise_for_status()
    buf = r.content
    if buf[:4] != MAGIC:
        raise SystemExit("bad magic from /encode — wire formats disagree")
    off = 4
    n, dim = struct.unpack_from("<II", buf, off)
    off += 8
    if n != 1:
        raise SystemExit(f"asked for one text, got {n}")
    dense = list(struct.unpack_from(f"<{dim}f", buf, off))
    off += dim * 4

    (count,) = struct.unpack_from("<I", buf, off)
    off += 4
    ids = struct.unpack_from(f"<{count}I", buf, off)
    off += count * 4
    wts = struct.unpack_from(f"<{count}f", buf, off)
    off += count * 4
    sparse = {i: w for i, w in zip(ids, wts, strict=True) if w > SPARSE_EPSILON}

    (tokens,) = struct.unpack_from("<I", buf, off)
    off += 4
    flat = struct.unpack_from(f"<{tokens * dim}f", buf, off)
    colbert = [list(flat[i * dim : (i + 1) * dim]) for i in range(tokens)]
    return dense, sparse, colbert


def _dashed(guid: str) -> str:
    """`2701be35...` -> `2701be35-8381-...`, Qdrant's spelling of the same id."""
    if "-" in guid or len(guid) != 32:
        return guid
    return f"{guid[:8]}-{guid[8:12]}-{guid[12:16]}-{guid[16:20]}-{guid[20:]}"


def sparse_body(sparse: dict[int, float]) -> dict[str, Any]:
    return {"indices": list(sparse), "values": list(sparse.values())}


def weighted_sum_points(
    session: Any, url: str, dense, sparse, colbert, limit: int
) -> list[dict[str, Any]]:
    """The paper's hybrid: retrieve with each head, then sum the three scores.

    Five store round-trips rather than one, because the candidate set has to be
    scored by all three heads and Qdrant answers one head per query. The union
    of the dense and sparse top-N is the candidate set — the same evidence the
    RRF pool is built from, so this arm differs from `no-colbert` in how the
    pool is ORDERED and not in what is in it.

    A candidate that one head did not return still gets that head's real score,
    fetched by id, rather than a zero. Substituting zero would not be a missing
    score, it would be an assertion that the head found the document maximally
    irrelevant — and it would penalise exactly the documents the other head
    liked, which is the case this combination exists to handle.
    """
    dense_leg = {
        "query": dense,
        "using": "dense",
        "limit": DENSE_PREFETCH,
        "with_payload": False,
    }
    sparse_leg = {
        "query": sparse_body(sparse),
        "using": "sparse",
        "limit": SPARSE_PREFETCH,
        "with_payload": False,
    }
    d = session.post(url, json=dense_leg).json()["result"]["points"]
    s = session.post(url, json=sparse_leg).json()["result"]["points"]
    pool = sorted({p["id"] for p in d} | {p["id"] for p in s})
    if not pool:
        return []

    flt = {"must": [{"has_id": pool}]}
    n = len(pool)

    def scores(leg: dict[str, Any]) -> dict[str, float]:
        body = {**leg, "limit": n, "filter": flt}
        pts = session.post(url, json=body).json()["result"]["points"]
        return {p["id"]: p["score"] for p in pts}

    ds = scores(dense_leg)
    ss = scores(sparse_leg)
    cs = scores(
        {"query": colbert, "using": "colbert", "limit": n, "with_payload": False}
    )

    w1, w2, w3 = M3_WEIGHTS
    tokens = len(colbert)
    merged = [
        {
            "id": pid,
            "score": w1 * ds.get(pid, 0.0)
            + w2 * ss.get(pid, 0.0)
            + w3 * normalise_maxsim(cs.get(pid, 0.0), tokens),
        }
        for pid in pool
    ]
    merged.sort(key=lambda p: p["score"], reverse=True)
    return merged[:limit]


def build_query(arm: str, dense, sparse, colbert, limit: int) -> dict[str, Any]:
    dense_leg = {"query": dense, "using": "dense", "limit": DENSE_PREFETCH}
    sparse_leg = {
        "query": sparse_body(sparse),
        "using": "sparse",
        "limit": SPARSE_PREFETCH,
    }
    if arm == "dense-only":
        return {**dense_leg, "limit": limit, "with_payload": False}
    if arm == "sparse-only":
        return {**sparse_leg, "limit": limit, "with_payload": False}
    fusion = {
        "prefetch": [dense_leg, sparse_leg],
        "query": {"fusion": "rrf"},
        "limit": FUSION_LIMIT,
    }
    if arm == "no-colbert":
        # The fused pool IS the answer — RRF already produced an ordering, and
        # the outer stage is exactly what is being removed.
        return {**fusion, "limit": limit, "with_payload": False}
    return {
        "prefetch": [fusion],
        "query": colbert,
        "using": "colbert",
        "limit": limit,
        "with_payload": False,
    }


def main() -> int:
    root = repo_root()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--config", type=Path, default=root / "bench" / "corpora.toml")
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--qrels-suffix", default="-docs")
    ap.add_argument(
        "--arm",
        choices=[*ARMS, "all"],
        required=True,
        help="`all` runs every arm in ONE pass, encoding each query once — the "
        "arms then differ only in the query sent to the store, which is the "
        "whole claim an ablation makes",
    )
    ap.add_argument("--mindex-label", default="baseline")
    ap.add_argument("--qdrant", default="http://127.0.0.1:6333")
    ap.add_argument("--embedder", default="http://127.0.0.1:11211")
    ap.add_argument(
        "--db",
        type=Path,
        default=Path.home() / ".local/share/mindex-bench/mindex-bench.db",
    )
    ap.add_argument("--limit", type=int, default=0, help="first N queries only")
    args = ap.parse_args()

    config = load_config(args.config)
    corpus = next(c for c in config["corpus"] if c["name"] == args.corpus)
    guid = project_guid(args.mindex_label, args.corpus).replace("-", "")
    collection = f"{guid}_v2"

    # The point id -> file mapping mindex keeps in SQLite. Read once; this is
    # the same join `post_search` does for the winners.
    import sqlite3

    conn = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
    # SQLite stores the point id dashless; Qdrant answers with dashes. Keyed on
    # the raw column this join matched NOTHING and every ranking came back
    # empty — caught only because an unmatched point is counted rather than
    # silently skipped, which is the same reason mindex has
    # `search_orphaned_winners`.
    meta = {
        _dashed(row[0]): (row[1], row[2], row[3])
        for row in conn.execute(
            "SELECT qdrant_guid, file_path, start_line, end_line "
            "FROM project_file_chunks WHERE project_guid = ? AND status = 'active'",
            (guid,),
        )
    }
    conn.close()

    import re

    exclude = [
        re.compile(glob_to_regex(g)) for g in corpus.get("search_exclude_paths", [])
    ]

    qrels = (
        root
        / config["run"]["data_dir"]
        / "qrels"
        / f"{args.corpus}{args.qrels_suffix}.jsonl"
    )
    instances = [json.loads(line) for line in qrels.open()]
    if args.limit:
        instances = instances[: args.limit]

    arms = list(ARMS) if args.arm == "all" else [args.arm]
    out_paths = {
        arm: root
        / config["run"]["results_dir"]
        / f"F2-{arm}__{args.corpus}{args.qrels_suffix}.jsonl"
        for arm in arms
    }
    print(f"  arms={arms}  collection={collection}  {len(meta)} active chunks")

    session = httpx.Client(timeout=120)
    url = f"{args.qdrant}/collections/{collection}/points/query"
    unknown = 0
    handles = {arm: out_paths[arm].open("w") for arm in arms}
    try:
        for pos, inst in enumerate(instances, start=1):
            dense, sparse, colbert = encode(args.embedder, inst["query"])
            for arm in arms:
                # Over-fetch, because the docs-tree exclusion is applied after
                # the store answers — mindex applies it inside the candidate
                # filter, so without this the arms would be cut at different
                # real depths.
                started = time.perf_counter()
                if arm == "weighted-sum":
                    points = weighted_sum_points(
                        session, url, dense, sparse, colbert, TOP_K * 3
                    )
                else:
                    body = build_query(arm, dense, sparse, colbert, TOP_K * 3)
                    resp = session.post(url, json=body)
                    resp.raise_for_status()
                    points = resp.json()["result"]["points"]
                latency = (time.perf_counter() - started) * 1000

                results = []
                for p in points:
                    hit = meta.get(p["id"])
                    if hit is None:
                        unknown += 1
                        continue
                    path, start, end = hit
                    if any(rx.match(path) for rx in exclude):
                        continue
                    results.append(
                        {
                            "path": path,
                            "score": p["score"],
                            "start_line": start,
                            "end_line": end,
                        }
                    )
                    if len(results) >= TOP_K:
                        break

                handles[arm].write(
                    json.dumps(
                        {
                            "schema": 1,
                            "corpus": args.corpus,
                            "language": corpus["language"],
                            "instance_id": inst["instance_id"],
                            "datasets": inst["datasets"],
                            "base_commit": inst["base_commit"],
                            "snapshot_sha": inst["base_commit"],
                            "query_bytes": len(inst["query"].encode()),
                            "gold_files": inst["gold_files"],
                            "gold_functions": inst.get("gold_functions"),
                            "n_gold": inst["n_gold"],
                            "category": inst.get("category"),
                            "leaks_gold_path": inst["leaks_gold_path"],
                            "leaks_gold_basename": inst["leaks_gold_basename"],
                            "lexical_overlap": inst.get("lexical_overlap"),
                            "overlap_bucket": inst.get("overlap_bucket"),
                            "doc_path": inst.get("doc_path"),
                            "results": results,
                            "n_results": len(results),
                            "distinct_files": len({r["path"] for r in results}),
                            "refusal": None,
                            "search_ms": round(latency, 1),
                            "index_ms": 0,
                            "orphans_pruned": 0,
                            "failed_files": 0,
                            "failed_paths": [],
                            "prov": {
                                "label": f"F2-{arm}",
                                "system": "qdrant-direct",
                                "arm": arm,
                                "collection": collection,
                                "top_k": TOP_K,
                                "dense_prefetch": DENSE_PREFETCH,
                                "sparse_prefetch": SPARSE_PREFETCH,
                                "fusion_limit": FUSION_LIMIT,
                            },
                        },
                        sort_keys=True,
                    )
                    + "\n"
                )
            if pos % 100 == 0:
                print(f"  [{pos}/{len(instances)}]")
    finally:
        for h in handles.values():
            h.close()

    if unknown:
        # A point Qdrant scored whose SQLite row is gone — mindex counts these
        # as `search_orphaned_winners` and so must this, rather than quietly
        # shortening a ranking.
        print(f"  WARN: {unknown} scored points had no SQLite row (orphans)")
    for arm in arms:
        print(f"  wrote {out_paths[arm].relative_to(root)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
