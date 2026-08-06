#!/usr/bin/env python3
"""Regenerate `bench/published/` — the artefacts a reader can check a number against.

WHY THIS EXISTS. `results/` is gitignored: one run is up to 23 MB of ranked
lists and a full tier-1 pass is 654 MB, none of which belongs in git. The
consequence went unnoticed for the whole first round — **nothing a reader could
use to verify a published number was committed**, and PROTOCOL §5.6's "a corpus
is frozen when its output is committed" pointed at no committed output. A prose
table is not a freeze record; it is a claim about one.

What is committed instead is the *small* end of the same pipeline, which is
enough to check every number in PROTOCOL §12 and FINDINGS:

  * one `<label>__<corpus>.summary.json` per cited run — the aggregate metrics
    `score.py` computes, plus the strata. Kilobytes.
  * one `<comparison>.stats.json` per published Δ — the paired randomization
    p-value and the BCa interval `stats.py` computes. Kilobytes.
  * `qrels.manifest.json` — for every query set: sha256, instance count, gold
    files, and the strata sizes. This is the freeze record §5.6 asks for. The
    qrels themselves are third-party gold reshaped by `build_*_qrels.py`, and a
    hash is what lets a reader confirm that the set they rebuilt is the set the
    numbers came from without either party shipping it.

What is deliberately NOT committed: the per-query rows. They are the bulk, they
are regenerable from the run, and a reader who wants them wants to re-run
anyway. If you need to re-derive a Δ from scratch you need `results/`, and the
right move then is to re-run the arm, not to trust an archived ranking.

Usage:

    bench/.venv/bin/python bench/publish.py            # regenerate everything
    bench/.venv/bin/python bench/publish.py --check    # fail if stale (CI)

`--check` is what makes this file more than a convenience: it recomputes into a
temporary directory and diffs, so a published number that no longer follows from
the artefact behind it is a red test rather than a discrepancy someone notices
in a year.
"""

from __future__ import annotations

import argparse
import filecmp
import hashlib
import json
import subprocess
import sys
import tempfile
from pathlib import Path

BENCH = Path(__file__).resolve().parent
RESULTS = BENCH / "results"
QRELS = BENCH / ".data" / "qrels"
PUBLISHED = BENCH / "published"

# Every run whose aggregate is quoted in PROTOCOL §12 or FINDINGS. Adding a
# number to either document means adding its run here; the `--check` mode turns
# the omission into a failure rather than a citation with nothing behind it.
CITED_RUNS = [
    # §12.15 / FINDINGS §11.1 — the shipped systems against each other.
    "v3-qwen06b-torch__django-docs-short",
    "F2-full__django-docs-short",
    # §12.10 — the chunk window.
    "F2-full__django-364-docs-short",
    "slicer256__django-docs-short",
    # §12.7 — what each stage of the v2 pipeline contributed.
    "F2-dense-only__django-docs-short",
    "F2-no-colbert__django-docs-short",
    "F2-sparse-only__django-docs-short",
    "F2-weighted-sum__django-docs-short",
    "F2-full__scikit-learn-docs-short",
    "F2-dense-only__scikit-learn-docs-short",
    # §12.6 — the lexical floor and the calibration arm.
    "bm25-unicode61__django-docs-short",
    "bm25-unicode61__scikit-learn-docs",
    "random__django-docs",
    # §12.12 — the model comparison (offline exact-cosine arms).
    "bgem3-exact__django-docs-short",
    "CodeRankEmbed__django-docs-short",
    "granite-embedding-english-r2__django-docs-short",
    "Qwen3-Embedding-0.6B__django-docs-short",
    "granite-embedding-english-r2__scikit-learn-docs-short",
    "Qwen3-Embedding-0.6B__scikit-learn-docs-short",
    # §12.14 — F10's arms on scikit-learn.
    "v3-ident__scikit-learn-ident",
    "bm25-unicode61__scikit-learn-ident",
    "bm25-trigram__scikit-learn-ident",
    "symbols__scikit-learn-ident",
    "random__scikit-learn-ident",
]

# Every Δ published with an interval. (system, baseline, output name).
CITED_COMPARISONS = [
    (
        "v3-qwen06b-torch__django-docs-short",
        "F2-full__django-docs-short",
        "v3-vs-v2__django-docs-short",
    ),
    (
        "F2-full__django-364-docs-short",
        "F2-full__django-docs-short",
        "F3-364-vs-512__django-docs-short",
    ),
]

DELTA = 0.01


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def qrels_manifest() -> dict[str, object]:
    """Hash and describe every query set, so a rebuild can be checked against it.

    The counts are recomputed rather than copied from PROTOCOL: a manifest that
    restates the document it is meant to corroborate corroborates nothing.
    """
    entries: dict[str, object] = {}
    for path in sorted(QRELS.glob("*.jsonl")):
        rows = [json.loads(line) for line in path.open()]
        strata: dict[str, int] = {}
        for row in rows:
            key = str(row.get("overlap_bucket", "unbucketed"))
            strata[key] = strata.get(key, 0) + 1
        entries[path.name] = {
            "sha256": sha256(path),
            "bytes": path.stat().st_size,
            "instances": len(rows),
            "gold_files": sum(len(r.get("gold_files", [])) for r in rows),
            "strata": dict(sorted(strata.items())),
        }
    return entries


def run(cmd: list[str]) -> None:
    result = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        sys.stderr.write(result.stdout + result.stderr)
        raise SystemExit(f"failed: {' '.join(cmd)}")


def generate(into: Path) -> list[str]:
    """Write every artefact into `into`. Returns the names of anything skipped."""
    into.mkdir(parents=True, exist_ok=True)
    python = sys.executable
    missing: list[str] = []

    for label in CITED_RUNS:
        source = RESULTS / f"{label}.jsonl"
        if not source.exists():
            missing.append(f"{label}.jsonl")
            continue
        run(
            [
                python,
                str(BENCH / "score.py"),
                str(source),
                "--json",
                str(into / f"{label}.summary.json"),
            ]
        )

    for system, baseline, name in CITED_COMPARISONS:
        a, b = RESULTS / f"{system}.jsonl", RESULTS / f"{baseline}.jsonl"
        if not (a.exists() and b.exists()):
            missing.append(f"{name}.stats.json")
            continue
        run(
            [
                python,
                str(BENCH / "stats.py"),
                str(a),
                str(b),
                "--delta",
                str(DELTA),
                "--json",
                str(into / f"{name}.stats.json"),
            ]
        )

    if QRELS.is_dir():
        (into / "qrels.manifest.json").write_text(
            json.dumps(qrels_manifest(), indent=2) + "\n"
        )
    else:
        missing.append("qrels.manifest.json")

    return missing


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--check",
        action="store_true",
        help="regenerate into a temporary directory and fail on any difference",
    )
    args = ap.parse_args()

    if not args.check:
        missing = generate(PUBLISHED)
        if missing:
            print(f"skipped {len(missing)} artefact(s) whose input is absent:")
            for name in missing:
                print(f"  {name}")
            print("re-run the arm, or drop it from CITED_RUNS with a reason")
        print(f"-> {PUBLISHED}")
        return 0

    with tempfile.TemporaryDirectory() as tmp:
        fresh = Path(tmp)
        missing = generate(fresh)
        published = {p.name for p in PUBLISHED.glob("*.json")}
        regenerated = {p.name for p in fresh.glob("*.json")}

        stale = []
        for name in sorted(regenerated & published):
            if not filecmp.cmp(fresh / name, PUBLISHED / name, shallow=False):
                stale.append(name)
        absent = sorted(regenerated - published)
        orphan = sorted(published - regenerated - set(missing))

        for name in stale:
            print(f"STALE     {name}  (the run no longer produces this)")
        for name in absent:
            print(f"UNCOMMITTED {name}")
        for name in orphan:
            print(f"ORPHAN    {name}  (nothing in CITED_* produces it)")
        if missing:
            print(f"note: {len(missing)} input(s) absent locally, not checked")
        if stale or absent or orphan:
            print("\nrun `bench/publish.py` and commit the result")
            return 1
        print("published artefacts match the runs they came from")
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
