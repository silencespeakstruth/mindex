#!/usr/bin/env python3
"""Family F4: a cross-encoder reranker, measured over rankings already on disk.

THE TRADE THIS TESTS, in one line. ColBERT buys a published +1.1 nDCG@10 for
270x the storage and 84% of query latency. A cross-encoder stores **nothing** —
it scores (query, chunk) pairs at query time — and the published gains are
+5 to +15 nDCG@10 on MTEB/BEIR. If that transfers even weakly, the ColBERT
question stops being "keep or drop" and becomes moot.

WHY THIS NEEDS NO REINDEX AND NO CORPUS PASS. Reranking is a pure function of
(query, candidate texts). Every result file already holds a ranked list of up
to 100 chunks with `path`/`start_line`/`end_line`, and the chunk text is in
SQLite. So this reads a completed run, rescores its own candidates, and writes
a new result file in the same schema. Total GPU cost is one forward pass per
(query, candidate) pair — no embedding of the corpus at all.

THE CEILING IS THE INPUT'S RECALL, and that is the point of reporting it.
A reranker can only reorder what the first stage returned: if the gold file is
not in the 100 candidates, no reranker recovers it. So `recall@100` of the
input run is the hard ceiling on `recall@10` here, and it is printed. A gain
that merely approaches that ceiling says the first stage was already good
enough and the ordering was the problem — which is exactly the hypothesis
`F2-weighted-sum` also tests, from the other side.

DEPTH IS A KNOB WITH A REAL COST. Cross-encoder latency is LINEAR in candidate
count — unlike ColBERT, whose document side is precomputed — so reranking 200
costs four times reranking 50. `--depth` exists to price that rather than
assume it, and the measured per-query latency is recorded per row.
"""

from __future__ import annotations

import argparse
import json
import sqlite3
import statistics
import sys
import time
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from build_qrels import repo_root

DEFAULT_MODEL = "BAAI/bge-reranker-v2-m3"
# A chunk can be 512 tokens and a query 200 characters; the pair is truncated by
# the tokenizer. Kept explicit because a silently different truncation is a
# different experiment.
MAX_PAIR_TOKENS = 1024


def chunk_texts(db: Path, guid: str) -> dict[tuple[str, int, int], str]:
    """(path, start, end) -> code, for the project the run indexed."""
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = conn.execute(
        "SELECT file_path, start_line, end_line, code FROM project_file_chunks "
        "WHERE project_guid = ? AND status = 'active'",
        (guid.replace("-", ""),),
    ).fetchall()
    conn.close()
    return {(r[0], r[1], r[2]): r[3] for r in rows}


def main() -> int:
    root = repo_root()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("input", type=Path, help="a completed run's JSONL")
    ap.add_argument("--label", default=None, help="output label; default ce-<depth>")
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument(
        "--depth",
        type=int,
        default=50,
        help="how many of the input's candidates to rescore (latency is linear in this)",
    )
    ap.add_argument("--batch-size", type=int, default=64)
    ap.add_argument(
        "--device",
        choices=["auto", "cuda", "xpu", "cpu"],
        default="auto",
        help="`auto` refuses to fall back to CPU. The first run of this script "
        "did fall back — `torch.cuda.is_available()` is False in the XPU venv — "
        "and reported 6.6 s per query at depth 50. That number described the "
        "harness, not the model, and it is the reason this flag exists.",
    )
    ap.add_argument(
        "--dtype",
        choices=["float32", "float16", "bfloat16"],
        default="float16",
        help="asserted after loading, not merely requested",
    )
    ap.add_argument("--mindex-label", default="baseline")
    ap.add_argument(
        "--db",
        type=Path,
        default=Path.home() / ".local/share/mindex-bench/mindex-bench.db",
    )
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument(
        "--qrels",
        type=Path,
        default=None,
        help="the query set; results carry only `query_bytes`, so the text has "
        "to come from here. Defaults to the qrels the input's name implies.",
    )
    args = ap.parse_args()

    from run import project_guid

    records = [json.loads(line) for line in args.input.open()]
    if args.limit:
        records = records[: args.limit]
    corpus = records[0]["corpus"]

    # A result row records how many BYTES the query was, not what it said —
    # deliberately, since the query set is the artifact and results should not
    # duplicate it. So the text is joined back by instance id, and a mismatch
    # is fatal rather than silently reranking against an empty string.
    stem_q = args.input.name.split("__", 1)[1].removesuffix(".jsonl")
    qrels_path = args.qrels or (root / "bench" / ".data" / "qrels" / f"{stem_q}.jsonl")
    queries = {
        json.loads(line)["instance_id"]: json.loads(line)["query"]
        for line in qrels_path.open()
    }
    missing_q = [r["instance_id"] for r in records if r["instance_id"] not in queries]
    if missing_q:
        raise SystemExit(
            f"{len(missing_q)} result rows have no query in {qrels_path} "
            f"(first: {missing_q[0]}). Reranking against a missing query would "
            f"score every pair on an empty string and look like a clean null."
        )
    guid = project_guid(args.mindex_label, corpus)
    texts = chunk_texts(args.db, guid)
    if not texts:
        raise SystemExit(
            f"no active chunks for {corpus} (guid {guid}) in {args.db}. The "
            f"reranker rescores the run's OWN candidates, so the index that "
            f"produced them has to still be there."
        )

    # Imported late and loudly: this pulls torch and puts a model on the GPU,
    # which is not what a reader of `--help` should pay for.
    import torch
    from external_embedder import resolve_device
    from sentence_transformers import CrossEncoder

    device = resolve_device(args.device, torch)
    print(f"  loading {args.model} on {device} ...", flush=True)
    model = CrossEncoder(args.model, max_length=MAX_PAIR_TOKENS, device=device)
    want = getattr(torch, args.dtype)
    if want is not torch.float32:
        model.model = model.model.to(want)
    got = next(model.model.parameters()).dtype
    landed = str(next(model.model.parameters()).device).split(":")[0]
    if got is not want or landed != device:
        raise SystemExit(
            f"asked for {args.dtype} on {device}; the model is {got} on "
            f"{landed}. Every latency in this run would describe the wrong "
            f"hardware or the wrong precision."
        )
    print(f"  device={landed} dtype={args.dtype} depth={args.depth}")

    label = args.label or f"ce{args.depth}"
    stem = args.input.name.split("__", 1)[1]
    out_path = root / "bench" / "results" / f"{label}__{stem}"

    ceiling_hits = ceiling_total = 0
    missing_text = 0
    latencies: list[float] = []
    with out_path.open("w") as out:
        for pos, rec in enumerate(records, start=1):
            cands = rec["results"][: args.depth]
            pairs, keep = [], []
            for c in cands:
                key = (c["path"], c["start_line"], c["end_line"])
                code = texts.get(key)
                if code is None:
                    # The chunk moved between the run and now. Kept in the
                    # ranking at its original position rather than dropped —
                    # silently shortening a ranking is what `distinct_files`
                    # and `search_orphaned_winners` exist to prevent.
                    missing_text += 1
                    continue
                pairs.append((queries[rec["instance_id"]], code))
                keep.append(c)
            if not pairs:
                reranked = cands
                latency = 0.0
            else:
                started = time.perf_counter()
                scores = model.predict(
                    pairs, batch_size=args.batch_size, show_progress_bar=False
                )
                latency = (time.perf_counter() - started) * 1000
                order = sorted(range(len(keep)), key=lambda i: -float(scores[i]))
                reranked = [{**keep[i], "score": float(scores[i])} for i in order]
            # Everything below the reranked depth keeps its original order and
            # sits after: the reranker was not asked about it, so it must not
            # be reordered on the strength of a score it never received.
            tail = rec["results"][args.depth :]
            results = reranked + tail

            gold = set(rec["gold_files"])
            if gold:
                ceiling_total += 1
                if gold & {c["path"] for c in rec["results"]}:
                    ceiling_hits += 1

            new: dict[str, Any] = dict(rec)
            new["results"] = results
            new["n_results"] = len(results)
            new["distinct_files"] = len({r["path"] for r in results})
            new["search_ms"] = round(rec.get("search_ms", 0.0) + latency, 1)
            new["prov"] = {
                **rec.get("prov", {}),
                "label": label,
                "reranker": args.model,
                "rerank_depth": args.depth,
                "rerank_ms": round(latency, 1),
                "rerank_device": device,
                "rerank_dtype": args.dtype,
                "rerank_batch_size": args.batch_size,
                "reranked_over": args.input.name,
            }
            latencies.append(latency)
            out.write(json.dumps(new, sort_keys=True) + "\n")
            if pos % 100 == 0:
                print(f"  [{pos}/{len(records)}] {latency:.0f}ms", flush=True)

    if missing_text:
        print(f"  WARN: {missing_text} candidates had no chunk row and were dropped")
    if ceiling_total:
        print(
            f"  ceiling: the gold file was among the input's candidates for "
            f"{ceiling_hits}/{ceiling_total} queries ({100 * ceiling_hits / ceiling_total:.1f}%) "
            f"— no reranker can exceed this at any k"
        )

    # THE NUMBER F8 IS DECLARED ON is the delta over the first stage, not the
    # reranked score. CoREB reports rerankers this way for a reason that is
    # visible in its own table: every off-the-shelf reranker it tested has a
    # NEGATIVE delta on at least one code task, and jina-reranker-v3 is negative
    # on all three despite a CoIR of 70.64. An absolute score hides that; a
    # delta cannot. Printed here rather than left to `stats.py` so a run that
    # made things worse says so before anyone opens the result file.
    import score as scoring

    before = [scoring.score_instance(r)[scoring.PRIMARY] for r in records]
    reranked_rows = [json.loads(line) for line in out_path.open()]
    after = [scoring.score_instance(r)[scoring.PRIMARY] for r in reranked_rows]
    delta = statistics.fmean(a - b for a, b in zip(after, before, strict=True))
    print(
        f"  {scoring.PRIMARY}: first stage {statistics.fmean(before):.4f} -> "
        f"reranked {statistics.fmean(after):.4f}  "
        f"delta {delta:+.4f}  (n={len(before)}, exploratory until stats.py)"
    )
    if latencies:
        ordered = sorted(latencies)
        print(
            f"  rerank latency on {device}/{args.dtype} at depth {args.depth}: "
            f"median {ordered[len(ordered) // 2]:.0f} ms, "
            f"p90 {ordered[int(0.9 * (len(ordered) - 1))]:.0f} ms"
        )
    print(f"  wrote {out_path.relative_to(root)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
