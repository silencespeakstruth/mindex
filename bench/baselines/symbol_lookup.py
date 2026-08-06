#!/usr/bin/env python3
"""Family F10's third criterion: is a lexical leg the answer, or is `symbols`?

If identifier queries turn out to want lexical matching, there are two things
that could be built and only one of them is a change to `/search`. mindex
already ships **exact-name definition lookup** — `POST /v0/{guid}/symbols`,
backed by `project_file_symbols` — and a caller who types `ensure_project` can
already be routed there by whatever is asking. Adding an FTS5 leg to `/search`
is, by contrast, a table plus an invalidation surface that has to follow
soft-deletes and GC. So a positive F10 result that `symbols` matches is not a
result about retrieval, it is an argument for routing, and PROTOCOL §5.7 makes
beating this arm one of the conditions for shipping a leg.

This is that arm. It resolves each query's identifiers against the symbol table
of the same indexed project every other arm is scored over, and ranks the files
that DEFINE them.

Ranking, because a baseline that only matches is not comparable to one that
ranks: files are ordered by how many of the query's identifiers they define,
then by how many definitions in total, then by path for determinism. It is
deliberately not mindex's own `symbols` ranking (anchor file > its directory >
the rest), which needs an anchor path the benchmark has no way to supply.

WHAT IT CANNOT DO, stated because a reader will otherwise take a low number for
a fact about `symbols`: nine of the supported languages ship no tags query at
all, so their files define nothing here. Both F10 corpora are Python, which
does have one, so the arm is measurable where it is used — but this is not a
general statement about the tool.
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from build_qrels import load_config, repo_root
from run import project_guid

from build_ident_qrels import extract_identifiers

# Matches run.py and bm25_fts5.py, and it must: a shorter ranking scores worse
# for reasons that have nothing to do with retrieval.
TOP_K = 100


def glob_to_regex(glob: str) -> str:
    """`docs/**` and friends — the same two forms `bm25_fts5.py` handles."""
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


def load_symbols(db: Path, guid: str, exclude: list[str]) -> dict[str, list[str]]:
    """name -> the files defining it, from mindex's own symbol table.

    Taken from the store rather than re-extracted, for the reason
    `bm25_fts5.py` states about chunks: an arm that rebuilt its own inputs
    would be measuring a different index from the one under test.
    """
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = conn.execute(
        "SELECT name, file_path FROM project_file_symbols WHERE project_guid = ?",
        (guid.replace("-", ""),),
    ).fetchall()
    conn.close()

    pats = [re.compile(glob_to_regex(g)) for g in exclude]
    index: dict[str, list[str]] = {}
    for name, path in rows:
        if any(p.match(path) for p in pats):
            continue
        index.setdefault(name, []).append(path)
    return index


def search(
    index: dict[str, list[str]], query: str
) -> tuple[list[dict[str, object]], float]:
    """Rank files by how much of the query they define.

    The query is re-tokenized with the corpus builder's own extractor, so this
    arm reads exactly the identifiers §3.4 put into the query — including on the
    `prose` arm, where it is the same question asked of an unprojected report.
    """
    start = time.perf_counter()
    idents = extract_identifiers(query)

    matched: dict[str, int] = {}
    defs: dict[str, int] = {}
    for ident in idents:
        for path in index.get(ident, []):
            defs[path] = defs.get(path, 0) + 1
        for path in set(index.get(ident, [])):
            matched[path] = matched.get(path, 0) + 1

    ranked = sorted(matched, key=lambda p: (-matched[p], -defs[p], p))[:TOP_K]
    latency = (time.perf_counter() - start) * 1000.0

    # `start_line`/`end_line` are absent on purpose: this arm answers with
    # files, ground truth is at file level, and inventing a span would put a
    # number in the record that nothing measured.
    results = [
        {"path": path, "score": float(matched[path]), "rank": pos}
        for pos, path in enumerate(ranked, start=1)
    ]
    return results, latency


def main() -> int:
    root = repo_root()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--config", type=Path, default=root / "bench" / "corpora.toml")
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--qrels-suffix", default="-ident")
    ap.add_argument("--label", default="symbols")
    ap.add_argument(
        "--mindex-label",
        default="baseline",
        help="the run.py label whose index this reads (its GUID names the symbols)",
    )
    ap.add_argument(
        "--db",
        type=Path,
        default=Path.home() / ".local/share/mindex-bench/mindex-bench.db",
    )
    args = ap.parse_args()

    config = load_config(args.config)
    corpus = next(c for c in config["corpus"] if c["name"] == args.corpus)

    qrels = (
        root
        / config["run"]["data_dir"]
        / "qrels"
        / f"{args.corpus}{args.qrels_suffix}.jsonl"
    )
    instances = [json.loads(line) for line in qrels.open()]

    guid = project_guid(args.mindex_label, args.corpus)
    exclude = corpus.get("search_exclude_paths", [])
    index = load_symbols(args.db, guid, exclude)
    if not index:
        raise SystemExit(
            f"no symbols for {args.corpus} (guid {guid}) in {args.db}. This arm "
            f"is scored over mindex's OWN symbol table, so the corpus must have "
            f"been indexed by run.py first — and the language must ship a tags "
            f"query, which nine of the supported ones do not."
        )
    print(f"  {sum(len(v) for v in index.values())} definitions, {len(index)} names")
    print(f"  excluded from the ranking: {exclude or 'nothing'}")

    out_path = (
        root
        / config["run"]["results_dir"]
        / f"{args.label}__{args.corpus}{args.qrels_suffix}.jsonl"
    )
    out_path.parent.mkdir(parents=True, exist_ok=True)

    empty = 0
    with out_path.open("w") as out:
        for pos, inst in enumerate(instances, start=1):
            results, latency = search(index, inst["query"])
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
                            "label": args.label,
                            "system": "symbol_lookup",
                            "top_k": TOP_K,
                            "symbol_source": str(args.db),
                            "names": len(index),
                            "exclude": exclude,
                        },
                    },
                    sort_keys=True,
                )
                + "\n"
            )
            if pos % 200 == 0:
                print(f"  [{pos}/{len(instances)}]")

    # Expected to be large on the mangled arm and non-trivial elsewhere: an
    # exact-name table answers nothing for a name it does not hold, which is
    # precisely the difference between this arm and a substring matcher.
    if empty:
        print(f"  {empty} queries matched no defined name at all")
    print(f"  wrote {out_path.relative_to(root)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
