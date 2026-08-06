#!/usr/bin/env python3
"""Build the descriptive query set: a project's own docs, as questions about its code.

Reads the Sphinx tree of a corpus at one pinned commit and emits the same JSONL
`run.py` already consumes. The rules and the reasoning are in `docs.py`; this
file is the driver and the drop report.

ONE COMMIT, NOT ONE PER QUERY. The issue-localization tier needed a snapshot per
instance because a bug report has a "before" and an "after", and indexing the
"after" hands over the answer. A description of behaviour that already exists
has no such split: the documentation describes the code beside it, at the same
commit. So this tier indexes each corpus **once**, which is what makes the
ablation matrix affordable — django is one ~47 GiB index instead of 812 rebuilds.

THE DOCS TREE IS EXCLUDED FROM THE RANKING, NOT FROM THE INDEX. The query text
is lifted out of a documentation file that mindex indexes on purpose, so that
file would come back first by near-exact match — a tautology, not a result. It
is dropped with `/search`'s own `exclude: {paths: [...]}`, so the index stays
the deployed one, the ranking is over code, and the exclusion is declared and
identical for every system and baseline.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from dataclasses import asdict
from pathlib import Path
from typing import Any

from build_qrels import GitRepo, Instance, load_config, path_matches, repo_root
from sphinx_docs import (
    MAX_QUERY_CHARS,
    MIN_QUERY_CHARS,
    Resolution,
    SymbolIndex,
    code_refs,
    lexical_overlap,
    overlap_bucket,
    shorten,
    split_sections,
    strip_for_query,
)


def build_corpus(
    corpus: dict[str, Any],
    clone_dir: Path,
    *,
    verify_snapshot: bool,
    short: bool = False,
) -> tuple[list[Instance], Resolution, SymbolIndex]:
    name = corpus["name"]
    clone = clone_dir / name
    docs_dir = clone / corpus["docs_dir"]
    if not docs_dir.is_dir():
        raise SystemExit(f"{name}: no docs tree at {docs_dir}")

    commit = corpus["docs_commit"]
    git = GitRepo(clone)
    if verify_snapshot:
        if not git.has_commit(commit):
            raise SystemExit(f"{name}: commit {commit} not in the clone")
        head = git_head(clone)
        if head != commit:
            raise SystemExit(
                f"{name}: the clone is at {head[:10]}, not the pinned "
                f"{commit[:10]}. The docs and the code must be read at the same "
                f"commit as the index, or the gold set describes another tree."
            )

    index = SymbolIndex(clone, corpus["package"])
    report = Resolution()
    instances: list[Instance] = []
    seen_queries: set[str] = set()

    suffixes = tuple(corpus.get("docs_suffixes", [".txt", ".rst"]))
    excluded = corpus.get("docs_exclude", [])
    for doc in sorted(docs_dir.rglob("*")):
        if not doc.is_file() or doc.suffix not in suffixes:
            continue
        rel_doc = doc.relative_to(clone).as_posix()
        if path_matches(rel_doc, excluded):
            report.excluded_doc += 1
            continue
        text = doc.read_text(encoding="utf-8", errors="replace")
        for section in split_sections(text, rel_doc):
            report.sections_total += 1
            body = "\n".join(section.body_lines)

            gold: set[str] = set()
            for _kind, target in code_refs(body):
                report.refs_total += 1
                path, outcome = index.resolve(target, section.module_context)
                if path:
                    report.refs_resolved += 1
                    gold.add(path)
                elif outcome == "ambiguous":
                    report.ambiguous_symbol += 1
                elif outcome == "empty_module":
                    report.empty_module += 1
                elif outcome == "not_defined":
                    report.symbol_not_defined += 1
                else:
                    report.unknown_module += 1

            if not gold:
                report.dropped_no_gold += 1
                continue

            query = f"{section.heading}. {strip_for_query(body)}"[:MAX_QUERY_CHARS]
            if len(query) < MIN_QUERY_CHARS:
                report.dropped_short_query += 1
                continue
            if short:
                # The gold set is decided by the WHOLE section, exactly as
                # above, and only then is the question shortened. Deciding it
                # from the short text instead would change two things at once
                # and make the comparison uninterpretable.
                cut = shorten(query)
                if cut is None:
                    report.dropped_short_query += 1
                    continue
                query = cut
            if query in seen_queries:
                # Identical prose under two headings is one question.
                continue
            seen_queries.add(query)

            gold_files = sorted(gold)
            overlap = lexical_overlap(query, gold_files, index)
            basenames = {p.rsplit("/", 1)[-1] for p in gold_files}
            instances.append(
                Instance(
                    instance_id=f"{name}:{rel_doc}#{section.lineno}",
                    corpus=name,
                    datasets=["project_docs"],
                    repo=corpus["repo"],
                    base_commit=commit,
                    query=query,
                    gold_files=gold_files,
                    category=None,
                    leaks_gold_path=any(p in query for p in gold_files),
                    leaks_gold_basename=any(b in query for b in basenames),
                    n_gold=len(gold_files),
                    doc_path=rel_doc,
                    lexical_overlap=round(overlap, 4),
                    overlap_bucket=overlap_bucket(overlap),
                )
            )
            report.kept += 1

    return instances, report, index


def git_head(clone: Path) -> str:
    import subprocess

    return subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=clone,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()


def print_report(name: str, report: Resolution, instances: list[Instance]) -> None:
    print(f"\n=== {name} ===")
    print(f"  sections read      {report.sections_total}")
    print(
        f"  references         {report.refs_total} "
        f"({report.refs_resolved} resolved, "
        f"{report.unknown_module} unknown module, "
        f"{report.ambiguous_symbol} ambiguous, "
        f"{report.empty_module} re-export shim)"
    )
    print(
        f"  doc files excluded {report.excluded_doc} "
        f"(release notes and project process describe change, not code)"
    )
    print(
        f"  sections dropped   {report.dropped_no_gold} no gold, "
        f"{report.dropped_short_query} query too short"
    )
    print(f"  instances kept     {report.kept}")
    if not instances:
        return

    n = len(instances)
    buckets = Counter(i.overlap_bucket for i in instances)
    print("\n  lexical overlap between query and gold identifiers:")
    for key in ("obvious", "mixed", "non-obvious"):
        c = buckets.get(key, 0)
        print(f"    {key:<12} {c:5d}  ({100.0 * c / n:4.1f}%)")

    sizes = Counter(min(i.n_gold, 5) for i in instances)
    print(
        "\n  gold-set size: "
        + ", ".join(f"{'5+' if k == 5 else k}:{sizes[k]}" for k in sorted(sizes))
    )
    leak_path = sum(1 for i in instances if i.leaks_gold_path)
    leak_base = sum(1 for i in instances if i.leaks_gold_basename)
    print(
        f"  leaks full path: {leak_path} ({100.0 * leak_path / n:.1f}%)  "
        f"leaks basename: {leak_base} ({100.0 * leak_base / n:.1f}%)"
    )
    lengths = sorted(len(i.query) for i in instances)
    print(
        f"  query length: median {lengths[n // 2]} chars, "
        f"max {lengths[-1]} (cap {MAX_QUERY_CHARS})"
    )


def audit(instances: list[Instance], count: int, seed: int) -> None:
    """Print a random sample for a human to read before anything is measured.

    PROTOCOL §10 requires this and it is not a formality: the three defects
    found in the first corpus were all found by reading instances, and none of
    them would have failed a test.
    """
    import random

    rng = random.Random(seed)
    sample = rng.sample(instances, min(count, len(instances)))
    for inst in sample:
        print("\n" + "─" * 76)
        print(f"{inst.instance_id}  [{inst.overlap_bucket}, {inst.lexical_overlap}]")
        print(f"GOLD: {inst.gold_files}")
        print(f"QUERY: {inst.query[:600]}")


def main() -> int:
    root = repo_root()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=root / "bench" / "corpora.toml")
    parser.add_argument("--corpus", action="append", dest="corpora")
    parser.add_argument(
        "--audit", type=int, default=0, help="print N sampled instances"
    )
    parser.add_argument("--audit-seed", type=int, default=0)
    parser.add_argument("--no-verify-snapshot", action="store_true")
    parser.add_argument(
        "--short",
        action="store_true",
        help="emit the SHORT query variant (`-docs-short`): the same sections "
        "and the same gold, cut to a question the size a caller actually types",
    )
    args = parser.parse_args()

    config = load_config(args.config)
    clone_dir = root / config["run"]["clone_dir"]
    out_dir = root / config["run"]["data_dir"] / "qrels"
    out_dir.mkdir(parents=True, exist_ok=True)

    available = {c["name"]: c for c in config["corpus"] if c.get("docs_dir")}
    names = args.corpora or sorted(available)
    missing = [n for n in names if n not in available]
    if missing:
        raise SystemExit(
            f"no docs configuration for: {', '.join(missing)} "
            f"(have: {', '.join(sorted(available))})"
        )

    total = 0
    for name in names:
        instances, report, _ = build_corpus(
            available[name],
            clone_dir,
            verify_snapshot=not args.no_verify_snapshot,
            short=args.short,
        )
        print_report(name, report, instances)
        if args.audit:
            audit(instances, args.audit, args.audit_seed)

        out = out_dir / f"{name}-docs{'-short' if args.short else ''}.jsonl"
        with out.open("w") as fh:
            for inst in sorted(instances, key=lambda i: i.instance_id):
                fh.write(json.dumps(asdict(inst), sort_keys=True) + "\n")
        print(f"\n  -> {out.relative_to(root)}")
        total += len(instances)

    print(f"\ntotal instances: {total}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
