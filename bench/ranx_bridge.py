#!/usr/bin/env python3
"""The one place a mindex result file becomes a `ranx` Qrels/Run pair.

WHY THIS FILE EXISTS AT ALL. `score.py` and `stats.py` grew their own nDCG,
their own bootstrap and their own permutation test, and every one of them is a
place a statistical bug can hide while producing a plausible number — which is
the failure mode this whole exercise exists to catch (`FINDINGS.md` §5.8: a
self-test that asserted an expectation rather than a property). `ranx` is the
maintained implementation of all three, Numba-JIT, with 25 fusion algorithms and
`optimize_fusion` on top. So the arithmetic moves there and this module keeps
only what is genuinely ours.

WHAT IS GENUINELY OURS, AND STAYS. Two conventions, both argued in `score.py`'s
docstring and neither expressible as a `ranx` option:

  RANKING IS OVER FILES, NOT CHUNKS. Ground truth names files; mindex returns
  chunks. A chunk credits its file at the rank of its FIRST occurrence and later
  chunks of an already-credited file are dropped. Counting them would make the
  metric depend on chunk size, which is a parameter under test.

  THE SCORES HANDED TO ranx ARE RANKS, NOT THE RETRIEVER'S SCORES. After the
  dedup a file's own score is meaningless (it is one chunk's score out of
  several), and two files can carry the same float from different legs. `ranx`
  sorts by score and its tie-breaking is not ours to assume, so this module
  emits strictly decreasing synthetic scores that reproduce the dedup order
  exactly. A run built any other way can silently reorder ties and disagree with
  `score.py` for a reason no reader would find.

WHAT ranx CANNOT DO, AND WHY IT IS NOT A DEFECT. `acc@k` (LocAgent's
all-gold-inside-top-k) is not an IR measure and has no `ranx` equivalent; it
stays in `score.py`, which is correct, because it is reported for comparability
with published numbers and is never gated on.
"""

from __future__ import annotations

import json
from collections.abc import Iterable
from pathlib import Path
from typing import Any


# A file at rank i gets this score. Strictly decreasing, so `ranx`'s sort
# reproduces the dedup order and no tie-break rule of its own can apply.
def _rank_score(i: int) -> float:
    return float(-i)


def ranked_files(results: Iterable[dict[str, Any]]) -> list[str]:
    """Chunk ranking to file ranking, first occurrence wins.

    Deliberately duplicated from `score.py` rather than imported: this is the
    convention the two implementations must AGREE on, and importing it would
    make the equivalence test assert a tautology.
    """
    seen: list[str] = []
    known: set[str] = set()
    for hit in results:
        path = hit["path"]
        if path not in known:
            known.add(path)
            seen.append(path)
    return seen


def load_records(path: Path) -> dict[str, dict[str, Any]]:
    """Result JSONL keyed by `instance_id`."""
    records: dict[str, dict[str, Any]] = {}
    with path.open() as fh:
        for line in fh:
            if line.strip():
                rec = json.loads(line)
                records[rec["instance_id"]] = rec
    return records


def to_qrels_run(
    records: dict[str, dict[str, Any]],
    instance_ids: Iterable[str] | None = None,
) -> tuple[dict[str, dict[str, int]], dict[str, dict[str, float]]]:
    """Plain dicts in `ranx`'s shape — Qrels/Run objects are built by the caller.

    Returning dicts rather than `ranx` objects keeps this importable without
    paying Numba's import cost, and lets the equivalence test compare the
    intermediate form.

    An instance whose gold set is empty is dropped from BOTH sides. `ranx`
    treats a query absent from the qrels as an error rather than as a zero,
    while `score.py` scores it 0.0; neither corpus contains one, and this makes
    that assumption explicit instead of latent.
    """
    ids = list(instance_ids) if instance_ids is not None else list(records)
    qrels: dict[str, dict[str, int]] = {}
    run: dict[str, dict[str, float]] = {}
    for qid in ids:
        rec = records[qid]
        gold = rec["gold_files"]
        if not gold:
            continue
        qrels[qid] = {path: 1 for path in gold}
        ranking = ranked_files(rec["results"])
        # A query whose ranking came back empty still has to appear, or the two
        # implementations disagree on the denominator rather than on the metric.
        run[qid] = {path: _rank_score(i) for i, path in enumerate(ranking)} or {
            "__empty__": 0.0
        }
    return qrels, run
