#!/usr/bin/env python3
"""Paired comparison of two runs: permutation test, bootstrap CI, TOST.

PROTOCOL §5. A difference in two macro means is not a result; this is what
turns one into a claim or refuses to.

  * **Paired two-sided randomization test**, B = 10 000 (Smucker, Allan &
    Carterette, CIKM 2007). Queries are the unit; the pairing is what makes the
    test powerful, since per-query metric variance dwarfs the between-system
    difference. Under the null the two systems' scores on a query are
    exchangeable, so each resample flips a fair coin per query.
  * **BCa bootstrap 95% CI** on the mean difference. A p-value without an
    interval is not reported (§5.2) — "significant" says the sign is probably
    right and nothing about whether the size is worth the 99.6% of stored bytes
    it costs.
  * **TOST** against a pre-registered margin, for the non-inferiority gate
    (§5.5). Reported here so the same script serves the release comparison and
    the CI gate.
  * **Per stratum**, because §3.0.1 exists: a pooled win that lives entirely in
    the `obvious` bucket is a win at exactly what the cheap baseline already
    does.

Both files must cover the same instance ids; the intersection is used and any
asymmetry is reported rather than absorbed.
"""

from __future__ import annotations

import argparse
import json
import math
import random
import statistics
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from score import score_all

B_PERM = 10_000
B_BOOT = 10_000
SEED = 20260805


def per_query(path: Path, metric: str) -> dict[str, float]:
    records = [json.loads(line) for line in path.open()]
    return {r["instance_id"]: r[metric] for r in score_all(records)}


def strata(path: Path, field: str = "overlap_bucket") -> dict[str, str]:
    """The stratum each instance belongs to, for the per-stratum breakdown.

    Parameterised because F10 (PROTOCOL §9.6) is decided on `ident_in_gold` and
    reported per `projection`, and hardcoding the difficulty axis meant the two
    strata that referee that family could not be reported at all. `False` is a
    stratum, so the mapping is on `is None` rather than falsiness — read as a
    boolean, every `ident_in_gold = false` instance would have collapsed into
    "unbucketed" together with the tiers that carry no such field, which is the
    one merge that would silently answer F10 in the affirmative.
    """
    out = {}
    for line in path.open():
        r = json.loads(line)
        value = r.get(field)
        out[r["instance_id"]] = "unbucketed" if value is None else str(value)
    return out


def permutation_p(diffs: list[float], rng: random.Random, b: int = B_PERM) -> float:
    """Two-sided. Under H0 the sign of each paired difference is arbitrary."""
    observed = abs(statistics.fmean(diffs))
    n = len(diffs)
    hits = 0
    for _ in range(b):
        total = 0.0
        for d in diffs:
            total += d if rng.random() < 0.5 else -d
        if abs(total / n) >= observed - 1e-12:
            hits += 1
    # Add-one, so a p of exactly 0 is never reported from a finite resample.
    return (hits + 1) / (b + 1)


def bca_ci(
    diffs: list[float], rng: random.Random, alpha: float = 0.05, b: int = B_BOOT
) -> tuple[float, float]:
    """Bias-corrected and accelerated bootstrap on the mean difference."""
    n = len(diffs)
    theta = statistics.fmean(diffs)
    boots = []
    for _ in range(b):
        sample = [diffs[rng.randrange(n)] for _ in range(n)]
        boots.append(statistics.fmean(sample))
    boots.sort()

    # Bias correction: how far the bootstrap distribution sits off the estimate.
    below = sum(1 for x in boots if x < theta)
    if below in (0, b):
        # Degenerate — every resample on one side. Fall back to the percentile
        # interval rather than dividing by an infinite z.
        lo = boots[max(0, int(alpha / 2 * b) - 1)]
        hi = boots[min(b - 1, int((1 - alpha / 2) * b))]
        return lo, hi
    z0 = _ppf(below / b)

    # Acceleration from the jackknife.
    total = sum(diffs)
    jack = [(total - d) / (n - 1) for d in diffs]
    jbar = statistics.fmean(jack)
    num = sum((jbar - x) ** 3 for x in jack)
    den = 6 * (sum((jbar - x) ** 2 for x in jack) ** 1.5)
    a = num / den if den else 0.0

    def endpoint(p: float) -> float:
        z = _ppf(p)
        adj = z0 + (z0 + z) / (1 - a * (z0 + z))
        idx = int(_cdf(adj) * b)
        return boots[min(b - 1, max(0, idx))]

    return endpoint(alpha / 2), endpoint(1 - alpha / 2)


def _ppf(p: float) -> float:
    """Inverse normal CDF (Acklam's rational approximation, ~1e-9 absolute)."""
    if p <= 0:
        return -8.0
    if p >= 1:
        return 8.0
    a = [
        -3.969683028665376e01,
        2.209460984245205e02,
        -2.759285104469687e02,
        1.383577518672690e02,
        -3.066479806614716e01,
        2.506628277459239e00,
    ]
    b = [
        -5.447609879822406e01,
        1.615858368580409e02,
        -1.556989798598866e02,
        6.680131188771972e01,
        -1.328068155288572e01,
    ]
    c = [
        -7.784894002430293e-03,
        -3.223964580411365e-01,
        -2.400758277161838e00,
        -2.549732539343734e00,
        4.374664141464968e00,
        2.938163982698783e00,
    ]
    d = [
        7.784695709041462e-03,
        3.224671290700398e-01,
        2.445134137142996e00,
        3.754408661907416e00,
    ]
    pl, ph = 0.02425, 1 - 0.02425
    if p < pl:
        q = math.sqrt(-2 * math.log(p))
        return (((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5]) / (
            (((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1
        )
    if p > ph:
        q = math.sqrt(-2 * math.log(1 - p))
        return -(
            ((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5]
        ) / ((((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1)
    q = p - 0.5
    r = q * q
    return (
        (((((a[0] * r + a[1]) * r + a[2]) * r + a[3]) * r + a[4]) * r + a[5])
        * q
        / (((((b[0] * r + b[1]) * r + b[2]) * r + b[3]) * r + b[4]) * r + 1)
    )


def _cdf(z: float) -> float:
    return 0.5 * (1 + math.erf(z / math.sqrt(2)))


def compare(
    name: str, diffs: list[float], rng: random.Random, delta: float | None
) -> dict[str, Any]:
    n = len(diffs)
    mean = statistics.fmean(diffs)
    p = permutation_p(diffs, rng)
    lo, hi = bca_ci(diffs, rng)
    row: dict[str, Any] = {
        "stratum": name,
        "n": n,
        "mean_delta": round(mean, 4),
        "ci95": [round(lo, 4), round(hi, 4)],
        "p_permutation": round(p, 5),
    }
    if delta is not None:
        # TOST: non-inferiority holds when the whole interval clears -delta.
        row["non_inferior"] = lo > -delta
        row["margin"] = delta
    return row


def self_test() -> int:
    """The two checks §10 requires, because a wrong test is worse than none.

    Under a true null the p-value must be uniform — so it must be below 0.05
    about 5% of the time, no more. And a 95% interval must contain the true
    mean about 95% of the time. Both are checked by simulation rather than
    asserted, since either failing silently would let this file certify
    whatever it was pointed at.
    """
    rng = random.Random(1)
    failures = []

    # 1. Null: paired samples from the same distribution. Metric values are
    # bounded in [0,1] and heavily zero-inflated, like nDCG actually is — a
    # test validated on Gaussians is not validated for this.
    def draw() -> float:
        return 0.0 if rng.random() < 0.45 else rng.random()

    trials, n, small = 400, 200, 0
    for _ in range(trials):
        diffs = [draw() - draw() for _ in range(n)]
        if permutation_p(diffs, rng, b=400) < 0.05:
            small += 1
    rate = small / trials
    ok = 0.02 <= rate <= 0.09
    print(
        f"  {'ok  ' if ok else 'FAIL'} null p<0.05 in {rate:.1%} of trials (want ~5%)"
    )
    if not ok:
        failures.append("type-I rate")

    # 2. Power, checked against the analytic value rather than a number chosen
    # by hand. The first version of this asserted ">= 80% at a shift of 0.03,
    # n = 1000" and failed at 57% — which was the TEST being right and the
    # expectation wrong: these draws are INDEPENDENT, and a paired test earns
    # its power from correlation (two retrieval systems fail on the same hard
    # query). Uncorrelated, the normal approximation gives ~53%, so 57% was
    # agreement. Asserting agreement is the check; asserting a level is not.
    shift, n2, trials2 = 0.03, 1000, 120
    hits = 0
    sds = []
    for _ in range(trials2):
        diffs = [draw() - draw() + shift for _ in range(n2)]
        sds.append(statistics.stdev(diffs))
        if permutation_p(diffs, rng, b=400) < 0.05:
            hits += 1
    empirical = hits / trials2
    se = statistics.fmean(sds) / math.sqrt(n2)
    analytic = 1 - _cdf(1.959964 - shift / se) + _cdf(-1.959964 - shift / se)
    ok = abs(empirical - analytic) < 0.12
    print(
        f"  {'ok  ' if ok else 'FAIL'} power at shift {shift}, n={n2}: "
        f"{empirical:.0%} empirical vs {analytic:.0%} analytic"
    )
    if not ok:
        failures.append("power")

    # 2b. And with the pairing real data has, the same shift must be found
    # every time — which is what makes this benchmark's n worth having.
    hits = 0
    for _ in range(40):
        base = [draw() for _ in range(n2)]
        diffs = [(x + shift + rng.gauss(0, 0.08)) - x for x in base]
        if permutation_p(diffs, rng, b=400) < 0.05:
            hits += 1
    ok = hits >= 38
    print(f"  {'ok  ' if ok else 'FAIL'} same shift on CORRELATED pairs: {hits}/40")
    if not ok:
        failures.append("paired power")

    # 3. Coverage of the BCa interval around a known true mean.
    covered = 0
    trials = 300
    for _ in range(trials):
        diffs = [draw() - draw() + 0.05 for _ in range(200)]
        lo, hi = bca_ci(diffs, rng, b=600)
        if lo <= 0.05 <= hi:
            covered += 1
    rate = covered / trials
    ok = 0.90 <= rate <= 0.99
    print(f"  {'ok  ' if ok else 'FAIL'} 95% CI covered the truth {rate:.1%} of trials")
    if not ok:
        failures.append("coverage")

    # 4. The inverse normal CDF the BCa endpoints rest on.
    ok = all(abs(_ppf(_cdf(z)) - z) < 1e-4 for z in (-2.5, -1.0, 0.0, 0.7, 1.96, 3.0))
    print(f"  {'ok  ' if ok else 'FAIL'} ppf inverts cdf")
    if not ok:
        failures.append("ppf")

    print("\nself-test:", "FAILED" if failures else "PASS")
    return 1 if failures else 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("system", type=Path, help="the run under test")
    ap.add_argument("baseline", type=Path)
    ap.add_argument("--metric", default="ndcg@10")
    ap.add_argument("--delta", type=float, default=None, help="TOST margin")
    ap.add_argument(
        "--stratum-field",
        default="overlap_bucket",
        help="per-query field to break the comparison down by "
        "(F10 reports on ident_in_gold and projection)",
    )
    ap.add_argument("--json", type=Path)
    args = ap.parse_args()

    a = per_query(args.system, args.metric)
    b = per_query(args.baseline, args.metric)
    bucket = strata(args.system, args.stratum_field)

    shared = sorted(set(a) & set(b))
    if len(shared) != len(a) or len(shared) != len(b):
        print(
            f"  WARN: {len(a)} vs {len(b)} queries, {len(shared)} shared. "
            f"Only the intersection is compared."
        )
    if not shared:
        raise SystemExit("no shared instance ids — these are not the same query set")

    rng = random.Random(SEED)
    print(f"{args.system.stem}  vs  {args.baseline.stem}   [{args.metric}]")
    print(
        f"  {statistics.fmean(a[i] for i in shared):.4f} "
        f"vs {statistics.fmean(b[i] for i in shared):.4f}\n"
    )

    rows = [compare("ALL", [a[i] - b[i] for i in shared], rng, args.delta)]
    for name in sorted({bucket[i] for i in shared}):
        ids = [i for i in shared if bucket[i] == name]
        if len(ids) >= 20:
            rows.append(compare(name, [a[i] - b[i] for i in ids], rng, args.delta))

    head = f"  {'stratum':<14}{'n':>6}{'Δ mean':>10}{'95% CI':>20}{'p':>10}"
    print(head + ("   TOST" if args.delta else ""))
    for r in rows:
        line = (
            f"  {r['stratum']:<14}{r['n']:>6}{r['mean_delta']:>10.4f}"
            f"{'[' + f'{r['ci95'][0]:+.4f}, {r['ci95'][1]:+.4f}' + ']':>20}"
            f"{r['p_permutation']:>10.4f}"
        )
        if args.delta:
            line += "   PASS" if r["non_inferior"] else "   FAIL"
        print(line)

    print(
        "\n  A CI containing 0 means the sign is not established. The interval, "
        "not the p-value, is what says whether a difference is worth its cost."
    )
    if args.json:
        args.json.write_text(
            json.dumps(
                {
                    "system": args.system.name,
                    "baseline": args.baseline.name,
                    "metric": args.metric,
                    # Which axis the per-stratum rows below are cut on. Two
                    # archives that break the same comparison down differently
                    # are otherwise indistinguishable once the console output
                    # has scrolled away.
                    "stratum_field": args.stratum_field,
                    "seed": SEED,
                    "b_permutation": B_PERM,
                    "b_bootstrap": B_BOOT,
                    "rows": rows,
                },
                indent=2,
            )
        )
        print(f"  -> {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
