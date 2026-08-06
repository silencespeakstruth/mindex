#!/usr/bin/env python3
"""Family F1: the lexical floor, over the identical chunk set.

Without this, a retrieval number is unfalsifiable. nDCG@10 = 0.43 sounds low
against a 0-1 scale, but the scale's floor is not 0.5 — on django it is roughly
10/2701 = 0.4% for a random ranker. The question that can actually be answered
is whether the dense + sparse + ColBERT pipeline beats the oldest, cheapest
thing that works, and **where** it does: PROTOCOL §3.0.1 exists because the two
answers differ by lexical-overlap bucket, and pooling them hides the only
effect worth measuring.

FAIRNESS IS THE WHOLE POINT, so every input is taken from mindex's own store
rather than rebuilt:

  * the same chunks — `project_file_chunks` at `status='active'`, the exact
    rows the candidate-set filter would have produced;
  * the same exclusion — `search_exclude_paths` applied identically, so the
    docs tree the query was lifted from is out of both rankings;
  * the same depth — `TOP_K` chunks, deduplicated to files by first
    occurrence, which is what `score.py` scores for either system;
  * the same output schema, so one scorer reads both and no metric is
    reimplemented for the baseline.

Ranking is SQLite's own FTS5 `bm25()`, not a reimplementation: it is the
reference implementation of the thing being claimed as a floor, and mindex
already depends on SQLite. `--tokenizer` picks between `unicode61` (words) and
`trigram` (substring matching, which finds `get_or_set` inside a longer
identifier); both are reported because they answer different questions and
neither is obviously the honest floor on code.
"""

from __future__ import annotations

import argparse
import json
import random
import re
import sqlite3
import sys
import time
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from build_qrels import load_config, repo_root
from run import project_guid

# Matches run.py, and it must: a shorter ranking scores worse for reasons that
# have nothing to do with retrieval quality.
TOP_K = 100

# FTS5 reads bare words as query syntax, so a prose query is not a query — a
# `NEAR` or a `-` in the text is a syntax error or, worse, a silent operator.
# Terms are extracted and quoted, then OR-ed.
TERM = re.compile(r"[A-Za-z_][A-Za-z0-9_]{1,}")

# Terms this common carry no discrimination and cost the whole scan. Kept tiny
# and English-only on purpose: a longer list is a tuning knob, and a baseline
# that has been tuned is not a floor.
STOP = {
    "the",
    "and",
    "for",
    "are",
    "but",
    "not",
    "you",
    "all",
    "can",
    "her",
    "was",
    "one",
    "our",
    "out",
    "day",
    "get",
    "has",
    "him",
    "his",
    "how",
    "its",
    "may",
    "new",
    "now",
    "old",
    "see",
    "two",
    "way",
    "who",
    "boy",
    "did",
    "use",
    "with",
    "this",
    "that",
    "from",
    "they",
    "will",
    "would",
    "there",
    "their",
    "what",
    "when",
    "which",
    "into",
    "than",
    "them",
    "then",
    "these",
    "those",
    "some",
    "such",
    "only",
    "also",
    "been",
    "have",
    "more",
    "most",
    "other",
    "over",
}


def load_chunks(
    db: Path, guid: str, exclude: list[str]
) -> list[tuple[int, str, str, int, int]]:
    """The candidate set, taken from mindex's store rather than rebuilt."""
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = conn.execute(
        "SELECT rowid, file_path, code, start_line, end_line "
        "FROM project_file_chunks WHERE project_guid = ? AND status = 'active'",
        (guid.replace("-", ""),),
    ).fetchall()
    conn.close()
    if exclude:
        pats = [re.compile(glob_to_regex(g)) for g in exclude]
        rows = [r for r in rows if not any(p.match(r[1]) for p in pats)]
    return rows


def glob_to_regex(glob: str) -> str:
    """`docs/**` and friends. Deliberately minimal — the config uses two forms."""
    out = []
    i = 0
    while i < len(glob):
        if glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif glob[i] == "*":
            out.append("[^/]*")
            i += 1
        else:
            out.append(re.escape(glob[i]))
            i += 1
    return "".join(out) + "$"


def build_fts(
    rows: list[tuple[int, str, str, int, int]], tokenizer: str
) -> sqlite3.Connection:
    conn = sqlite3.connect(":memory:")
    if tokenizer == "trigram":
        tok = "trigram"
    else:
        # `remove_diacritics 2` and the default separators; no stemming, because
        # a stemmer is a choice and this is meant to be the unadorned floor.
        tok = "unicode61"
    conn.execute(f"CREATE VIRTUAL TABLE chunks USING fts5(code, tokenize='{tok}')")
    conn.executemany(
        "INSERT INTO chunks(rowid, code) VALUES (?, ?)",
        [(r[0], r[2]) for r in rows],
    )
    return conn


def fts_query(text: str) -> str:
    seen: dict[str, None] = {}
    for m in TERM.finditer(text):
        w = m.group(0)
        if len(w) > 2 and w.lower() not in STOP:
            seen.setdefault(w.lower(), None)
    if not seen:
        return ""
    return " OR ".join(f'"{w}"' for w in seen)


def random_search(
    rowids: list[int], meta: dict[int, tuple[str, int, int]], rng: random.Random
) -> tuple[list[dict[str, Any]], float]:
    """The floor under the floor: `TOP_K` chunks drawn uniformly, no query read.

    This is not a competitor, it is the instrument's calibration. Every metric
    reported here has an analytic expectation for a uniform ranking, and the
    whole chain — qrels, the result schema, chunk-to-file deduplication, the
    scorer — is only trustworthy if the measured value lands on it. It is the
    one baseline whose right answer is known in advance, which is exactly why
    it is worth running through the same path as the others rather than
    computed in closed form.
    """
    started = time.perf_counter()
    picked = rng.sample(rowids, min(TOP_K, len(rowids)))
    latency = (time.perf_counter() - started) * 1000
    out = []
    for i, rowid in enumerate(picked):
        path, start, end = meta[rowid]
        out.append(
            {
                "path": path,
                "score": float(TOP_K - i),
                "start_line": start,
                "end_line": end,
            }
        )
    return out, latency


def search(
    conn: sqlite3.Connection, meta: dict[int, tuple[str, int, int]], query: str
) -> tuple[list[dict[str, Any]], float]:
    expr = fts_query(query)
    if not expr:
        return [], 0.0
    started = time.perf_counter()
    try:
        rows = conn.execute(
            "SELECT rowid, bm25(chunks) FROM chunks WHERE chunks MATCH ? "
            "ORDER BY bm25(chunks) LIMIT ?",
            (expr, TOP_K),
        ).fetchall()
    except sqlite3.OperationalError:
        # A query whose every term is punctuation-adjacent. Recorded as an
        # empty ranking rather than dropped, exactly as a mindex refusal is.
        return [], (time.perf_counter() - started) * 1000
    latency = (time.perf_counter() - started) * 1000
    out = []
    for rowid, score in rows:
        path, start, end = meta[rowid]
        # bm25() is negative-better in SQLite; flip it so `score.py` and any
        # reader see the same "higher is better" convention as mindex.
        out.append(
            {"path": path, "score": -score, "start_line": start, "end_line": end}
        )
    return out, latency


def main() -> int:
    root = repo_root()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--config", type=Path, default=root / "bench" / "corpora.toml")
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--qrels-suffix", default="-docs")
    ap.add_argument(
        "--tokenizer", choices=["unicode61", "trigram"], default="unicode61"
    )
    ap.add_argument(
        "--system",
        choices=["bm25", "random"],
        default="bm25",
        help="`random` calibrates the harness against a known analytic answer",
    )
    ap.add_argument("--seed", type=int, default=20260805)
    ap.add_argument("--label", default=None, help="run label; defaults to bm25-<tok>")
    ap.add_argument(
        "--mindex-label",
        default="baseline",
        help="the run.py label whose index this reads (its GUID names the chunks)",
    )
    ap.add_argument(
        "--db",
        type=Path,
        default=Path.home() / ".local/share/mindex-bench/mindex-bench.db",
    )
    args = ap.parse_args()

    config = load_config(args.config)
    corpus = next(c for c in config["corpus"] if c["name"] == args.corpus)
    label = args.label or (
        "random" if args.system == "random" else f"bm25-{args.tokenizer}"
    )

    qrels = (
        root
        / config["run"]["data_dir"]
        / "qrels"
        / f"{args.corpus}{args.qrels_suffix}.jsonl"
    )
    instances = [json.loads(line) for line in qrels.open()]

    # The same GUID run.py derives — imported rather than re-derived, so the
    # chunk set is the one mindex actually searched and cannot drift from it.
    guid = project_guid(args.mindex_label, args.corpus)

    exclude = corpus.get("search_exclude_paths", [])
    rows = load_chunks(args.db, guid, exclude)
    if not rows:
        raise SystemExit(
            f"no active chunks for {args.corpus} (guid {guid}) in {args.db}. "
            f"The lexical floor is scored over mindex's OWN chunk set, so the "
            f"corpus must have been indexed by run.py first."
        )
    meta = {r[0]: (r[1], r[3], r[4]) for r in rows}
    print(f"  {len(rows)} active chunks, {len({r[1] for r in rows})} files")
    print(f"  excluded from the ranking: {exclude or 'nothing'}")

    rng = random.Random(args.seed)
    rowids = [r[0] for r in rows]
    conn = build_fts(rows, args.tokenizer) if args.system == "bm25" else None
    out_path = (
        root
        / config["run"]["results_dir"]
        / f"{label}__{args.corpus}{args.qrels_suffix}.jsonl"
    )
    out_path.parent.mkdir(parents=True, exist_ok=True)

    empty = 0
    with out_path.open("w") as out:
        for pos, inst in enumerate(instances, start=1):
            if args.system == "random":
                results, latency = random_search(rowids, meta, rng)
            else:
                assert conn is not None
                results, latency = search(conn, meta, inst["query"])
            if not results:
                empty += 1
            out.write(
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
                        # PROTOCOL §3.4/§9.6 — absent on every other tier, and
                        # carried here because `score.py` cuts F10's strata from
                        # the result record, not from the qrels file.
                        "projection": inst.get("projection"),
                        "ident_in_gold": inst.get("ident_in_gold"),
                        "ident_df_min": inst.get("ident_df_min"),
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
                            "label": label,
                            "system": args.system,
                            "tokenizer": args.tokenizer,
                            "top_k": TOP_K,
                            "chunk_source": str(args.db),
                            "chunks": len(rows),
                            "exclude": exclude,
                        },
                    },
                    sort_keys=True,
                )
                + "\n"
            )
            if pos % 200 == 0:
                print(f"  [{pos}/{len(instances)}]")

    if empty:
        print(f"  {empty} queries produced no ranking at all (no usable term)")
    print(f"  wrote {out_path.relative_to(root)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
