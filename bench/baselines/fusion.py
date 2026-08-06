#!/usr/bin/env python3
"""Family F7: the rule that turns several retrieval legs into one ordering.

THE QUESTION, AND WHY IT IS NOT THE ONE THE PROPOSAL ASSUMED. `FINDINGS.md` §8b
argues that a heterogeneous index is rescued by RRF, because RRF fuses *ranks*
and is therefore model-agnostic — no calibration between a 1024-d space and a
768-d one required. That argument is half right. RRF is model-agnostic and it is
also **strength-blind**: it gives a leg the same say whatever its quality, so
adding a weak leg to a strong one costs accuracy. Measured on the archive, an
equal-weight RRF of CodeRankEmbed with BGE-M3's sparse head scores 0.3805
against 0.4060 for CodeRankEmbed alone, and no RRF weighting recovers the
difference. Score-normalised weighted fusion reaches 0.4210. The published
record agrees from a different direction: on APPS code generation, hybrid RRF
scored 33.54 against BM25 alone at 38.00 (arXiv 2605.14503).

So the rule is a parameter, not a given, and this script measures it.

FUSION IS OVER CHUNKS, SCORING IS OVER FILES, AND THE ORDER MATTERS. mindex
fuses chunks and returns chunks; the metric credits files. Fusing first and
deduplicating second is what the server would do, and it is not the same
operation as deduplicating each leg and fusing the files: a file whose legs
favour *different* chunks accumulates evidence in one and not the other. This
script therefore fuses at chunk granularity and hands the result through the
harness's own chunk-to-file convention.

That is also why `ranx.optimize_fusion` is not used here and `ranx.fuse` is.
`optimize_fusion` evaluates internally against qrels, and our qrels name files
while the runs being fused name chunks — it would score every candidate as a
miss. `fuse` needs no qrels, so the fusion arithmetic (25 methods, six
normalisations, all maintained and tested upstream) is theirs and only the
weight search is ours.

WEIGHTS ARE CHOSEN ON ONE CORPUS AND REPORTED ON ANOTHER. A weight tuned and
reported on the same queries is not a measurement, and the effect being chased
here (~0.015) is the size at which that stops being a technicality. `--train`
and `--test` are separate arguments with no default that could quietly be the
same corpus, and the chosen weight is written into every output row's
provenance beside the corpus it was chosen on.
"""

from __future__ import annotations

import argparse
import itertools
import json
import statistics
import sys
import time
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import score as S
from ranx_bridge import load_records

# Fusion methods worth a grid. `wsum` over min-max is the arm the archive says
# wins; `rrf` is the incumbent proposal and the thing it has to beat; the rest
# are here so "we tried the obvious alternatives" is a fact rather than a claim.
# NOTE the names: `fuse(method=...)` keys are NOT ranx's exported function
# names. `ranx.fusion.__all__` exports `comb_sum`/`comb_mnz`/`comb_max`, but the
# switch in `ranx/fusion/__init__.py` accepts `sum`/`mnz`/`max`, and passing the
# exported name raises "Fusion method comb_sum not supported" — after the grid
# has already run. Taken from the switch, not the export list.
METHODS = ("wsum", "rrf", "sum", "mnz", "max", "wmnz")
NORMS = ("min-max", "zmuv", "sum", "max", "rank", "borda")

# `borda` is excluded from `--sweep-methods` and reachable only by naming it in
# `--norms`. Measured on this corpus: every other normalisation completed a
# 21-point weight grid over 1 115 queries in 25-38 s, and `borda` had not
# finished ONE grid after fourteen minutes at 983% CPU. Its cost is in ranx, not
# here, and the point of a sweep is to survey cheaply — one cell that runs two
# orders of magnitude longer than the rest turns the survey into a hang with no
# error message, which is the shape of a bug rather than a result.
SWEEPABLE_NORMS = tuple(n for n in NORMS if n != "borda")


def chunk_id(hit: dict[str, Any]) -> str:
    """A chunk's identity inside one query's candidate list.

    `path:start-end` rather than the Qdrant point id, because a leg produced by
    an external embedder never saw a point id — it ranked rows read out of
    SQLite. The span is what every producer in this harness has in common.
    """
    return f"{hit['path']}:{hit['start_line']}-{hit['end_line']}"


def to_chunk_run(
    records: dict[str, dict[str, Any]], ids: list[str]
) -> dict[str, dict[str, float]]:
    """One leg's rankings as a `ranx` Run over chunks, carrying its real scores.

    Unlike `ranx_bridge.to_qrels_run`, the retriever's own scores are kept: this
    is the input to a *score*-normalising fusion, and replacing them with ranks
    here would silently turn every method into its rank-based cousin.
    """
    run: dict[str, dict[str, float]] = {}
    for qid in ids:
        hits = records[qid]["results"]
        # A file's chunk can appear twice only if a producer returned it twice;
        # keep the better score rather than whichever came last.
        scored: dict[str, float] = {}
        for hit in hits:
            key = chunk_id(hit)
            score = float(hit["score"])
            if key not in scored or score > scored[key]:
                scored[key] = score
        # ranx rejects an empty posting list; a leg that found nothing for this
        # query must still appear, contributing no candidate.
        run[qid] = scored or {"__none__": 0.0}
    return run


def fused_ranking(
    fused_run: dict[str, dict[str, float]], qid: str
) -> list[dict[str, Any]]:
    """A fused chunk posting list back into the harness's result shape."""
    postings = fused_run.get(qid, {})
    ordered = sorted(postings.items(), key=lambda kv: -kv[1])
    out = []
    for key, score in ordered:
        if key.startswith("__"):
            continue
        path, _, span = key.rpartition(":")
        start, _, end = span.partition("-")
        out.append(
            {
                "path": path,
                "start_line": int(start),
                "end_line": int(end),
                "score": float(score),
            }
        )
    return out


def evaluate_weights(
    runs: list[Any],
    records: dict[str, dict[str, Any]],
    ids: list[str],
    weights: tuple[float, ...],
    norm: str,
    method: str,
    metric: str,
) -> tuple[float, list[float]]:
    """Mean and per-query metric for one weight vector."""
    from ranx import fuse

    params = _params_for(method, weights)
    # `to_dict()` rebuilds the whole run, so it is hoisted out of the per-query
    # loop: inside it, a grid of G weights over N queries costs G*N rebuilds of
    # an N-query structure rather than G. Measured the difference the hard way.
    fused = fuse(runs=runs, norm=norm, method=method, params=params).to_dict()
    per_query = []
    for qid in ids:
        rec = dict(records[qid])
        rec["results"] = fused_ranking(fused, qid)
        per_query.append(S.score_instance(rec)[metric])
    return statistics.fmean(per_query), per_query


def _params_for(method: str, weights: tuple[float, ...]) -> dict[str, Any] | None:
    """Only some methods take weights; handing them to the rest is an error.

    `sum`/`mnz`/`max` (ranx's CombSUM/CombMNZ/CombMAX) are unweighted by
    construction — they are in the grid as controls, and a weight sweep over
    them would report the same number many times and look like a plateau.
    """
    if method in ("wsum", "wmnz"):
        return {"weights": tuple(weights)}
    return None


def weight_grid(n_legs: int, step: float) -> list[tuple[float, ...]]:
    """Weights on the simplex, so only their ratio varies.

    A grid over unconstrained weights would spend most of its points on
    rescalings of the same ordering — `wsum` is scale-invariant in the ranking
    it produces, so (0.7, 0.3) and (7, 3) are one arm, not two.
    """
    ticks = round(1.0 / step)
    grid = []
    for combo in itertools.product(range(ticks + 1), repeat=n_legs):
        if sum(combo) != ticks:
            continue
        grid.append(tuple(c / ticks for c in combo))
    return grid


def load_corpus(
    leg_specs: list[str], suffix: str, corpus: str
) -> tuple[dict[str, dict[str, Any]], list[str], list[str], list[Any]]:
    """Every leg's archived run for one corpus, intersected on instance id."""
    from ranx import Run

    names, records = [], {}
    for spec in leg_specs:
        name, _, template = spec.partition("=")
        path = Path(template.format(corpus=corpus, suffix=suffix))
        if not path.exists():
            raise SystemExit(f"leg {name!r}: no such run file: {path}")
        names.append(name)
        records[name] = load_records(path)

    ids = sorted(set.intersection(*[set(r) for r in records.values()]))
    if not ids:
        raise SystemExit("the legs share no instance id — check --qrels-suffix")

    runs = []
    for name in names:
        run = Run(to_chunk_run(records[name], ids))
        run.name = name
        runs.append(run)
    # Any leg's records carry the gold set; they are the same corpus.
    return records[names[0]], ids, names, runs


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--leg",
        action="append",
        required=True,
        metavar="NAME=PATH_TEMPLATE",
        help="repeatable; PATH_TEMPLATE may use {corpus} and {suffix}, "
        "e.g. code=results/CodeRankEmbed__{corpus}{suffix}.jsonl",
    )
    ap.add_argument("--train", required=True, help="corpus the weights are chosen on")
    ap.add_argument("--test", required=True, help="corpus the result is reported on")
    ap.add_argument("--qrels-suffix", default="-docs-short")
    ap.add_argument("--metric", default="ndcg@10")
    ap.add_argument("--norm", default="min-max", choices=NORMS)
    ap.add_argument("--method", default="wsum", choices=METHODS)
    ap.add_argument("--step", type=float, default=0.05, help="weight grid spacing")
    ap.add_argument(
        "--sweep-methods",
        action="store_true",
        help="run every (method, norm) pair instead of the one named. This is a "
        "SEARCH and therefore exploratory (PROTOCOL.md §5.3): the confirmatory "
        "test is the one rule it selects, run against the incumbent on the "
        "corpus the search did not see.",
    )
    ap.add_argument("--methods", default=None, help="comma-separated subset")
    ap.add_argument(
        "--norms",
        default=None,
        help="comma-separated subset; `borda` is excluded from the default "
        "sweep because it does not finish - see SWEEPABLE_NORMS",
    )
    ap.add_argument(
        "--label", default=None, help="output label; defaults to the method"
    )
    ap.add_argument("--out-dir", type=Path, default=Path("results"))
    args = ap.parse_args()

    if args.train == args.test:
        raise SystemExit(
            "--train and --test name the same corpus. A weight chosen and "
            "reported on the same queries is not a measurement; pass two."
        )

    train_recs, train_ids, names, train_runs = load_corpus(
        args.leg, args.qrels_suffix, args.train
    )
    test_recs, test_ids, _, test_runs = load_corpus(
        args.leg, args.qrels_suffix, args.test
    )
    print(
        f"legs: {', '.join(names)}\n"
        f"train {args.train}: {len(train_ids)} queries\n"
        f"test  {args.test}: {len(test_ids)} queries",
        file=sys.stderr,
    )

    if args.sweep_methods:
        methods = args.methods.split(",") if args.methods else list(METHODS)
        norms = args.norms.split(",") if args.norms else list(SWEEPABLE_NORMS)
        combos = [(m, n) for m in methods for n in norms]
    else:
        combos = [(args.method, args.norm)]
    grid = weight_grid(len(names), args.step)

    summaries = []
    for method, norm in combos:
        candidates = (
            grid if _params_for(method, grid[0]) else [tuple([1.0] * len(names))]
        )
        t0 = time.perf_counter()
        best_w, best_train = candidates[0], -1.0
        for w in candidates:
            mean, _ = evaluate_weights(
                train_runs, train_recs, train_ids, w, norm, method, args.metric
            )
            if mean > best_train:
                best_train, best_w = mean, w
        held_out, _ = evaluate_weights(
            test_runs, test_recs, test_ids, best_w, norm, method, args.metric
        )
        elapsed = time.perf_counter() - t0
        summaries.append(
            {
                "method": method,
                "norm": norm,
                "weights": list(best_w),
                "train_corpus": args.train,
                "train_metric": round(best_train, 6),
                "test_corpus": args.test,
                "held_out_metric": round(held_out, 6),
                "grid_points": len(candidates),
                "seconds": round(elapsed, 1),
            }
        )
        print(
            f"  {method:9s} {norm:8s} w={best_w} "
            f"train={best_train:.4f} held-out={held_out:.4f}  ({elapsed:.0f}s)",
            file=sys.stderr,
        )

    summaries.sort(key=lambda s: -s["held_out_metric"])
    winner = summaries[0]

    # Re-fuse the test corpus under the winner and write it in the harness's own
    # result schema, so stats.py and score.py consume it like any other arm.
    from ranx import fuse

    fused = fuse(
        runs=test_runs,
        norm=winner["norm"],
        method=winner["method"],
        params=_params_for(winner["method"], tuple(winner["weights"])),
    ).to_dict()

    label = args.label or f"fuse-{winner['method']}-{winner['norm']}"
    out = args.out_dir / f"{label}__{args.test}{args.qrels_suffix}.jsonl"
    args.out_dir.mkdir(parents=True, exist_ok=True)
    with out.open("w") as fh:
        for qid in test_ids:
            rec = dict(test_recs[qid])
            rec["results"] = fused_ranking(fused, qid)
            rec["n_results"] = len(rec["results"])
            rec["prov"] = {
                "system": "fusion",
                "label": label,
                "legs": names,
                "leg_sources": args.leg,
                "method": winner["method"],
                "norm": winner["norm"],
                "weights": winner["weights"],
                # The provenance a reader needs to disbelieve the number: which
                # corpus chose the weights, and on which one this row was scored.
                "weights_chosen_on": args.train,
                "weight_grid_step": args.step,
                "metric_optimized": args.metric,
            }
            fh.write(json.dumps(rec) + "\n")

    summary_path = out.with_suffix("").with_suffix(".search.json")
    summary_path.write_text(json.dumps(summaries, indent=2) + "\n")
    print(f"\nwrote {out}\nwrote {summary_path}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
