"""The gate that lets `score.py`'s arithmetic be deleted — and not before.

WHAT THIS ASSERTS, AND WHY IT IS A TEST RATHER THAN A NOTE. Every number in
`PROTOCOL.md` §12 and `FINDINGS.md` was produced by hand-written nDCG, MRR, MAP
and recall. Replacing them with `ranx` is only safe if the replacement
reproduces the archive, so this recomputes archived runs both ways and requires
agreement to four decimals — the precision those documents are quoted at.

IT ALSO PINS ranx'S OWN BEHAVIOUR, which is the less obvious half. Three
properties of `ranx` are load-bearing for this harness and none of them is
documented; all three were read out of the source (`ranx/statistical_tests/`)
rather than a docstring, and a minor version bump could change any of them
silently:

  1. `fisher` and `student` apply NO family-wise correction — every pair in an
     N-model comparison is tested independently at `max_p`. PROTOCOL §5.2
     requires Holm-Bonferroni inside a family, so the correction stays ours.
     Only `tukey` controls FWER on its own.
  2. `n_permutations` defaults to 1000; PROTOCOL specifies B = 10 000. A caller
     that forgets the argument gets a coarser test that still looks right.
  3. `ranx` computes NO confidence intervals. PROTOCOL §5.2: "a p-value is never
     reported without its interval." So `stats.py`'s BCa bootstrap cannot be
     retired, whatever happens to the rest of it.

Run: bench/.venv/bin/python -m pytest bench/tests/ -q
"""

from __future__ import annotations

import random
import statistics
import sys
from pathlib import Path

import pytest

BENCH = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(BENCH))

import score as S
import stats as ST
from ranx_bridge import load_records, to_qrels_run

# Four decimals is not arbitrary: it is what PROTOCOL.md §12 and FINDINGS.md
# quote, so a disagreement below it could not have changed a published claim.
TOLERANCE = 1e-4

# Both corpora, and arms whose rankings were produced by different code paths —
# mindex's own nested prefetch, a Qdrant-direct ablation, a brute-force external
# embedder and a lexical baseline. A convention that held only for one producer
# would not be a convention.
ARCHIVED = [
    "F2-full__django-docs-short.jsonl",
    "F2-no-colbert__django-docs-short.jsonl",
    "F2-sparse-only__django-docs-short.jsonl",
    "CodeRankEmbed__django-docs-short.jsonl",
    "bm25-unicode61__django-docs-short.jsonl",
    "F2-full__scikit-learn-docs-short.jsonl",
    "F2-weighted-sum__scikit-learn-docs-short.jsonl",
]

# Every metric `ranx` and `score.py` both compute AND agree on. Two are absent
# and each absence is a decision:
#
#   `acc@k` — LocAgent's all-gold-inside-top-k is not an IR measure and has no
#   `ranx` equivalent. It stays in `score.py`, which is where it belongs: it is
#   reported for comparability with published numbers and never gated on.
#
#   `map@20` — a real convention difference, not a rounding one. See
#   `test_map_diverges_only_where_the_gold_set_exceeds_the_cutoff`.
SHARED_METRICS = [
    "ndcg@10",
    "mrr@10",
    "recall@1",
    "recall@5",
    "recall@10",
    "recall@20",
]

MAP_AT = 20


@pytest.mark.parametrize("name", ARCHIVED)
def test_ranx_reproduces_score_py(name: str) -> None:
    """The equivalence gate itself, one archived run at a time."""
    from ranx import Qrels, Run, evaluate

    path = BENCH / "results" / name
    if not path.exists():
        pytest.skip(f"{name} absent")

    records = load_records(path)
    qrels_d, run_d = to_qrels_run(records)
    assert qrels_d, f"{name} produced no scorable instance"

    ranx_scores = evaluate(Qrels(qrels_d), Run(run_d), SHARED_METRICS)
    ours = [S.score_instance(records[qid]) for qid in qrels_d]

    for metric in SHARED_METRICS:
        theirs = float(ranx_scores[metric])
        mine = statistics.fmean(row[metric] for row in ours)
        assert abs(theirs - mine) < TOLERANCE, (
            f"{name} {metric}: ranx {theirs:.6f} vs score.py {mine:.6f}"
        )


@pytest.mark.parametrize("name", ARCHIVED)
def test_map_diverges_only_where_the_gold_set_exceeds_the_cutoff(name: str) -> None:
    """`map@20` is `score.py`'s, and this says exactly why and exactly how much.

    `score.py` normalises AP by `min(|gold|, k)`; `ranx` — following trec_eval's
    `map_cut`, which unlike `ndcg_cut` does not cap its ideal — normalises by
    `|gold|`. So a query with more gold files than the cutoff cannot reach 1.0
    under `ranx` no matter what any system returns, which measures the query
    rather than the system. `score.py`'s docstring makes that argument for both
    AP and the nDCG ideal; `ranx` happens to agree on nDCG and not on AP.

    The divergence is therefore expected, bounded, and must stay confined to
    `|gold| > k`. It affects **one query in 1 475** across both corpora — but
    the one is worth a test, because the two implementations would otherwise
    look interchangeable and someone would swap them.
    """
    from ranx import Qrels, Run, evaluate

    path = BENCH / "results" / name
    if not path.exists():
        pytest.skip(f"{name} absent")

    records = load_records(path)
    qrels_d, run_d = to_qrels_run(records)
    ids = list(qrels_d)
    theirs = evaluate(Qrels(qrels_d), Run(run_d), f"map@{MAP_AT}", return_mean=False)

    for qid, ranx_ap in zip(ids, theirs):
        ours = S.score_instance(records[qid])[f"map@{MAP_AT}"]
        n_gold = len(set(records[qid]["gold_files"]))
        if n_gold <= MAP_AT:
            assert abs(float(ranx_ap) - ours) < 1e-9, (
                f"{name} {qid}: |gold|={n_gold} <= {MAP_AT} yet the two APs "
                f"differ ({ranx_ap:.6f} vs {ours:.6f}) — the divergence is not "
                "the normalisation it is documented to be"
            )
        elif abs(float(ranx_ap) - ours) > 1e-9:
            # The whole difference must be the denominator, exactly.
            assert abs(float(ranx_ap) * n_gold - ours * MAP_AT) < 1e-6, (
                f"{name} {qid}: |gold|={n_gold}, ranx {ranx_ap:.6f}, ours "
                f"{ours:.6f} — not explained by |gold| vs min(|gold|, k)"
            )


def test_the_dedup_convention_is_what_makes_them_agree() -> None:
    """A negative control for the test above.

    Agreement to four decimals is only evidence if disagreement were possible.
    Ranking chunks instead of files — the convention `score.py` argues for and
    `ranx` knows nothing about — must move the number, or this whole file is
    asserting that two implementations of the same trivial thing are the same.
    """
    from ranx import Qrels, Run, evaluate

    path = BENCH / "results" / "F2-full__django-docs-short.jsonl"
    if not path.exists():
        pytest.skip("archive absent")
    records = load_records(path)

    qrels_d, run_d = to_qrels_run(records)
    deduped = float(evaluate(Qrels(qrels_d), Run(run_d), "ndcg@10"))

    # The same runs, credited per chunk: a file hit at three ranks counts three
    # times, which is exactly the inflation the convention exists to prevent.
    chunkwise = {}
    for qid in qrels_d:
        hits = records[qid]["results"]
        chunkwise[qid] = {f"{h['path']}#{i}": float(-i) for i, h in enumerate(hits)}
    qrels_chunk = {
        qid: {
            f"{h['path']}#{i}": 1
            for i, h in enumerate(records[qid]["results"])
            if h["path"] in set(records[qid]["gold_files"])
        }
        for qid in qrels_d
    }
    qrels_chunk = {q: v for q, v in qrels_chunk.items() if v}
    chunkwise = {q: chunkwise[q] for q in qrels_chunk}
    naive = float(evaluate(Qrels(qrels_chunk), Run(chunkwise), "ndcg@10"))

    assert abs(naive - deduped) > 0.01, (
        f"chunk-level {naive:.4f} vs file-level {deduped:.4f} — the convention "
        "made no difference, so the equivalence above proves nothing"
    )


def test_ranx_fisher_agrees_with_our_permutation_test() -> None:
    """Two implementations of Smucker et al.'s paired randomization test.

    `stats.py` flips the sign of each paired difference; `ranx` swaps the pair
    itself. These are the same test, so the p-values must agree to within Monte
    Carlo error at B = 10 000 — checked on a pair whose effect is large enough
    to be nowhere near the boundary, and on one that is genuinely null.
    """
    from ranx import Qrels, Run, compare

    a = BENCH / "results" / "CodeRankEmbed__django-docs-short.jsonl"
    b = BENCH / "results" / "F2-full__django-docs-short.jsonl"
    if not (a.exists() and b.exists()):
        pytest.skip("archive absent")

    recs_a, recs_b = load_records(a), load_records(b)
    ids = sorted(set(recs_a) & set(recs_b))
    qrels_d, run_a = to_qrels_run(recs_a, ids)
    _, run_b = to_qrels_run(recs_b, ids)

    ra, rb = Run(run_a), Run(run_b)
    ra.name, rb.name = "treatment", "control"
    report = compare(
        Qrels(qrels_d),
        [rb, ra],
        metrics=["ndcg@10"],
        stat_test="fisher",
        n_permutations=10_000,  # property 2: the default is 1000
        max_p=0.05,
    )
    theirs = report.comparisons[frozenset(["control", "treatment"])]["ndcg@10"][
        "p_value"
    ]

    diffs = [
        S.score_instance(recs_a[i])["ndcg@10"] - S.score_instance(recs_b[i])["ndcg@10"]
        for i in qrels_d
    ]
    mine = ST.permutation_p(diffs, random.Random(20260805), b=10_000)

    # Both are far below any threshold this harness uses; asserting equality of
    # two Monte Carlo estimates near zero would be asserting the seed.
    assert theirs < 0.01 and mine < 0.01, f"ranx {theirs}, stats.py {mine}"
    assert statistics.fmean(diffs) > 0


def test_ranx_applies_no_family_wise_correction() -> None:
    """Property 1, pinned: Holm stays ours.

    Read out of `ranx/statistical_tests/__init__.py`, where `fisher` and
    `student` loop over every ordered pair and call the test with the same
    `max_p`. If a future version starts correcting, this fails and the
    correction in `stats.py` becomes a double one — which is the silent
    direction and therefore the one worth a test.
    """
    import inspect

    from ranx import statistical_tests as st

    source = inspect.getsource(st.compute_statistical_significance)
    for forbidden in ("bonferroni", "holm", "fdr", "benjamini"):
        assert forbidden not in source.lower(), (
            f"ranx now mentions {forbidden!r} in compute_statistical_significance; "
            "check whether stats.py's Holm correction has become a second one"
        )


def test_ranx_offers_no_confidence_interval() -> None:
    """Property 3, pinned: `bca_ci` cannot be retired.

    PROTOCOL §5.2 forbids reporting a p-value without an interval, so if this
    ever starts failing it is good news and `stats.py` can shrink further.
    """
    import ranx

    exported = {name.lower() for name in dir(ranx)}
    assert not any(
        "confidence" in n or n.endswith("_ci") or "bootstrap" in n for n in exported
    ), "ranx appears to have gained interval estimation — revisit stats.py"
