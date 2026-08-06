#!/usr/bin/env python3
"""PROTOCOL.md §5.1 — run the identical configuration N times and publish its spread.

This is the first measurement the benchmark makes and it involves no
comparison, which is the point: a harness that cannot state its own
reproducibility cannot support a claim about a two-point difference. Every
later result is read against the number this produces, and the non-inferiority
margin δ is derived from it by the rule fixed in §5.5 before any of this data
existed:

    δ = 2 × pooled between-run SD, rounded UP to the nearest 0.005,
        floor 0.01 nDCG@10.

WHY THE REPETITIONS REINDEX. Re-querying one index measures the query path
alone, and the query path is the half least likely to move: mindex's own
retrieval is deterministic given a set of vectors. The variance worth knowing
comes from the embedder — fp16 on a GPU, with two backends CLAUDE.md records
as not bit-identical — and it enters when the corpus is embedded. So each
repetition gets its own project GUID and its own collection, built from
scratch. Anything cheaper would report a reassuring zero.

WHAT THE POWER FIGURE CAN AND CANNOT SAY. §5.2 asks for the query count needed
to detect δ = 0.02 nDCG@10 at 80% power. That needs the SD of the per-query
DIFFERENCE between two systems, and at this point there is only one system.
Two bounds are therefore published rather than one number pretending to be
exact: the optimistic one uses the difference SD observed between repetitions
of the same configuration (a floor — two genuinely different systems disagree
more), and the conservative one assumes the two systems' per-query scores are
independent, giving σ_d = √2 × σ_query (a ceiling — a real A/B is positively
correlated, since both systems find the easy queries). The true requirement
lies between them.
"""

from __future__ import annotations

import argparse
import itertools
import json
import math
import statistics
import subprocess
import sys
from pathlib import Path
from typing import Any

import httpx
from fetch import load_config, repo_root, select_corpora
from run import Server, project_guid
from score import HEADLINE, PRIMARY, load_rows, mean, score_all, stratify

# §5.5, fixed before the data existed.
DELTA_MULTIPLIER = 2.0
DELTA_ROUNDING = 0.005
DELTA_FLOOR = 0.01

# §5.2: the effect the benchmark is sized to detect, and the test it is sized for.
TARGET_DELTA = 0.02
ALPHA = 0.05
POWER = 0.80


def z(p: float) -> float:
    """Standard-normal quantile.

    Hand-rolled rather than imported so the one place a distribution enters the
    protocol is visible in this file; the inverse-erf identity is exact enough
    for a sample-size figure reported to the nearest query.
    """
    return math.sqrt(2.0) * _erfinv(2.0 * p - 1.0)


def _erfinv(x: float) -> float:
    # Newton refinement on Winitzki's initial estimate; converges to machine
    # precision in a handful of steps over the range this file uses.
    a = 0.147
    ln = math.log(1.0 - x * x)
    t = 2.0 / (math.pi * a) + ln / 2.0
    guess = math.copysign(math.sqrt(math.sqrt(t * t - ln / a) - t), x)
    for _ in range(4):
        err = math.erf(guess) - x
        guess -= err / (2.0 / math.sqrt(math.pi) * math.exp(-guess * guess))
    return guess


def round_up_to(value: float, step: float) -> float:
    return math.ceil(value / step - 1e-12) * step


def sd(values: list[float]) -> float:
    """Sample SD; zero for a single observation rather than an error."""
    return statistics.stdev(values) if len(values) > 1 else 0.0


def run_once(
    root: Path,
    corpus_names: list[str],
    label: str,
    extra: list[str],
    *,
    fresh: bool,
    run_tag: str = "",
) -> None:
    cmd = [
        sys.executable,
        str(root / "bench" / "run.py"),
        "--label",
        label,
        # The equivalence check answers a question about mindex's skip logic,
        # not about run-to-run variance, and the ordinary run already asserts
        # it. Repeating it per repetition doubles the cost for nothing.
        "--equivalence-sample",
        "0",
        *extra,
    ]
    if fresh:
        cmd.append("--fresh")
    if run_tag:
        cmd += ["--run-tag", run_tag]
    for name in corpus_names:
        cmd += ["--corpus", name]
    proc = subprocess.run(cmd, check=False)
    if proc.returncode != 0:
        raise SystemExit(
            f"repetition {label}{run_tag} failed; the noise floor is not measurable"
        )


def per_run_scores(
    results_dir: Path, run_name: str, qrels_names: list[str]
) -> list[dict[str, Any]]:
    """Score one repetition. `run_name` is label+tag, as run.py names the file."""
    paths = [results_dir / f"{run_name}__{q}.jsonl" for q in qrels_names]
    missing = [p for p in paths if not p.exists()]
    if missing:
        raise SystemExit(f"missing results: {[str(p) for p in missing]}")
    return score_all(load_rows(paths))


def corpus_means(rows: list[dict[str, Any]]) -> dict[str, dict[str, float]]:
    out = {}
    for corpus, group in stratify(rows, "corpus").items():
        out[corpus] = {k: mean([r[k] for r in group]) for k in HEADLINE}
    return out


def analyse(runs: list[list[dict[str, Any]]]) -> dict[str, Any]:
    per_run = [corpus_means(rows) for rows in runs]
    corpora = sorted(per_run[0])

    # Between-run SD of each corpus's mean, and of the macro figure.
    by_corpus: dict[str, dict[str, float]] = {}
    for corpus in corpora:
        by_corpus[corpus] = {
            metric: sd([run[corpus][metric] for run in per_run]) for metric in HEADLINE
        }
    macro_per_run = [
        {m: mean([run[c][m] for c in corpora]) for m in HEADLINE} for run in per_run
    ]
    macro_sd = {m: sd([run[m] for run in macro_per_run]) for m in HEADLINE}

    # Pooled: the root-mean-square of the per-corpus SDs. Averaging the SDs
    # themselves would understate the spread, since variance is what adds.
    pooled_sd = {
        metric: math.sqrt(mean([by_corpus[c][metric] ** 2 for c in corpora]))
        for metric in HEADLINE
    }

    delta = max(
        DELTA_FLOOR,
        round_up_to(DELTA_MULTIPLIER * pooled_sd[PRIMARY], DELTA_ROUNDING),
    )

    # How many individual queries moved at all between repetitions — the most
    # legible statement of reproducibility there is, and independent of any
    # aggregation choice.
    keyed = [{r["instance_id"]: r for r in rows} for rows in runs]
    shared = sorted(set.intersection(*[set(k) for k in keyed]))
    unstable = [
        iid for iid in shared if len({round(k[iid][PRIMARY], 9) for k in keyed}) > 1
    ]

    # Per-query difference SD between repetitions of the SAME configuration:
    # the floor for a real comparison's σ_d.
    diffs: list[float] = []
    for a, b in itertools.combinations(range(len(keyed)), 2):
        diffs += [keyed[a][iid][PRIMARY] - keyed[b][iid][PRIMARY] for iid in shared]
    sigma_d_observed = sd(diffs)

    # Per-query SD of the metric itself, giving the independent-systems ceiling.
    sigma_query = sd([mean([k[iid][PRIMARY] for k in keyed]) for iid in shared])
    sigma_d_ceiling = math.sqrt(2.0) * sigma_query

    z_sum = z(1.0 - ALPHA / 2.0) + z(POWER)

    def n_required(sigma: float) -> int | None:
        if sigma <= 0:
            return None
        return math.ceil((z_sum * sigma / TARGET_DELTA) ** 2)

    return {
        "repetitions": len(runs),
        "n_queries": len(shared),
        "corpora": corpora,
        "per_run_macro": macro_per_run,
        "between_run_sd_per_corpus": by_corpus,
        "between_run_sd_macro": macro_sd,
        "pooled_between_run_sd": pooled_sd,
        "primary_metric": PRIMARY,
        "delta": delta,
        "delta_rule": (
            f"max({DELTA_FLOOR}, roundup({DELTA_MULTIPLIER} × pooled SD, "
            f"{DELTA_ROUNDING}))"
        ),
        "unstable_queries": len(unstable),
        "unstable_query_ids": unstable[:50],
        "sigma_d_observed": sigma_d_observed,
        "sigma_query": sigma_query,
        "sigma_d_independent_ceiling": sigma_d_ceiling,
        "power_target_delta": TARGET_DELTA,
        "queries_needed_optimistic": n_required(sigma_d_observed),
        "queries_needed_conservative": n_required(sigma_d_ceiling),
    }


def print_analysis(a: dict[str, Any]) -> None:
    print(f"\n{'=' * 68}")
    print(f"NOISE FLOOR — {a['repetitions']} identical runs, {a['n_queries']} queries")
    print("=" * 68)

    print(f"\nmacro {a['primary_metric']} per repetition:")
    for i, run in enumerate(a["per_run_macro"], start=1):
        print(f"  run {i}: {run[a['primary_metric']]:.6f}")

    print("\nbetween-run SD:")
    for metric in HEADLINE:
        print(
            f"  {metric:<12} macro={a['between_run_sd_macro'][metric]:.6f}  "
            f"pooled={a['pooled_between_run_sd'][metric]:.6f}"
        )

    print("\nper corpus (SD of the corpus mean):")
    for corpus, metrics in a["between_run_sd_per_corpus"].items():
        print(
            f"  {corpus:<16} {a['primary_metric']}={metrics[a['primary_metric']]:.6f}"
        )

    pct = 100.0 * a["unstable_queries"] / a["n_queries"] if a["n_queries"] else 0.0
    print(
        f"\nqueries whose {a['primary_metric']} was not identical across all "
        f"repetitions: {a['unstable_queries']}/{a['n_queries']} ({pct:.1f}%)"
    )

    print(f"\nδ (PROTOCOL §5.5) = {a['delta']:.4f} {a['primary_metric']}")
    print(f"  rule: {a['delta_rule']}")
    print(
        f"  nothing smaller than {DELTA_MULTIPLIER:.0f} × pooled SD "
        f"({DELTA_MULTIPLIER * a['pooled_between_run_sd'][a['primary_metric']]:.6f}) "
        "is reportable as a finding, whatever its p-value."
    )

    print(
        f"\npower to detect δ = {a['power_target_delta']} at {POWER:.0%}, α = {ALPHA}:"
    )
    print(f"  σ_d observed between repetitions: {a['sigma_d_observed']:.6f}")
    print(
        f"  σ_d ceiling (independent systems): {a['sigma_d_independent_ceiling']:.6f}"
    )
    print(f"  queries needed, optimistic:    {a['queries_needed_optimistic']}")
    print(f"  queries needed, conservative:  {a['queries_needed_conservative']}")


def main() -> int:
    root = repo_root()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=root / "bench" / "corpora.toml")
    parser.add_argument("--corpus", action="append", dest="corpora")
    parser.add_argument("--tier", type=int, default=0)
    parser.add_argument("--qrels-suffix", default="")
    parser.add_argument(
        "--index-repeats",
        type=int,
        default=10,
        help="full rebuilds, each into its own project. The expensive half, "
        "and the one δ is derived from.",
    )
    parser.add_argument(
        "--query-repeats",
        type=int,
        default=10,
        help="re-queries of ONE index, which isolates the share of the noise "
        "that indexing cannot be blamed for",
    )
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--label-prefix", default="noise")
    parser.add_argument(
        "--reuse-label",
        default=None,
        help="re-query an index a previous run.py already built, instead of "
        "rebuilding one. The right choice when the comparisons being sized are "
        "same-index ones (F1 reads mindex's chunks, F2 queries its collection), "
        "which no amount of embedder variance can reach.",
    )
    parser.add_argument(
        "--skip-run",
        action="store_true",
        help="analyse repetitions already on disk instead of producing them",
    )
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()

    config = load_config(args.config)
    corpora = select_corpora(config, args.corpora, args.tier)
    if not corpora:
        raise SystemExit("no corpora selected")
    names = [c["name"] for c in corpora]
    qrels_names = [f"{n}{args.qrels_suffix}" for n in names]
    results_dir = root / config["run"]["results_dir"]
    passthrough = [
        "--config",
        str(args.config),
        # `=`-joined, not two tokens: the descriptive suffix is `-docs`, and
        # argparse reads a leading `-` as the next option rather than as this
        # one's value.
        f"--qrels-suffix={args.qrels_suffix}",
        "--concurrency",
        str(args.concurrency),
    ]

    prefix = f"{args.label_prefix}{args.concurrency}"
    index_labels = [f"{prefix}i{i}" for i in range(1, args.index_repeats + 1)]
    query_tags = [f"q{i}" for i in range(2, args.query_repeats + 1)]

    # Re-querying an index that already exists is the whole measurement when the
    # comparisons at hand are same-index ones: F1 reads mindex's own chunks and
    # F2 queries mindex's own collection, so NEITHER can be moved by the
    # embedder variance that `--index-repeats` exists to capture. Charging them
    # a rebuild margin would refuse a real effect on the strength of noise that
    # cannot reach them.
    requery_label = args.reuse_label or (index_labels[-1] if index_labels else None)
    if requery_label is None and query_tags:
        raise SystemExit(
            "--query-repeats needs an index to re-query: either build some with "
            "--index-repeats, or name an existing one with --reuse-label."
        )

    if not args.skip_run:
        server = Server(
            config["run"]["server_url"], verify=not config["run"]["no_verify"]
        )
        for i, label in enumerate(index_labels, start=1):
            print(f"\n### rebuild {i}/{args.index_repeats} ({label})")
            run_once(root, names, label, passthrough, fresh=True)
            if label != index_labels[-1]:
                # Each repetition is its own project, so its own Qdrant
                # collection: ten django rebuilds held at once would be ~200 GiB
                # for no reason, since the results are already on disk. The last
                # one survives because the re-query phase needs an index.
                for corpus_name in names:
                    guid = project_guid(label, corpus_name)
                    try:
                        server.delete_project(guid)
                    except httpx.HTTPError as exc:
                        print(f"  WARN: could not drop {guid}: {exc}")
        server.close()
        # Re-query the last index. Its own first pass is already on disk from
        # the rebuild above, so only the extra passes are run here.
        for tag in query_tags:
            print(f"\n### re-query {tag} of {requery_label} (no rebuild)")
            assert requery_label is not None
            run_once(root, names, requery_label, passthrough, fresh=False, run_tag=tag)

    rebuilds = [per_run_scores(results_dir, lb, qrels_names) for lb in index_labels]
    requeries = (
        [
            per_run_scores(results_dir, f"{requery_label}{t}", qrels_names)
            for t in ([""] + query_tags)
        ]
        if requery_label
        else []
    )

    analysis = analyse(rebuilds)
    analysis["concurrency"] = args.concurrency
    analysis["qrels_suffix"] = args.qrels_suffix

    # The decomposition. Rebuild noise contains query noise, so the difference
    # is what indexing adds — and only that part is addressable by changing how
    # the corpus is embedded.
    if len(requeries) > 1:
        query_only = analyse(requeries)
        analysis["query_only"] = {
            "repeats": len(requeries),
            "between_run_sd_macro": query_only["between_run_sd_macro"],
            "unstable_queries": query_only["unstable_queries"],
            "n_queries": query_only["n_queries"],
        }

    print_analysis(analysis)
    q = analysis.get("query_only")
    if q:
        total = analysis["between_run_sd_macro"][PRIMARY]
        qsd = q["between_run_sd_macro"][PRIMARY]
        print(f"\n{'=' * 68}")
        print("WHERE THE NOISE LIVES")
        print("=" * 68)
        print(f"  rebuild + re-query (total)   SD = {total:.6f}")
        print(f"  re-query one index only      SD = {qsd:.6f}")
        share = 100.0 * qsd / total if total else 0.0
        print(
            f"  the query path alone accounts for {share:.0f}% of the spread; "
            f"the rest is what rebuilding the index adds."
        )
        print(
            f"  queries unstable on a FROZEN index: "
            f"{q['unstable_queries']}/{q['n_queries']} — nothing was reindexed "
            f"and no vector changed, so this part is the vector store."
        )

    out = (
        args.json
        or results_dir / f"noise_floor{args.qrels_suffix}-c{args.concurrency}.json"
    )
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(analysis, indent=2, sort_keys=True) + "\n")
    print(f"\nwrote {out.relative_to(root)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
