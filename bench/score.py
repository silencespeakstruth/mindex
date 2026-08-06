#!/usr/bin/env python3
"""Turn the ranked lists run.py recorded into the metrics PROTOCOL.md §4 names.

Pure function of the result files: it opens no server, indexes nothing and can
be re-run over an archived JSONL years later. That separation is what makes a
re-analysis cheap and an accusation of moving the metric checkable.

TWO CONVENTIONS DO MOST OF THE WORK, and both are chosen rather than obvious.

RANKING IS OVER FILES, NOT CHUNKS. Ground truth names the files a fix touched;
mindex returns chunks. A chunk hit credits its file at the rank of its FIRST
occurrence, and later chunks of an already-credited file are dropped rather
than counted again. Counting them would let a system that returns ten chunks
of one file look like it found ten things, and it would make the metric depend
on chunk size — which is exactly one of the parameters under test.

AGGREGATION IS MACRO. Every corpus contributes one number to the headline
figure regardless of its instance count. django has sixty times ripgrep's
instances; pooling per query would make the headline a statement about django
with a rounding error attached, and would silently re-weight itself whenever
a corpus is added.

`--per-query` writes one row per instance, which is what stats.py consumes:
the paired tests in §5 need per-query deltas, not the aggregate.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any

# Reported in this order everywhere, so two runs are diffable line by line.
RECALL_AT = (1, 5, 10, 20)
NDCG_AT = 10
MRR_AT = 10
MAP_AT = 20
ACC_AT = (1, 5, 10, 20)


def ranked_files(results: list[dict[str, Any]]) -> list[str]:
    """Chunk ranking to file ranking, first occurrence wins."""
    seen: list[str] = []
    known = set()
    for hit in results:
        path = hit["path"]
        if path not in known:
            known.add(path)
            seen.append(path)
    return seen


def dcg(gains: list[float]) -> float:
    return sum(g / math.log2(i + 2) for i, g in enumerate(gains))


def ndcg_at(ranking: list[str], gold: set[str], k: int) -> float:
    """Binary-relevance nDCG.

    The ideal ranking is min(|gold|, k) relevant documents, so a query with
    more gold files than k can still score 1.0. The alternative — an IDCG over
    all |gold| — would cap such a query below 1 no matter what any system did,
    which measures the query rather than the system.
    """
    if not gold:
        return 0.0
    gains = [1.0 if p in gold else 0.0 for p in ranking[:k]]
    ideal = [1.0] * min(len(gold), k)
    denom = dcg(ideal)
    return dcg(gains) / denom if denom else 0.0


def recall_at(ranking: list[str], gold: set[str], k: int) -> float:
    if not gold:
        return 0.0
    return len(gold & set(ranking[:k])) / len(gold)


def reciprocal_rank(ranking: list[str], gold: set[str], k: int) -> float:
    for i, path in enumerate(ranking[:k], start=1):
        if path in gold:
            return 1.0 / i
    return 0.0


def average_precision(ranking: list[str], gold: set[str], k: int) -> float:
    """AP truncated at k, normalized by min(|gold|, k).

    Normalizing by |gold| instead would make AP unreachable whenever the gold
    set is larger than the cutoff — the same argument as the nDCG ideal above.
    """
    if not gold:
        return 0.0
    hits = 0
    total = 0.0
    for i, path in enumerate(ranking[:k], start=1):
        if path in gold:
            hits += 1
            total += hits / i
    denom = min(len(gold), k)
    return total / denom if denom else 0.0


def accuracy_at(ranking: list[str], gold: set[str], k: int) -> float:
    """LocAgent's Acc@k: all gold locations inside the top k, or nothing.

    Reported for comparability with published numbers and never gated on. It
    is all-or-nothing, so on multi-file instances it is mostly measuring |gold|
    and moves in steps too coarse to detect a regression.
    """
    if not gold:
        return 0.0
    return 1.0 if gold <= set(ranking[:k]) else 0.0


def score_instance(record: dict[str, Any]) -> dict[str, float]:
    ranking = ranked_files(record["results"])
    gold = set(record["gold_files"])
    scored: dict[str, float] = {
        f"ndcg@{NDCG_AT}": ndcg_at(ranking, gold, NDCG_AT),
        f"mrr@{MRR_AT}": reciprocal_rank(ranking, gold, MRR_AT),
        f"map@{MAP_AT}": average_precision(ranking, gold, MAP_AT),
    }
    for k in RECALL_AT:
        scored[f"recall@{k}"] = recall_at(ranking, gold, k)
    for k in ACC_AT:
        scored[f"acc@{k}"] = accuracy_at(ranking, gold, k)
    # The full curve; the consumer of this index is an agent reading the top
    # few results, so the shape of the head is the interesting part and a
    # handful of cutoffs can hide it.
    for k in range(1, 21):
        scored[f"curve_recall@{k}"] = recall_at(ranking, gold, k)
    return scored


PRIMARY = f"ndcg@{NDCG_AT}"

# Everything except the 20-point curve, which is reported separately.
HEADLINE = (
    [PRIMARY, f"mrr@{MRR_AT}", f"map@{MAP_AT}"]
    + [f"recall@{k}" for k in RECALL_AT]
    + [f"acc@{k}" for k in ACC_AT]
)


def mean(xs: list[float]) -> float:
    return statistics.fmean(xs) if xs else 0.0


def aggregate(rows: list[dict[str, Any]], keys: list[str]) -> dict[str, float]:
    return {k: mean([r[k] for r in rows]) for k in keys}


def stratify(rows: list[dict[str, Any]], field: str) -> dict[str, list[dict[str, Any]]]:
    """Group per-query rows by one reported dimension.

    Language, issue category, gold-set size and leakage each get their own
    breakdown because a regression confined to one of them is invisible in the
    pooled mean — which is how a pooled mean stays flat while the thing you
    changed broke.
    """
    groups: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        groups[str(row.get(field))].append(row)
    return dict(groups)


def gold_size_bucket(n: int) -> str:
    if n <= 1:
        return "1"
    if n == 2:
        return "2"
    if n <= 4:
        return "3-4"
    return "5+"


def leakage_stratum(row: dict[str, Any]) -> str:
    if row["leaks_gold_path"]:
        return "path"
    if row["leaks_gold_basename"]:
        return "basename"
    return "none"


def load_rows(paths: list[Path]) -> list[dict[str, Any]]:
    rows = []
    for path in paths:
        with path.open() as fh:
            for line in fh:
                if line.strip():
                    rows.append(json.loads(line))
    return rows


def score_all(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    scored = []
    for rec in records:
        row: dict[str, Any] = {
            "corpus": rec["corpus"],
            "language": rec["language"],
            "instance_id": rec["instance_id"],
            "datasets": rec["datasets"],
            "category": rec["category"],
            "n_gold": rec["n_gold"],
            "gold_size": gold_size_bucket(rec["n_gold"]),
            "leakage": leakage_stratum(rec),
            # The axis the descriptive tier exists to separate: a query whose
            # words are already the file's identifiers is one a lexical matcher
            # wins by default, and pooling those with the rest averages away
            # the only effect dense and ColBERT retrieval are there to produce.
            "overlap_bucket": rec.get("overlap_bucket"),
            "lexical_overlap": rec.get("lexical_overlap"),
            # PROTOCOL §3.4 / §9.6. `projection` names the arm a query belongs
            # to; the other two are the strata that decide whether a lexical
            # gain is retrieval or string matching. All three are absent on
            # every other tier and simply stratify to None there.
            "projection": rec.get("projection"),
            "ident_in_gold": rec.get("ident_in_gold"),
            "ident_df_min": rec.get("ident_df_min"),
            "n_results": rec["n_results"],
            "ranking_depth": len(ranked_files(rec["results"])),
            # A query the server could not serve scores zero like any other
            # miss, and is counted separately so the zero cannot be read as
            # "ranked badly" when it means "answered with an error".
            "refusal": rec.get("refusal"),
            "search_ms": rec["search_ms"],
            "label": rec["prov"]["label"],
        }
        row.update(score_instance(rec))
        scored.append(row)
    return scored


def report(rows: list[dict[str, Any]]) -> dict[str, Any]:
    by_corpus = stratify(rows, "corpus")
    per_corpus = {c: aggregate(rs, HEADLINE) for c, rs in sorted(by_corpus.items())}

    # Macro: each corpus once. Never the mean over all queries.
    macro = {k: mean([per_corpus[c][k] for c in per_corpus]) for k in HEADLINE}
    macro_curve = {
        f"curve_recall@{k}": mean(
            [mean([r[f"curve_recall@{k}"] for r in by_corpus[c]]) for c in by_corpus]
        )
        for k in range(1, 21)
    }

    out: dict[str, Any] = {
        "n_queries": len(rows),
        "n_corpora": len(by_corpus),
        "macro": macro,
        "macro_recall_curve": macro_curve,
        "per_corpus": per_corpus,
        "per_corpus_n": {c: len(rs) for c, rs in sorted(by_corpus.items())},
        "strata": {},
        "refusals": {},
    }
    for row in rows:
        if row.get("refusal"):
            code = str(row["refusal"])
            out["refusals"][code] = out["refusals"].get(code, 0) + 1
    for field in (
        "language",
        "category",
        "gold_size",
        "leakage",
        "overlap_bucket",
        "projection",
        "ident_in_gold",
    ):
        # A field no tier in this run carries would otherwise report one
        # stratum called "None" holding every query — a breakdown that looks
        # like a result and says nothing. The identifier fields are absent from
        # both older tiers by construction.
        if all(row.get(field) is None for row in rows):
            continue
        groups = stratify(rows, field)
        out["strata"][field] = {
            key: {"n": len(rs), **aggregate(rs, [PRIMARY, "recall@10"])}
            for key, rs in sorted(groups.items())
        }

    # How deep the scored ranking actually goes. A recall@k computed over a
    # ranking with fewer than k entries is not wrong, but it is not recall@k
    # either — it is recall over whatever came back, and the two are
    # indistinguishable in the output. Measured at top_k=20 on ripgrep: 20
    # chunks deduplicated to a mean of 8.9 files, so every recall@20 was really
    # recall@9 and the flat tail that produced read as a finding about
    # retrieval.
    depths = [int(r.get("ranking_depth", 0)) for r in rows]
    out["ranking_depth"] = {
        "mean": round(mean([float(d) for d in depths]), 2),
        "min": min(depths) if depths else 0,
        "shorter_than": {
            str(k): sum(1 for d in depths if d < k)
            for k in sorted({*RECALL_AT, NDCG_AT})
        },
    }
    return out


def print_report(summary: dict[str, Any]) -> None:
    print(f"\nqueries: {summary['n_queries']}  corpora: {summary['n_corpora']}")
    if summary.get("refusals"):
        total = sum(summary["refusals"].values())
        share = 100.0 * total / summary["n_queries"]
        print(
            f"  {total} ({share:.1f}%) were REFUSED by the server and scored "
            f"zero: {summary['refusals']}"
        )
        print("  every metric below is depressed by exactly that many zeros.")

    depth = summary.get("ranking_depth")
    if depth:
        print(f"  scored ranking depth: mean {depth['mean']} files, min {depth['min']}")
        short = {k: v for k, v in depth["shorter_than"].items() if v}
        if short:
            # Not a warning about the system: a warning about what the numbers
            # below are allowed to be called.
            print(
                f"  queries whose ranking is SHORTER than the cutoff: {short} "
                f"of {summary['n_queries']} — at those cutoffs the figure is "
                f"recall over what came back, not recall@k."
            )
    print("\nmacro (each corpus weighted once):")
    for key in HEADLINE:
        print(f"  {key:<12} {summary['macro'][key]:.4f}")

    print("\nper corpus:")
    width = max(len(c) for c in summary["per_corpus"]) if summary["per_corpus"] else 8
    header = f"  {'corpus':<{width}}  {'n':>5}  " + "  ".join(
        f"{k:>10}" for k in (PRIMARY, "recall@10", "mrr@10")
    )
    print(header)
    for corpus, vals in summary["per_corpus"].items():
        n = summary["per_corpus_n"][corpus]
        row = "  ".join(f"{vals[k]:>10.4f}" for k in (PRIMARY, "recall@10", "mrr@10"))
        print(f"  {corpus:<{width}}  {n:>5}  {row}")

    print("\nrecall@k curve (macro):")
    curve = summary["macro_recall_curve"]
    for k in (1, 2, 3, 5, 10, 15, 20):
        bar = "#" * round(curve[f"curve_recall@{k}"] * 40)
        print(f"  k={k:<3} {curve[f'curve_recall@{k}']:.4f}  {bar}")

    for field, groups in summary["strata"].items():
        print(f"\nby {field}:")
        for key, vals in groups.items():
            print(
                f"  {key:<16} n={vals['n']:<5} "
                f"{PRIMARY}={vals[PRIMARY]:.4f}  recall@10={vals['recall@10']:.4f}"
            )


# ---------------------------------------------------------------------------
# Self-test. The scorer is the one component whose bugs are invisible in its
# output: a wrong nDCG is still a plausible number. So every formula is checked
# against a hand-computed value, and the two ends of the range are checked
# against systems whose scores are known analytically.
# ---------------------------------------------------------------------------


def self_test() -> int:
    failures = []

    def check(name: str, got: float, want: float, tol: float = 1e-9) -> None:
        ok = abs(got - want) <= tol
        print(f"  {'ok  ' if ok else 'FAIL'} {name}: got {got:.6f} want {want:.6f}")
        if not ok:
            failures.append(name)

    # Hand-computed. gold = {a}; ranking puts it third.
    # DCG = 1/log2(4) = 0.5; IDCG = 1/log2(2) = 1 -> nDCG = 0.5
    ranking = ["x", "y", "a", "z"]
    check("ndcg@10 single gold at rank 3", ndcg_at(ranking, {"a"}, 10), 0.5)
    check("mrr@10 same", reciprocal_rank(ranking, {"a"}, 10), 1 / 3)
    check("recall@1 same", recall_at(ranking, {"a"}, 1), 0.0)
    check("recall@5 same", recall_at(ranking, {"a"}, 5), 1.0)
    check("map@20 same", average_precision(ranking, {"a"}, 20), 1 / 3)
    check("acc@1 same", accuracy_at(ranking, {"a"}, 1), 0.0)
    check("acc@5 same", accuracy_at(ranking, {"a"}, 5), 1.0)

    # Two gold files at ranks 1 and 3.
    # DCG = 1/log2(2) + 1/log2(4) = 1.5; IDCG = 1 + 1/log2(3) = 1.63093
    r2 = ["a", "x", "b"]
    check("ndcg@10 two gold", ndcg_at(r2, {"a", "b"}, 10), 1.5 / (1 + 1 / math.log2(3)))
    check("map@20 two gold", average_precision(r2, {"a", "b"}, 20), (1 + 2 / 3) / 2)
    check("acc@3 two gold", accuracy_at(r2, {"a", "b"}, 3), 1.0)
    check("acc@2 two gold", accuracy_at(r2, {"a", "b"}, 2), 0.0)

    # A perfect ranker must score exactly 1 on every metric, including when the
    # gold set is larger than the cutoff.
    gold5 = {f"g{i}" for i in range(5)}
    perfect = sorted(gold5) + [f"n{i}" for i in range(20)]
    for k in (1, 5, 10):
        check(f"perfect ndcg@{k}", ndcg_at(perfect, gold5, k), 1.0)
    check("perfect map@20", average_precision(perfect, gold5, MAP_AT), 1.0)
    check("perfect mrr@10", reciprocal_rank(perfect, gold5, MRR_AT), 1.0)
    check("perfect acc@5", accuracy_at(perfect, gold5, 5), 1.0)

    # A gold set of 12 with a cutoff of 10: still reachable, by construction.
    gold12 = {f"g{i:02d}" for i in range(12)}
    check("ndcg@10 gold>k", ndcg_at(sorted(gold12), gold12, 10), 1.0)

    # An empty ranking is the 404 case and must be zero, not an error.
    check("empty ranking ndcg", ndcg_at([], {"a"}, 10), 0.0)
    check("empty ranking recall", recall_at([], {"a"}, 10), 0.0)

    # Chunk-to-file collapse: five chunks of one file are one file at rank 1.
    hits = [{"path": "a"} for _ in range(5)] + [{"path": "b"}]
    got = ranked_files(hits)
    ok = got == ["a", "b"]
    print(f"  {'ok  ' if ok else 'FAIL'} ranked_files dedupes: {got}")
    if not ok:
        failures.append("ranked_files")

    # A random ranker over N documents with one gold document has expected
    # reciprocal rank H(N)/N. Checked by exhaustive enumeration rather than
    # simulation, so the test cannot flake.
    n = 8
    expected_rr = sum(1 / i for i in range(1, n + 1)) / n
    observed = mean(
        [
            reciprocal_rank([f"d{j}" for j in range(n)], {f"d{pos}"}, n)
            for pos in range(n)
        ]
    )
    check("random ranker mrr = H(n)/n", observed, expected_rr)

    print("\nself-test:", "FAILED" if failures else "PASS")
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", nargs="*", type=Path, help="run.py JSONL files")
    parser.add_argument("--per-query", type=Path, help="write per-query scores here")
    parser.add_argument("--json", type=Path, help="write the summary here")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.results:
        parser.error("give at least one results file, or --self-test")

    records = load_rows(args.results)
    if not records:
        raise SystemExit("no records")
    rows = score_all(records)

    labels = {r["label"] for r in rows}
    if len(labels) > 1:
        # Scoring two configurations into one aggregate produces a number that
        # describes neither. stats.py is what compares them.
        raise SystemExit(
            f"results carry {len(labels)} labels ({sorted(labels)}); "
            "score one configuration at a time"
        )

    summary = report(rows)
    summary["label"] = next(iter(labels))
    print_report(summary)

    if args.per_query:
        args.per_query.parent.mkdir(parents=True, exist_ok=True)
        with args.per_query.open("w") as fh:
            for row in rows:
                fh.write(json.dumps(row, sort_keys=True) + "\n")
        print(f"\nper-query: {args.per_query}")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
        print(f"summary:   {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
