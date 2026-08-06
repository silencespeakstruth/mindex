#!/usr/bin/env python3
"""Turn published datasets into the frozen query set a benchmark run scores.

Output is one JSONL per corpus under `<data_dir>/qrels/`, plus a report of the
counts every filter dropped. PROTOCOL.md §3.2 and §3.3 define the rules; this
file implements them and nothing else. In particular it decides nothing about
relevance — a gold file is a file the published fix patch modified.

Three filters do real work here, and each one is a way the benchmark could
have been quietly wrong:

  * **Added files are not gold.** A patch that creates `foo/bar.py` names a
    path that does not exist at `base_commit`. No retriever can return it, so
    counting it as relevant would depress every system's recall by a constant
    nobody could explain.
  * **Test files are not gold.** A test is where a bug is demonstrated, not
    where it lives; a system that ranks the test first has localized nothing.
  * **Gold paths must exist at the snapshot.** Verified against the clone with
    `git cat-file`, which also catches renames the patch header hides.

The leakage stratum of §9.2 is computed here too, not to drop anything, but so
that results can be reported separately for instances whose problem statement
already spells a gold path.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections import Counter
from collections.abc import Iterator
from dataclasses import asdict, dataclass, field
from pathlib import Path, PurePosixPath
from typing import Any

import tomllib

# `diff --git a/<old> b/<new>` — the only header that survives renames intact.
DIFF_GIT = re.compile(r"^diff --git a/(?P<old>.+?) b/(?P<new>.+)$")
OLD_FILE = re.compile(r"^--- (?:a/)?(?P<path>.+)$")
NEW_FILE = re.compile(r"^\+\+\+ (?:b/)?(?P<path>.+)$")
DEV_NULL = "/dev/null"


@dataclass
class Instance:
    """One scored query. Field names are the results-JSONL contract."""

    instance_id: str
    corpus: str
    # Every dataset that contains this fix. SWE-bench Verified is a
    # human-validated subset of SWE-bench full, so the same instance is in
    # both: running the query twice would waste a reindex and double-count it
    # in any pooled figure, while dropping one would make "Verified nDCG" —
    # the number comparable to published work — unreportable. So the WORK is
    # deduplicated and the LABELS are not.
    datasets: list[str]
    repo: str
    base_commit: str
    query: str
    gold_files: list[str]
    gold_functions: list[str] = field(default_factory=list)
    category: str | None = None
    # PROTOCOL.md §9.2, two strengths. A traceback pasted into an issue names
    # the full path; prose ("the bug is in literal.rs") names only the file.
    # The weaker signal is the commoner one, so both are recorded and results
    # are stratified on them.
    leaks_gold_path: bool = False
    leaks_gold_basename: bool = False
    n_gold: int = 0
    # Descriptive tier only (build_docs_qrels.py). `lexical_overlap` is the
    # share of the query's content words already present in the gold files'
    # identifiers and paths — the axis separating the queries a lexical matcher
    # wins by default from the ones semantic retrieval exists for.
    doc_path: str | None = None
    lexical_overlap: float | None = None
    overlap_bucket: str | None = None


@dataclass
class Drops:
    """Why instances did not survive. Published, never absorbed."""

    total: int = 0
    no_query: int = 0
    no_patch: int = 0
    gold_all_added: int = 0
    gold_all_tests: int = 0
    gold_all_records: int = 0
    gold_missing_at_snapshot: int = 0
    unknown_commit: int = 0
    query_over_vector_limit: int = 0
    # Not a drop: the fix was already collected from an earlier dataset, so
    # this row only added a label. Counted separately so a reader can tell
    # "SWE-bench Verified contributed nothing new" from "it was rejected".
    merged: int = 0
    kept: int = 0


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def load_config(path: Path) -> dict[str, Any]:
    with path.open("rb") as fh:
        return tomllib.load(fh)


# --------------------------------------------------------------------------
# Patch parsing
# --------------------------------------------------------------------------


def patch_paths(patch: str) -> tuple[set[str], set[str]]:
    """Return (paths that exist at base, paths the patch creates).

    Read from the `---`/`+++` pair rather than the `diff --git` line, because
    only that pair distinguishes a created file (`--- /dev/null`) from a
    modified one. The `a/` side is the path as it exists at `base_commit`,
    which is the only spelling a retriever could ever return.
    """
    existing: set[str] = set()
    created: set[str] = set()

    old: str | None = None
    for line in patch.splitlines():
        if line.startswith("--- "):
            m = OLD_FILE.match(line)
            old = m.group("path").strip() if m else None
        elif line.startswith("+++ "):
            m = NEW_FILE.match(line)
            new = m.group("path").strip() if m else None
            if old is None:
                continue
            if old == DEV_NULL:
                if new and new != DEV_NULL:
                    created.add(new)
            else:
                existing.add(old)
            old = None

    return existing, created


def path_matches(path: str, patterns: list[str]) -> bool:
    """Glob a repo-relative path, correctly, including at the tree root.

    `fnmatch` is the obvious tool and it is wrong here: it is not path-aware,
    so `**/` compiles to something demanding a literal `/` and a pattern like
    `**/tests/**` does not match `tests/foo.py`. django keeps its test suite in
    a root-level `tests/`, so the naive version silently admitted test files
    into the gold set of the single largest corpus. `PurePosixPath.full_match`
    (Python 3.13+) treats `**` as zero-or-more segments, which is what every
    pattern here means. Pinned by `--self-test`.
    """
    p = PurePosixPath(path)
    return any(p.full_match(pat) for pat in patterns)


# --------------------------------------------------------------------------
# Snapshot verification
# --------------------------------------------------------------------------


class GitRepo:
    """Thin `git cat-file` wrapper; verifies gold paths exist at a commit."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.available = (path / ".git").exists() or path.is_dir()
        self._commit_cache: dict[str, bool] = {}

    def has_commit(self, sha: str) -> bool:
        if sha not in self._commit_cache:
            proc = subprocess.run(
                ["git", "cat-file", "-e", f"{sha}^{{commit}}"],
                cwd=self.path,
                capture_output=True,
                check=False,
            )
            self._commit_cache[sha] = proc.returncode == 0
        return self._commit_cache[sha]

    def existing_paths(self, sha: str, paths: list[str]) -> set[str]:
        """Subset of `paths` present in the tree at `sha`."""
        if not paths:
            return set()
        # One `cat-file --batch-check` beats one process per path: django's
        # 850 instances would otherwise be tens of thousands of forks.
        stdin = "\n".join(f"{sha}:{p}" for p in paths) + "\n"
        proc = subprocess.run(
            ["git", "cat-file", "--batch-check=%(objectname) %(objecttype)"],
            cwd=self.path,
            input=stdin,
            capture_output=True,
            text=True,
            check=False,
        )
        present: set[str] = set()
        for path, line in zip(paths, proc.stdout.splitlines()):
            if not line.endswith(("missing", "ambiguous")) and " blob" in line:
                present.add(path)
        return present


# --------------------------------------------------------------------------
# Dataset readers
# --------------------------------------------------------------------------


def read_parquet(path: Path) -> list[dict[str, Any]]:
    # Imported here, not at module scope, so --self-test runs under a bare
    # interpreter. `type: ignore` follows the repo's `fastapi` precedent: the
    # stubs exist only inside bench/.venv, and the lint matrix runs mypy from
    # the system interpreter.
    import pyarrow.parquet as pq  # type: ignore[import-not-found,import-untyped]

    return pq.read_table(path).to_pylist()


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    with path.open() as fh:
        return [json.loads(line) for line in fh if line.strip()]


def swebench_records(rows: list[dict[str, Any]], repo: str) -> Iterator[dict[str, Any]]:
    """SWE-bench and Loc-Bench share a schema."""
    for row in rows:
        if row.get("repo") != repo:
            continue
        yield {
            "instance_id": row["instance_id"],
            "base_commit": row["base_commit"],
            "query": (row.get("problem_statement") or "").strip(),
            "patch": row.get("patch") or "",
            "category": row.get("category"),
            "gold_functions": list(row.get("edit_functions") or []),
        }


def multi_swebench_records(
    rows: list[dict[str, Any]], repo: str
) -> Iterator[dict[str, Any]]:
    """Multi-SWE-bench: the query is the linked ISSUE, not the pull request.

    `title`/`body` on the record belong to the PR and describe the fix — one
    ripgrep PR is titled "replace clap with lexopt". Using them would measure
    how well a retriever copies an answer it was handed.
    """
    for row in rows:
        org = row.get("org", "")
        name = row.get("repo", "")
        if f"{org}/{name}" != repo:
            continue
        issues = row.get("resolved_issues") or []
        query = "\n\n".join(
            f"{(i.get('title') or '').strip()}\n\n{(i.get('body') or '').strip()}".strip()
            for i in issues
        ).strip()
        yield {
            "instance_id": f"{org}__{name}-{row['number']}",
            "base_commit": (row.get("base") or {}).get("sha", ""),
            "query": query,
            "patch": row.get("fix_patch") or "",
            "category": None,
            "gold_functions": [],
        }


def dataset_path(
    data_dir: Path, name: str, spec: dict[str, Any], corpus: dict[str, Any]
) -> Path:
    base = data_dir / "datasets" / name
    if spec["kind"] == "swebench_like":
        return base / f"{spec['split']}-00000-of-00001.parquet"
    org, repo = corpus["repo"].split("/", 1)
    fname = spec["path_template"].format(lang=corpus["multi_lang"], org=org, name=repo)
    return base / Path(fname).name


# --------------------------------------------------------------------------
# Build
# --------------------------------------------------------------------------


class QueryTokenizer:
    """Counts a query in the units the vector store actually constrains.

    ColBERT emits one `VECTOR_DIM`-wide row per token and a Qdrant multivector
    may hold at most 1 048 576 elements, so a query above 1024 BGE-M3 tokens
    cannot be scored — the request fails with 503 `qdrant.unavailable`
    regardless of how far above it sits, because the embedder's own `--maxlen`
    truncation still leaves more rows than the store accepts. The bound is
    Qdrant's representation, not a mindex setting: `STORABLE_TOKENS_CEILING`
    derives from the same constant and is documented "structural, not
    configurable, like VECTOR_DIM".

    Counted with the real tokenizer rather than estimated from bytes, because a
    ratio that is right on prose is wrong on a stack trace, and both appear in
    the same issue report.
    """

    def __init__(self, model: str) -> None:
        # Imported here rather than at module scope so fetch.py and the
        # --self-test path do not pay for it.
        from tokenizers import Tokenizer  # type: ignore[import-untyped]

        self.tokenizer = Tokenizer.from_pretrained(model)

    def count(self, text: str) -> int:
        return len(self.tokenizer.encode(text).ids)


def build_corpus(
    corpus: dict[str, Any],
    config: dict[str, Any],
    data_dir: Path,
    clone_dir: Path,
    *,
    verify_snapshots: bool,
    tokenizer: QueryTokenizer | None,
) -> tuple[list[Instance], dict[str, Drops], set[str]]:
    exclude = config["qrels"]["exclude_test_paths"]
    exclude_records = config["qrels"]["exclude_record_paths"]
    token_limit = int(config["qrels"]["max_query_tokens"])
    git = GitRepo(clone_dir / corpus["name"])
    if verify_snapshots and not git.available:
        raise SystemExit(
            f"{corpus['name']}: no clone at {git.path} — run fetch.py, "
            f"or pass --no-verify-snapshots to build an unverified draft"
        )

    # Keyed by (base_commit, gold set) so the same fix arriving from a second
    # dataset merges instead of duplicating. Insertion order is preserved and
    # the final list is re-sorted chronologically before it is written.
    by_key: dict[str, Instance] = {}
    reports: dict[str, Drops] = {}
    over_limit: set[str] = set()

    for ds_name in corpus["datasets"]:
        spec = config["datasets"][ds_name]
        path = dataset_path(data_dir, ds_name, spec, corpus)
        if not path.exists():
            raise SystemExit(f"missing {path} — run fetch.py first")

        rows = read_parquet(path) if path.suffix == ".parquet" else read_jsonl(path)
        reader = (
            swebench_records
            if spec["kind"] == "swebench_like"
            else multi_swebench_records
        )
        drops = Drops()

        for rec in reader(rows, corpus["repo"]):
            drops.total += 1

            if not rec["query"]:
                drops.no_query += 1
                continue
            if not rec["patch"]:
                drops.no_patch += 1
                continue

            # Dropped here rather than scored as a miss later, and the choice is
            # load-bearing for the comparisons this corpus exists to support:
            # the BM25/FTS5 floor of F1 accepts a query of any length, so
            # scoring mindex zero on queries no vector store could hold would
            # charge it a penalty that is not about retrieval quality. The
            # exclusion is declared, counted, and applies identically to every
            # system and baseline. Its cost is stated in PROTOCOL §4.2: these
            # are not a random 8%, they are the longest problem statements, so
            # the corpus after this describes queries under the limit.
            if tokenizer is not None and tokenizer.count(rec["query"]) > token_limit:
                drops.query_over_vector_limit += 1
                # Also tracked as a set, because the per-dataset counters sum to
                # more than the corpus loses: SWE-bench Verified is a subset of
                # full, so one excluded fix is counted by both. The same trap
                # the `merged` counter exists for.
                over_limit.add(f"{rec['base_commit']}::{rec['instance_id']}")
                continue

            existing, created = patch_paths(rec["patch"])
            if not existing:
                # Two different failures that must not be pooled: a patch that
                # only creates files (nothing to retrieve), and a patch this
                # parser could not read at all (a harness bug worth seeing).
                if created:
                    drops.gold_all_added += 1
                else:
                    drops.no_patch += 1
                continue

            gold = sorted(p for p in existing if not path_matches(p, exclude))
            if not gold:
                drops.gold_all_tests += 1
                continue

            gold = sorted(p for p in gold if not path_matches(p, exclude_records))
            if not gold:
                drops.gold_all_records += 1
                continue

            if verify_snapshots:
                if not git.has_commit(rec["base_commit"]):
                    drops.unknown_commit += 1
                    continue
                present = git.existing_paths(rec["base_commit"], gold)
                if len(present) != len(gold):
                    # A gold path absent at the snapshot means the patch header
                    # and the tree disagree — usually a rename. Dropping is the
                    # honest move: we cannot say what the retriever should have
                    # returned.
                    drops.gold_missing_at_snapshot += 1
                    continue

            key = f"{rec['base_commit']}::{','.join(gold)}"
            if key in by_key:
                # Same fix, already collected from another dataset. Attribute
                # it and move on: the query runs once, both labels survive.
                existing_inst = by_key[key]
                if ds_name not in existing_inst.datasets:
                    existing_inst.datasets.append(ds_name)
                    # Loc-Bench is the only source of a category or of
                    # function-level gold; take them wherever they appear.
                    existing_inst.category = existing_inst.category or rec["category"]
                    if rec["gold_functions"] and not existing_inst.gold_functions:
                        existing_inst.gold_functions = rec["gold_functions"]
                drops.merged += 1
                continue

            query = rec["query"]
            basenames = {p.rsplit("/", 1)[-1] for p in gold}
            by_key[key] = Instance(
                instance_id=rec["instance_id"],
                corpus=corpus["name"],
                datasets=[ds_name],
                repo=corpus["repo"],
                base_commit=rec["base_commit"],
                query=query,
                gold_files=gold,
                gold_functions=rec["gold_functions"],
                category=rec["category"],
                leaks_gold_path=any(p in query for p in gold),
                leaks_gold_basename=any(b in query for b in basenames),
                n_gold=len(gold),
            )
            drops.kept += 1

        reports[ds_name] = drops

    return list(by_key.values()), reports, over_limit


def print_report(
    name: str,
    reports: dict[str, Drops],
    instances: list[Instance],
    over_limit: set[str],
) -> None:
    print(f"\n=== {name} ===")
    # `usable` is what the dataset contributes to scoring — new instances plus
    # ones an earlier dataset already collected. `new` is how many queries it
    # added to the run. They differ precisely where one dataset subsets another.
    per_ds = Counter(ds for i in instances for ds in i.datasets)
    hdr = f"{'dataset':22s} {'total':>6s} {'usable':>7s} {'new':>6s} {'ret%':>6s}  dropped"
    print(hdr)
    print("-" * len(hdr))
    for ds, d in reports.items():
        usable = per_ds.get(ds, 0)
        pct = 100.0 * usable / d.total if d.total else 0.0
        why = ", ".join(
            f"{k}={v}"
            for k, v in (
                ("no_query", d.no_query),
                ("no_patch", d.no_patch),
                ("added_only", d.gold_all_added),
                ("tests_only", d.gold_all_tests),
                ("records_only", d.gold_all_records),
                ("missing@snap", d.gold_missing_at_snapshot),
                ("unknown_commit", d.unknown_commit),
                ("query_too_long", d.query_over_vector_limit),
            )
            if v
        )
        print(
            f"{ds:22s} {d.total:6d} {usable:7d} {d.kept:6d} {pct:5.1f}%  {why or '-'}"
        )

    over = len(over_limit)
    if over:
        # Printed on its own line rather than left in the drop list, because it
        # is the one exclusion that removes a systematic slice of the corpus —
        # the longest problem statements — instead of removing rows that could
        # not be scored at all.
        print(
            f"  {over} instance(s) excluded: the query exceeds what a Qdrant "
            f"multivector can hold, so no configuration can be scored on them. "
            f"They are the LONGEST problem statements, not a random sample; "
            f"every number from this corpus describes queries under that limit."
        )

    if not instances:
        return

    n = len(instances)
    leak_path = sum(1 for i in instances if i.leaks_gold_path)
    leak_base = sum(1 for i in instances if i.leaks_gold_basename)
    sizes = Counter(min(i.n_gold, 5) for i in instances)
    print(
        f"\n  instances kept: {n}"
        f"  |  leaks full path: {leak_path} ({100.0 * leak_path / n:.1f}%)"
        f"  |  leaks basename: {leak_base} ({100.0 * leak_base / n:.1f}%)"
    )
    print(
        "  gold-set size: "
        + ", ".join(f"{'5+' if k == 5 else k}:{sizes[k]}" for k in sorted(sizes))
    )
    cats = Counter(i.category for i in instances if i.category)
    if cats:
        print("  categories: " + ", ".join(f"{k}:{v}" for k, v in cats.most_common()))


def self_test() -> int:
    """Pin the filters. Every case here is one the naive version got wrong.

    `fnmatch` admitted a root-level `tests/` — django's whole test suite —
    into gold sets, silently and only for the largest corpus. That is the
    class of defect this benchmark exists to avoid, so it is checked rather
    than remembered.
    """
    tests = ["**/tests/**", "**/test_*.py", "**/*_test.go", "**/*.spec.ts"]
    records = ["**/CHANGELOG*", "**/RELEASE*", "**/AUTHORS*"]
    cases: list[tuple[str, list[str], bool]] = [
        ("tests/model_fields/test_json.py", tests, True),  # django, root-level
        ("django/db/models/query.py", tests, False),
        ("test_x.py", tests, True),
        ("src/main_test.go", tests, True),
        ("packages/x/foo.spec.ts", tests, True),
        ("crates/core/args.rs", tests, False),
        ("CHANGELOG.md", records, True),  # root-level, fnmatch missed it
        ("RELEASE-CHECKLIST.md", records, True),
        ("doc/rg.1.md", records, False),  # a man page IS part of the change
        ("crates/core/args.rs", records, False),
    ]
    failures = 0
    for path, patterns, expected in cases:
        got = path_matches(path, patterns)
        status = "ok  " if got == expected else "FAIL"
        if got != expected:
            failures += 1
        print(f"  {status} {path:34s} -> {got} (expected {expected})")

    # The diff parser must tell a created file from a modified one: a created
    # path does not exist at base_commit and can never be retrieved.
    patch = (
        "diff --git a/old.py b/old.py\n--- a/old.py\n+++ b/old.py\n"
        "diff --git a/new.py b/new.py\n--- /dev/null\n+++ b/new.py\n"
        "diff --git a/gone.py b/gone.py\n--- a/gone.py\n+++ /dev/null\n"
    )
    existing, created = patch_paths(patch)
    ok = existing == {"old.py", "gone.py"} and created == {"new.py"}
    print(
        f"  {'ok  ' if ok else 'FAIL'} patch_paths: existing={sorted(existing)} created={sorted(created)}"
    )
    failures += 0 if ok else 1

    print("\nself-test:", "PASS" if failures == 0 else f"{failures} FAILURE(S)")
    return 1 if failures else 0


def main() -> int:
    root = repo_root()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=root / "bench" / "corpora.toml")
    parser.add_argument("--corpus", action="append", dest="corpora")
    parser.add_argument("--tier", type=int, default=1)
    parser.add_argument(
        "--no-verify-snapshots",
        action="store_true",
        help="skip the git existence check (draft only — never for a real run)",
    )
    parser.add_argument(
        "--self-test", action="store_true", help="check the filters and exit"
    )
    parser.add_argument(
        "--keep-over-limit-queries",
        action="store_true",
        help="keep queries the vector store cannot hold (they will be refused)",
    )
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    config = load_config(args.config)
    data_dir = root / config["run"]["data_dir"]
    clone_dir = root / config["run"]["clone_dir"]
    out_dir = data_dir / "qrels"
    out_dir.mkdir(parents=True, exist_ok=True)

    corpora = [c for c in config["corpus"] if c["tier"] <= args.tier]
    if args.corpora:
        by_name = {c["name"]: c for c in config["corpus"]}
        missing = [n for n in args.corpora if n not in by_name]
        if missing:
            raise SystemExit(f"unknown corpus name(s): {', '.join(missing)}")
        corpora = [by_name[n] for n in args.corpora]

    tokenizer: QueryTokenizer | None = None
    if args.keep_over_limit_queries:
        print(
            "WARNING: --keep-over-limit-queries. Queries above the vector-store "
            "limit will be run and refused, and every metric will carry those "
            "zeros. This is not the pre-registered corpus."
        )
    else:
        tokenizer = QueryTokenizer(config["qrels"]["tokenizer_model"])

    grand_total = 0
    for corpus in corpora:
        instances, reports, over_limit = build_corpus(
            corpus,
            config,
            data_dir,
            clone_dir,
            verify_snapshots=not args.no_verify_snapshots,
            tokenizer=tokenizer,
        )
        print_report(corpus["name"], reports, instances, over_limit)

        out = out_dir / f"{corpus['name']}.jsonl"
        with out.open("w") as fh:
            # A stable order, so two builds of the same corpus are diffable. It
            # is NOT the run order: what makes the incremental reindex cheap is
            # commit *time*, which is not recoverable from a SHA, so run.py
            # reads it from the clone and sorts there.
            for inst in sorted(instances, key=lambda i: (i.base_commit, i.instance_id)):
                fh.write(json.dumps(asdict(inst), sort_keys=True) + "\n")
        print(f"  -> {out.relative_to(root)}")
        grand_total += len(instances)

    print(f"\ntotal instances: {grand_total}")
    print(
        "\nThis query set is now FROZEN (PROTOCOL.md §5.6). Instances are never\n"
        "added or removed after a result has been seen."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
