#!/usr/bin/env python3
"""PROTOCOL §3.4: the identifier query set, as a projection of the issue tier.

WHAT THIS IS NOT. It is not a new ground truth. Every gold set here is copied
byte-for-byte out of the frozen issue-tier qrels — `gold_files`,
`gold_functions`, `base_commit`, `repo`, `datasets` — and exactly one field is
rewritten: `query`. That copy is asserted per instance and a mismatch raises.
It is the whole argument for taking this corpus seriously: a query set nobody
published carries weight only while it inherits provenance from one that was.

WHY IT EXISTS. §12.12 closed the second-leg question — a sparse/lexical leg
stopped paying once the dense leg was modern, PASS TOST at δ=0.01 in both
directions — and named its own limit in the same breath: both corpora are
Python and both query sets are documentation prose. CoIR measures BM25 varying
56× across its datasets. If a lexical leg is worth anything to `/search`, it is
worth it to a caller who types a name, and nothing here has ever asked one.

WHY THE ISSUE TIER AND NOT THE DESCRIPTIVE ONE. This inverts §1's own argument
and the inversion is the point. The descriptive tier's gold *is* the file
defining the symbol a section references, so an identifier query over it would
measure that exact strings match exact strings — the tautology this family has
to avoid being. The issue tier's gold is the files a fix patch touched, and the
identifiers a bug report names are symptom-side: the API the reporter called,
not the module that had to change.

THE HAZARD. The extractor and the mangler decide what every query in this
corpus *says*. A bug in either produces a plausible corpus rather than an
error — the exact failure mode that let three defects survive in the first
corpus until somebody read the instances (§11). So every rule below is pinned
by `--self-test`, and `--audit N` prints instances for a human before anything
is measured. Neither is a formality; the tests came second, after the reading.
"""

from __future__ import annotations

import argparse
import json
import random
import re
import subprocess
import sys
from collections import Counter
from dataclasses import asdict, replace
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from build_qrels import GitRepo, Instance, load_config, repo_root
from sphinx_docs import overlap_bucket, words

# The arms of §3.4. `prose` is the unmodified problem statement and is a real
# arm, not a control: fusion has never been measured on the issue tier at all,
# so it is the first honest reference this family has.
PROJECTIONS = ("prose", "ident", "ident-mangled", "ident-intent")

# A fenced block is lifted whole and then tokenized, so it must come out before
# free-text scanning or its contents would be scanned twice under two rules.
#
# The two marks are NOT equivalent, and treating them as one was the first
# defect the audit caught. An inline backtick is an author writing "this token
# is a name". A fence in a bug report is a dump: version banners, a `--version`
# block, a diff, a console transcript, and in one measured ripgrep instance an
# entire DNA sequence — all of which entered arm A1 and turned an identifier
# query into a bag of every word in the report. So a fence gets the same shape
# requirement as free prose; only inline backticks are exempt. What survives in
# a fence is what should: `_fetch_all` and `django/db/models/query.py` out of a
# traceback, which is the richest identifier source these reports have.
FENCE = re.compile(r"```[^\n]*\n(.*?)```", re.DOTALL)
BACKTICK = re.compile(r"`([^`\n]+)`")

# A token a language would accept as a name, optionally dotted. Deliberately
# not anchored: it is used with `findall` over prose.
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*")

# "Identifier-shaped": an underscore, a camel boundary, or a dot. Required
# everywhere EXCEPT inside inline backticks, where a caller who wrote `bisect`
# meant the token and demanding shape would drop the identifiers that were
# explicitly flagged as identifiers.
SNAKE = re.compile(r"[a-z0-9]_[a-z0-9]", re.IGNORECASE)
CAMEL_BOUNDARY = re.compile(r"[a-z0-9][A-Z]")

MIN_IDENT_CHARS = 4

# Nothing a caller types as a name is this long. The bound exists because the
# things that are — base64 blobs, commit SHAs pasted whole, the DNA sequence —
# happen to satisfy every other rule.
MAX_IDENT_CHARS = 40

# An identifier query is a caller naming a few things. Past this it stops being
# one and becomes a bag of tokens that happens to score well because it quotes
# half the file. First appearance wins; the pre-cap count is reported so the
# truncation is visible rather than absorbed.
MAX_IDENTS = 12

# Words that pass the shape test and mean nothing as names. Kept SHORT and
# English-only on purpose: a long list is a tuning knob, and a corpus whose
# queries were tuned is not evidence. Anything longer belongs in a measured
# amendment, not here.
_STOPWORDS_TEXT = """
    that this with from have will been they there their which would could should
    when what where while because about after before other some such than then
    into over under between during without within your yours does done make made
    only also just like more most much many even here does very
"""
STOPWORDS = frozenset(_STOPWORDS_TEXT.split())

# `ident_df_min` costs one `git grep` per identifier per instance and is the
# only expensive thing here. A separate cap was drafted and then measured away:
# a grep against django's tree is ~0.1 s, so the whole corpus is ~16 minutes,
# once, for a query set that is frozen on commit. Two constants that must stay
# equal is a defect waiting to happen, and `MAX_IDENTS` already bounds the list.
VOWELS = "aeiouAEIOU"


# --------------------------------------------------------------------------
# Extraction
# --------------------------------------------------------------------------


def _accept(token: str, *, require_shape: bool) -> bool:
    if not MIN_IDENT_CHARS <= len(token) <= MAX_IDENT_CHARS:
        return False
    if token.lower() in STOPWORDS:
        return False
    if not require_shape:
        return True
    return bool(
        "_" in token
        or "." in token
        or SNAKE.search(token)
        or CAMEL_BOUNDARY.search(token)
    )


def extract_identifiers(text: str) -> list[str]:
    """Identifier-shaped tokens, in order of first appearance, deduplicated.

    Three sources, two rules. An **inline backtick** is an author writing "this
    token is a name", so shape is not required there. **Fenced blocks** and
    **free prose** both require shape: a token must carry an underscore, a dot
    or a camel boundary, so bare `cache` in a sentence stays a word while
    `cache_key` and `FileResponse` do not. Getting this wrong is silent in both
    directions — too strict and the arm reduces to a handful of dotted paths,
    too loose and it becomes the prose arm with the articles removed, which is
    what the fence rule was before the audit read it.
    """
    seen: dict[str, None] = {}

    def take(chunk: str, *, require_shape: bool) -> None:
        for token in IDENT.findall(chunk):
            if _accept(token, require_shape=require_shape):
                seen.setdefault(token, None)

    rest = text
    fenced = FENCE.findall(rest)
    rest = FENCE.sub(" ", rest)

    inline = BACKTICK.findall(rest)
    rest = BACKTICK.sub(" ", rest)

    for chunk in inline:
        take(chunk, require_shape=False)
    for chunk in fenced:
        take(chunk, require_shape=True)
    take(rest, require_shape=True)

    return list(seen)[:MAX_IDENTS]


def title_line(text: str) -> str:
    """The issue's first non-empty line — its title in every dataset here."""
    for line in text.splitlines():
        stripped = line.strip()
        if stripped:
            return stripped
    return ""


# --------------------------------------------------------------------------
# Perturbation (arm A2)
# --------------------------------------------------------------------------


def _to_camel(token: str) -> str:
    head, *tail = token.split("_")
    if not tail:
        return token
    return head + "".join(part[:1].upper() + part[1:] for part in tail)


def _to_snake(token: str) -> str:
    out = CAMEL_BOUNDARY.sub(lambda m: f"{m.group(0)[0]}_{m.group(0)[1]}", token)
    return out.lower() if out != token else token


def _flip_case_style(token: str) -> str:
    if "_" in token:
        return _to_camel(token)
    if CAMEL_BOUNDARY.search(token):
        return _to_snake(token)
    return token


def _drop_vowels(token: str) -> str:
    """Abbreviate as a hurried caller would: keep the first char, drop vowels."""
    out = token[0] + "".join(c for c in token[1:] if c not in VOWELS)
    return out if len(out) >= 3 else token


def _transpose(token: str, rng: random.Random) -> str:
    if len(token) < 4:
        return token
    i = rng.randrange(1, len(token) - 2)
    return token[:i] + token[i + 1] + token[i] + token[i + 2 :]


def mangle(token: str, rng: random.Random) -> str:
    """One perturbation, deterministic in `rng`, never a no-op if avoidable.

    The rules are tried from the one chosen, in a fixed rotation, so a token
    that cannot take its drawn rule (a flat lowercase name has no case style to
    flip) still gets perturbed rather than silently surviving intact into an
    arm whose whole premise is that the literal string is absent.
    """
    rules = (
        _flip_case_style,
        _drop_vowels,
        lambda t: _transpose(t, rng),
    )
    start = rng.randrange(len(rules))
    for offset in range(len(rules)):
        out = rules[(start + offset) % len(rules)](token)
        if out != token:
            return out
    return token


# --------------------------------------------------------------------------
# §9.6 statistics, measured against the snapshot
# --------------------------------------------------------------------------


def _blob(git: GitRepo, sha: str, path: str) -> str | None:
    proc = subprocess.run(
        ["git", "show", f"{sha}:{path}"],
        cwd=git.path,
        capture_output=True,
        text=True,
        errors="replace",
        check=False,
    )
    return proc.stdout if proc.returncode == 0 else None


def ident_in_gold(git: GitRepo, sha: str, gold: list[str], idents: list[str]) -> bool:
    """Does any identifier occur literally in any gold file at the snapshot?

    Case-sensitive substring, the same predicate `ident_df_min` uses — not a
    word match. A lexical leg matching `get_or_set` inside `_get_or_set_many`
    is a hit for BM25's trigram tokenizer, and this stratum exists to describe
    what that leg can reach, not what a tokenizer ought to consider a word.
    """
    if not idents:
        return False
    for path in gold:
        text = _blob(git, sha, path)
        if text and any(ident in text for ident in idents):
            return True
    return False


def df_min(git: GitRepo, sha: str, idents: list[str]) -> tuple[int | None, int]:
    """(df of the rarest *present* identifier, count of present identifiers).

    The minimum, not the maximum: it says that even the most selective string
    the caller supplied still matches this many files, which is the condition
    under which matching is free and ranking is the entire task (§9.6).

    "Present" is load-bearing and was the audit's second finding. Taken over
    every identifier, the minimum is 0 for almost every instance, because a bug
    report reliably contributes at least one token that appears nowhere in the
    tree — and 0 is not a collision statistic, it is route 1's statistic
    wearing route 3's name. An identifier that matches nothing is already
    described by `ident_in_gold` and by the absent count returned beside this.
    None means no identifier occurred anywhere, which is a real and different
    answer from a small minimum.
    """
    present: list[int] = []
    for ident in idents:
        proc = subprocess.run(
            ["git", "grep", "-l", "-F", "-e", ident, sha],
            cwd=git.path,
            capture_output=True,
            text=True,
            check=False,
        )
        # rc 1 is "no match", which is a real answer (0) rather than an error.
        if proc.returncode not in (0, 1):
            continue
        count = len(proc.stdout.splitlines())
        if count:
            present.append(count)
    return (min(present) if present else None), len(present)


def gold_vocabulary(gold: list[str]) -> set[str]:
    """The words a lexical matcher would find in the gold files' paths.

    Deliberately paths only. `sphinx_docs.file_vocabulary` reads identifiers
    out of a `SymbolIndex`, which is Python-and-Sphinx-only and does not exist
    for this tier; reading whole file bodies instead would put every English
    word in every file's vocabulary and report every query as `obvious`. Paths
    are the part both tiers can supply honestly, and the bucket thresholds are
    §3.0.1's, imported rather than restated.
    """
    vocab: set[str] = set()
    for path in gold:
        vocab |= words(path.replace("/", " ").replace(".", " "))
    return vocab


def lexical_overlap(query: str, vocab: set[str]) -> float:
    q = {w for w in words(query) if len(w) > 3}
    if not q:
        return 0.0
    return len(q & vocab) / len(q)


# --------------------------------------------------------------------------
# Projection
# --------------------------------------------------------------------------

INHERITED = ("gold_files", "gold_functions", "base_commit", "repo", "datasets")


def project(
    source: Instance,
    projection: str,
    idents: list[str],
    seed: int,
    vocab: set[str],
    *,
    in_gold: bool | None,
    df: int | None,
) -> Instance:
    if projection == "prose":
        query = source.query
    elif projection == "ident":
        query = " ".join(idents)
    elif projection == "ident-mangled":
        rng = random.Random(seed)
        query = " ".join(mangle(t, rng) for t in idents)
    elif projection == "ident-intent":
        query = " ".join([*idents, title_line(source.query)]).strip()
    else:
        raise ValueError(f"unknown projection {projection!r}")

    overlap = lexical_overlap(query, vocab)
    out = replace(
        source,
        instance_id=f"{source.instance_id}#{projection}",
        query=query,
        projection=projection,
        source_instance_id=source.instance_id,
        n_idents=len(idents),
        ident_in_gold=in_gold,
        ident_df_min=df,
        mangle_seed=seed if projection == "ident-mangled" else None,
        lexical_overlap=round(overlap, 4),
        overlap_bucket=overlap_bucket(overlap),
    )

    # The provenance guarantee, checked rather than trusted. A projection that
    # altered a gold set would be a new ground truth wearing an inherited one's
    # name, and nothing downstream could tell.
    for fieldname in INHERITED:
        if getattr(out, fieldname) != getattr(source, fieldname):
            raise RuntimeError(
                f"{source.instance_id}: projection changed {fieldname!r}, which "
                f"§3.4 requires to be inherited byte-identically"
            )
    return out


def build(
    qrels_path: Path,
    git: GitRepo,
    *,
    seed: int,
    with_df: bool,
) -> tuple[list[Instance], Counter[str]]:
    report: Counter[str] = Counter()
    out: list[Instance] = []

    with qrels_path.open() as fh:
        sources = [Instance(**json.loads(line)) for line in fh if line.strip()]

    report["source_instances"] = len(sources)
    for pos, source in enumerate(sources):
        idents = extract_identifiers(source.query)
        if not idents:
            # Not a drop of the instance — the prose arm is still measurable —
            # but the identifier arms would be empty queries, which score as
            # empty rankings and would silently depress every identifier
            # number with rows that asked nothing.
            report["no_identifiers"] += 1
            continue

        vocab = gold_vocabulary(source.gold_files)
        in_gold = ident_in_gold(git, source.base_commit, source.gold_files, idents)
        df, n_present = (
            df_min(git, source.base_commit, idents) if with_df else (None, 0)
        )

        report["kept"] += 1
        report["ident_in_gold"] += int(in_gold)
        if with_df:
            report["no_ident_present"] += int(df is None)
            report["idents_present"] += n_present
            report["idents_probed"] += len(idents)
        for projection in PROJECTIONS:
            out.append(
                project(
                    source,
                    projection,
                    idents,
                    seed + pos,
                    vocab,
                    in_gold=in_gold,
                    df=df,
                )
            )
    return out, report


# --------------------------------------------------------------------------
# Reporting
# --------------------------------------------------------------------------


def print_report(corpus: str, report: Counter[str], instances: list[Instance]) -> None:
    kept = report["kept"]
    print(f"\n{corpus}")
    print(f"  source instances:  {report['source_instances']}")
    print(f"  no identifiers:    {report['no_identifiers']} (all arms skipped)")
    print(f"  projected:         {kept} x {len(PROJECTIONS)} = {len(instances)}")
    if kept:
        share = 100.0 * report["ident_in_gold"] / kept
        print(f"  ident_in_gold:     {report['ident_in_gold']}/{kept} ({share:.1f}%)")
        if report["idents_probed"]:
            print(
                f"  idents present:    {report['idents_present']}/"
                f"{report['idents_probed']} probed; "
                f"{report['no_ident_present']} instance(s) had none"
            )

    by_arm: dict[str, list[Instance]] = {p: [] for p in PROJECTIONS}
    for inst in instances:
        by_arm[inst.projection or ""].append(inst)
    for arm, rows in by_arm.items():
        if not rows:
            continue
        overlaps = sorted(i.lexical_overlap or 0.0 for i in rows)
        lengths = sorted(len(i.query) for i in rows)
        buckets = Counter(i.overlap_bucket for i in rows)
        mid = len(rows) // 2
        print(
            f"  {arm:<14} median overlap {overlaps[mid]:.3f}  "
            f"median len {lengths[mid]:>5}  "
            f"{dict(buckets)}"
        )

    dfs = [i.ident_df_min for i in instances if i.ident_df_min is not None]
    if dfs:
        dfs.sort()
        print(
            f"  ident_df_min:      median {dfs[len(dfs) // 2]}, "
            f"p90 {dfs[int(len(dfs) * 0.9)]}, max {dfs[-1]}"
        )


def audit(instances: list[Instance], count: int, seed: int) -> None:
    """Print sampled instances for a human to read before anything is measured.

    Every corpus defect found so far was found this way and none would have
    failed a test (§11). Arms of one source are printed together, because the
    question a reader is answering is whether the projections still ask the
    same question — which cannot be seen from one arm alone.
    """
    by_source: dict[str, list[Instance]] = {}
    for inst in instances:
        by_source.setdefault(inst.source_instance_id or "", []).append(inst)

    rng = random.Random(seed)
    keys = sorted(by_source)
    for key in rng.sample(keys, min(count, len(keys))):
        arms = by_source[key]
        head = arms[0]
        print("\n" + "─" * 76)
        print(
            f"{key}  gold={head.gold_files}  "
            f"ident_in_gold={head.ident_in_gold}  df_min={head.ident_df_min}"
        )
        for inst in arms:
            print(
                f"  [{inst.projection:<13}] "
                f"({inst.overlap_bucket}, {inst.lexical_overlap}) "
                f"{inst.query[:300]}"
            )


# --------------------------------------------------------------------------
# Self-test. Each rule decides what a query says; a mistake produces a
# plausible corpus rather than an error.
# --------------------------------------------------------------------------


def self_test() -> int:
    failures: list[str] = []

    def check(label: str, got: Any, want: Any) -> None:
        if got != want:
            failures.append(f"{label}: got {got!r}, want {want!r}")

    # Free prose: shape is required, so bare words are not identifiers however
    # long. This is the rule that keeps arm A1 from being the prose arm with
    # the articles removed.
    check(
        "free prose keeps only shaped tokens",
        extract_identifiers(
            "The cache breaks when cache_key is reused by FileResponse."
        ),
        ["cache_key", "FileResponse"],
    )

    # Code marks are the author flagging a name, so shape is not required
    # there — and this is the half that is easy to lose, because the tokens it
    # admits look exactly like the ones free prose rejects.
    check(
        "backticks admit unshaped tokens",
        extract_identifiers("Passing `bisect` to the runner fails."),
        ["bisect"],
    )
    # Found by audit, not by reasoning: a fence is a dump, not a name. Every
    # token below is real output from real ripgrep instances, and every one of
    # them shipped into arm A1 before this rule existed.
    check(
        "fences require shape, so console noise stays out",
        extract_identifiers(
            "```console\n"
            "ripgrep 11.0.1 (rev 7bf7ceb5d3)\n"
            "-SIMD +AVX (compiled)\n"
            'File "django/db/models/query.py", line 12, in _fetch_all\n'
            "```\n"
        ),
        ["query.py", "_fetch_all"],
    )
    # A directory path is deliberately NOT an identifier here. `IDENT` does not
    # cross `/`, so `django/db/models/query.py` contributes its basename and its
    # frame name and nothing else. Admitting whole paths would maximise §9.2
    # leakage by construction — the query would spell the answer — and that axis
    # already has its own stratum; conflating it with this one would make the
    # corpus tautological in exactly the way §3.4 exists to avoid.
    check(
        "directory paths do not survive as identifiers",
        extract_identifiers("see src/backend/handlers.rs"),
        ["handlers.rs"],
    )
    check(
        "a pasted sequence cannot be an identifier",
        extract_identifiers("```\nCCAGCTACTCGGGAGGCTGAGGCTGGAGGATCGCTTGAGTCCAGG\n```"),
        [],
    )
    check(
        "an identifier query is a few names, not a bag of tokens",
        len(extract_identifiers(" ".join(f"`name_{n}`" for n in range(40)))),
        MAX_IDENTS,
    )

    check(
        "dotted paths survive from prose",
        extract_identifiers("See django.db.models for details."),
        ["django.db.models"],
    )
    check("short tokens are dropped", extract_identifiers("`abc` and `abcd`"), ["abcd"])
    check(
        "stopwords are dropped even when backticked",
        extract_identifiers("`because` matters"),
        [],
    )
    check(
        "order is first appearance and duplicates collapse",
        extract_identifiers("`beta_x` then `alpha_y` then `beta_x`"),
        ["beta_x", "alpha_y"],
    )

    # Each mangle rule, pinned individually — `mangle()` rotates between them,
    # so a rule that silently became a no-op would be masked by its neighbour.
    check("snake to camel", _flip_case_style("cache_key"), "cacheKey")
    check("camel to snake", _flip_case_style("FileResponse"), "file_response")
    check("flat name has no case style", _flip_case_style("bisect"), "bisect")
    check("vowel drop keeps the first char", _drop_vowels("cache_key"), "cch_ky")
    check(
        "transposition swaps one pair", _transpose("abcdef", random.Random(0))[0], "a"
    )

    # The property the whole arm rests on: the literal string must change.
    for token in ("cache_key", "FileResponse", "bisect", "django.db.models"):
        rng = random.Random(1)
        if mangle(token, rng) == token:
            failures.append(f"mangle left {token!r} intact")

    check(
        "title line is the first non-empty line",
        title_line("\n\n Crash on save \nx"),
        "Crash on save",
    )
    check("overlap of nothing is zero", lexical_overlap("", {"a"}), 0.0)
    check(
        "overlap counts content words only",
        lexical_overlap("cache_key views", {"cache", "key"}),
        0.5,
    )

    # The provenance assertion must fire. A projection that quietly rewrote a
    # gold set is the one failure this file cannot be allowed to have.
    src = Instance(
        instance_id="x",
        corpus="c",
        datasets=["d"],
        repo="r",
        base_commit="0" * 40,
        query="`cache_key` fails",
        gold_files=["a.py"],
    )
    out = project(src, "ident", ["cache_key"], 0, {"a"}, in_gold=True, df=3)
    check("projection changes only the query", out.gold_files, src.gold_files)
    check("arm ids name their source", out.source_instance_id, "x")
    check("arm ids are suffixed", out.instance_id, "x#ident")

    for line in failures:
        print(f"FAIL {line}")
    print(f"\n{'FAILED' if failures else 'ok'} — {len(failures)} failure(s)")
    return 1 if failures else 0


def main() -> int:
    root = repo_root()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--config", type=Path, default=root / "bench" / "corpora.toml")
    ap.add_argument("--corpus", action="append", dest="corpora")
    ap.add_argument("--audit", type=int, default=0, help="print N sampled sources")
    ap.add_argument("--audit-seed", type=int, default=0)
    ap.add_argument("--seed", type=int, default=20260806, help="mangle seed base")
    ap.add_argument(
        "--no-df",
        action="store_true",
        help="skip ident_df_min (one `git grep` per identifier is its whole cost)",
    )
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        return self_test()
    if not args.corpora:
        raise SystemExit("--corpus is required (or --self-test)")

    config = load_config(args.config)
    data_dir = root / config["run"]["data_dir"]
    clone_dir = root / config["run"]["clone_dir"]
    out_dir = data_dir / "qrels"
    out_dir.mkdir(parents=True, exist_ok=True)

    total = 0
    for name in args.corpora:
        qrels_path = out_dir / f"{name}.jsonl"
        if not qrels_path.exists():
            raise SystemExit(
                f"{qrels_path} does not exist — §3.4 projects the issue tier, so "
                f"`build_qrels.py --corpus {name}` has to have run first"
            )
        git = GitRepo(clone_dir / name)
        if not git.available:
            raise SystemExit(f"clone missing: {git.path} (run fetch.py)")

        instances, report = build(
            qrels_path, git, seed=args.seed, with_df=not args.no_df
        )
        print_report(name, report, instances)
        if args.audit:
            audit(instances, args.audit, args.audit_seed)

        out = out_dir / f"{name}-ident.jsonl"
        with out.open("w") as fh:
            for inst in sorted(instances, key=lambda i: (i.base_commit, i.instance_id)):
                fh.write(json.dumps(asdict(inst), sort_keys=True) + "\n")
        print(f"\n  -> {out.relative_to(root)}")
        total += len(instances)

    print(f"\ntotal instances: {total}")
    print(
        "\nFROZEN (PROTOCOL §5.6): instances are never added or removed after a\n"
        "result has been seen. `bench/.data/` is gitignored, so what actually\n"
        "records the freeze is the counts above, written into §12.13 BEFORE any\n"
        "identifier retrieval is scored — a rebuild that disagrees with that\n"
        "section is a changed corpus, whatever the file's timestamp says."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
