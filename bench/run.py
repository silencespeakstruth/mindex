#!/usr/bin/env python3
"""Execute one benchmark configuration over one or more corpora.

For each corpus this walks its instances in commit order, moving the working
tree and the index forward one snapshot at a time, and records the ranked
result list for every query. It measures nothing and decides nothing:
`score.py` reads what this writes.

THE SNAPSHOT RULE, which is the whole reason this file is not a for-loop over
queries. Every instance names its own `base_commit`. Indexing the repository
once, at any single commit, is wrong in both directions: for an instance whose
fix landed before that commit the index contains the answer, and for one whose
gold files did not exist yet it cannot contain it at all. So the tree is
checked out per instance and the index is moved with it. That is affordable
only because mindex skips unchanged files by sha256 AND derivation version, so
a step between adjacent commits costs the diff rather than the repository.

THREE THINGS THAT WOULD SILENTLY CORRUPT A RUN, each handled here:

  - A checkout deletes files, and mindex's indexer only ever uploads what it
    finds; nothing tells the server a path is gone. Left alone, chunks from
    commits already passed keep answering queries. Every step therefore runs a
    drift check and deletes the `orphaned` bucket. This is exactly what
    /drift exists for.
  - Deletion is soft and indexing is append-only, so the vectors of every
    superseded chunk stay in Qdrant until GC. At ~764 KiB per chunk a corpus
    of 850 checkouts orphans far more than it indexes, so `POST /gc` runs on
    a fixed interval rather than at the end.
  - An incrementally-reached index is only equal to a freshly-built one if
    mindex's skip logic is correct. Assuming that is how a harness measures a
    bug as a quality score, so --equivalence-sample rebuilds a sample of
    snapshots from scratch and compares chunk boundaries. A divergence stops
    the run.
"""

from __future__ import annotations

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import time
import urllib.parse
import uuid
from collections.abc import Generator
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import httpx
import tomllib
from fetch import load_config, repo_root, select_corpora

# One namespace, so a (label, corpus) pair always names the same project. A run
# interrupted halfway is resumable, and two configurations can never collide in
# Qdrant because the collection name derives from this GUID.
BENCH_NAMESPACE = uuid.UUID("6f9b1f2c-4a1e-5d3b-9c8a-2e7d4f1a0b63")

# Chunks requested per query — NOT the depth of the ranking that gets scored.
#
# Ground truth is at file level and mindex returns chunks, so the scored ranking
# is the chunk list deduplicated to files. Those are very different depths: at
# `top_k = 20` the 12 ripgrep queries came back with a **mean of 8.9 distinct
# files** (min 6, max 12), so 12 of 12 could not reach the deepest cutoff and 9
# of 12 could not reach the primary one. Recall@20 was silently recall@9, and
# the flat tail that produced looked like a finding about retrieval.
#
# 100 is the server's `[search].max_top_k` default and yields ~21.7 distinct
# files here. Every configuration and baseline is cut at the same depth.
#
# RETRACTED 2026-08-06 (PROTOCOL §11): this comment used to justify raising
# top_k by arguing it is "a truncation, not a retrieval decision", because "the
# pipeline prefetches 200 dense + 200 sparse, fuses, reranks with ColBERT and
# then cuts". That was true under v2 and is false under v3, which asks Qdrant
# for `top_k` directly — there is no prefetch, no fusion and no rerank, so the
# depth IS a retrieval decision and a deeper cut is a different HNSW search,
# not a longer slice of one ranking. The value is unchanged and comparisons
# stay valid (all arms share the depth); what is gone is the reason it was
# safe. Should a lexical leg ship, the original argument becomes true again.
TOP_K = 100

# Indexing a large repository from cold is minutes of GPU work in one call.
INDEX_TIMEOUT_S = 3600
HTTP_TIMEOUT_S = 300

# How many times one query is asked before its 5xx is taken as an answer about
# mindex rather than about the weather. Three, because the point is only to
# separate a blink from a deterministic refusal.
SEARCH_ATTEMPTS = 3
SEARCH_RETRY_BACKOFF_S = 2.0


# ---------------------------------------------------------------------------
# Provenance
# ---------------------------------------------------------------------------


def git_out(args: list[str], cwd: Path | None = None) -> str:
    proc = subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, check=False
    )
    if proc.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {proc.stderr.strip()}")
    return proc.stdout.strip()


def const_from_source(root: Path, relpath: str, name: str) -> str | None:
    """Read a `const NAME: &str = "x";` out of the tree this binary was built from.

    The versions are compile-time constants with no runtime accessor, so source
    is the only place to read them. That is sound exactly as far as the binary
    matches the tree, which is why `mindex_git_dirty` is recorded beside them.
    """
    text = (root / relpath).read_text()
    marker = f"{name}: &str = "
    idx = text.find(marker)
    if idx < 0:
        return None
    start = text.index('"', idx) + 1
    return text[start : text.index('"', start)]


def embedder_invocation(server_url: str | None = None) -> str | None:
    """Who is serving the embeddings, as a string that changes when they do.

    Which process produced a set of vectors is a real confound and nothing on
    the wire says so: `/health` reports liveness, and `/v1/models` reports the
    model NAME, which is identical across backends serving the same weights at
    different precisions — the case CLAUDE.md documents as presenting like a
    ranking-quality problem rather than an error.

    So the identity is the **argv of whatever holds the embedder's port**, read
    through the listening socket rather than by grepping the process table for
    a server name. The grep was the first version and it was wrong twice over:
    it named `bge_m3_api`, a server that no longer exists, and any command line
    merely MENTIONING that string matched — including this harness's own shell.
    A false "the embedder changed" aborts a corpus that was fine.

    Falls back to `/v1/models` when the socket cannot be attributed (a container,
    another user's process), and to None when there is nothing to ask.
    """
    if not server_url:
        return None
    port = urllib.parse.urlparse(server_url).port
    if port:
        proc = subprocess.run(
            ["ss", "-ltnpH", f"sport = :{port}"],
            capture_output=True,
            text=True,
            check=False,
        )
        match = re.search(r"pid=(\d+)", proc.stdout)
        if match:
            try:
                raw = Path(f"/proc/{match.group(1)}/cmdline").read_bytes()
                return raw.replace(b"\0", b" ").decode(errors="replace").strip()
            except OSError:
                pass
        served = probe_json(f"{server_url.rstrip('/')}/v1/models")
        if served:
            return json.dumps(served, sort_keys=True)
    return None


def probe_json(url: str) -> dict[str, Any] | None:
    try:
        with httpx.Client(timeout=10, verify=False) as client:
            resp = client.get(url)
            resp.raise_for_status()
            data = resp.json()
            return data if isinstance(data, dict) else None
    except (httpx.HTTPError, ValueError):
        # Provenance is recorded best-effort: a dependency that cannot describe
        # itself must not stop a run, it must show up as a null in the row.
        return None


def build_provenance(
    root: Path, server_config: Path, label: str, seed: int
) -> dict[str, Any]:
    config_bytes = server_config.read_bytes()
    model_cfg = tomllib.loads(server_config.read_text()).get("model", {})
    embedder_url = model_cfg.get("server_url")
    embedder = probe_json(f"{embedder_url.rstrip('/')}/health") if embedder_url else {}
    embedder = embedder or {}
    qdrant = probe_json("http://localhost:6333/") or {}
    return {
        "label": label,
        "seed": seed,
        "mindex_version": None,  # filled from /version once the server answers
        "mindex_git_sha": git_out(["rev-parse", "HEAD"], cwd=root),
        "mindex_git_dirty": bool(git_out(["status", "--porcelain", "src"], cwd=root)),
        "chunks_derivation_version": const_from_source(
            root, "src/slicing/traits.rs", "CHUNKS_DERIVATION_VERSION"
        ),
        "symbols_derivation_version": const_from_source(
            root, "src/slicing/symbols.rs", "SYMBOLS_DERIVATION_VERSION"
        ),
        "collection_schema_version": const_from_source(
            root, "src/db/qdrant.rs", "COLLECTION_SCHEMA_VERSION"
        ),
        "server_config_sha256": hashlib.sha256(config_bytes).hexdigest(),
        "embedder_invocation": embedder_invocation(embedder_url),
        "embedder_server_url": embedder_url,
        "embedder_health": embedder,
        "qdrant_version": qdrant.get("version"),
        "top_k": TOP_K,
    }


# ---------------------------------------------------------------------------
# Server
# ---------------------------------------------------------------------------


class Server:
    """The benchmark's client of its own mindex instance.

    Deliberately thin: a helper that retried, reordered or filtered would be a
    second retrieval system sitting between mindex and the score.
    """

    def __init__(self, base_url: str, *, verify: bool) -> None:
        self.base_url = base_url.rstrip("/")
        self.client = httpx.Client(timeout=HTTP_TIMEOUT_S, verify=verify)

    def close(self) -> None:
        self.client.close()

    def version(self) -> str:
        resp = self.client.get(f"{self.base_url}/version")
        resp.raise_for_status()
        return str(resp.json().get("version"))

    def search(
        self,
        guid: str,
        query: str,
        top_k: int,
        exclude_paths: list[str] | None = None,
    ) -> tuple[list[dict], float, str | None]:
        """One query. Returns (results, latency_ms, refusal code or None).

        A 5xx is retried, and what survives the retries is recorded rather than
        raised on — the two are told apart by behaviour instead of by guessing
        at a cause. A dependency that blinked answers the second attempt; a
        query the server cannot serve refuses every attempt identically, which
        is a fact about mindex and belongs in the results as a zero, not in an
        exclusion that would quietly drop the hardest queries from the corpus.

        Retrying is not a second retrieval system: it re-asks the same question
        of the same index and never reorders, filters or merges an answer.
        """
        last_code: str | None = None
        body: dict[str, Any] = {"query": query, "top_k": top_k}
        if exclude_paths:
            # The descriptive tier lifts its queries out of the docs tree, which
            # is indexed on purpose, so those files would come back first by
            # near-exact match — a tautology rather than a result. Dropped from
            # the RANKING and not from the index, through the same selector a
            # real caller would use, so the index stays the deployed one.
            body["exclude"] = {"paths": exclude_paths}
        started = time.perf_counter()
        for attempt in range(SEARCH_ATTEMPTS):
            resp = self.client.post(f"{self.base_url}/v0/{guid}/search", json=body)
            # 404 is the contract for "nothing active matched", not an error.
            if resp.status_code == 404:
                return [], (time.perf_counter() - started) * 1000, None
            if resp.status_code < 500:
                resp.raise_for_status()
                elapsed_ms = (time.perf_counter() - started) * 1000
                return list(resp.json()["results"]), elapsed_ms, None
            try:
                last_code = str(resp.json().get("code"))
            except ValueError:
                last_code = f"http.{resp.status_code}"
            if attempt < SEARCH_ATTEMPTS - 1:
                time.sleep(SEARCH_RETRY_BACKOFF_S * (attempt + 1))
        return [], (time.perf_counter() - started) * 1000, last_code

    def delete_files(self, guid: str, paths: list[str]) -> int:
        resp = self.client.request(
            "DELETE",
            f"{self.base_url}/projects/{guid}/files",
            json={"include": {"paths": paths}},
        )
        if resp.status_code == 204:
            return 0
        resp.raise_for_status()
        return int(resp.json()["deleted_files"])

    def delete_project(self, guid: str) -> None:
        resp = self.client.delete(f"{self.base_url}/projects/{guid}")
        resp.raise_for_status()

    def gc(self) -> dict[str, Any]:
        resp = self.client.post(f"{self.base_url}/gc")
        # 409 means a sweep is already running; the next interval collects it.
        if resp.status_code == 409:
            return {"skipped": "gc_in_progress"}
        resp.raise_for_status()
        return dict(resp.json())

    def failed_files(self, guid: str) -> list[str]:
        """Paths the server could not index.

        A failed file is not in the index and cannot be retrieved, so it lowers
        recall without appearing anywhere in the scores. Recorded per instance
        rather than raised on: mindex's retry worker clears transient failures
        by itself, and a run that aborted on one would never finish a corpus.
        What must not happen is that it goes unrecorded.
        """
        resp = self.client.get(f"{self.base_url}/projects/{guid}/files?status=failed")
        if resp.status_code == 404:
            return []
        resp.raise_for_status()
        body = resp.json()
        files = body.get("files", body) if isinstance(body, dict) else body
        return [f["path"] if isinstance(f, dict) else str(f) for f in files]

    def project(self, guid: str) -> dict[str, Any] | None:
        resp = self.client.get(f"{self.base_url}/projects/{guid}")
        if resp.status_code == 404:
            return None
        resp.raise_for_status()
        return dict(resp.json())

    def integrity_counters(self) -> dict[str, float]:
        """The two counters that say the retrieval itself cannot be trusted.

        Read from /metrics because there is no other way to see them: both
        describe something the server handled without erroring, so the response
        to a query that suffered one is a well-formed 200. Returns zeros when
        metrics are off, which keeps an older config runnable at the cost of
        the check — the alternative, refusing to run, would make a benchmark
        depend on an observability switch.
        """
        try:
            resp = self.client.get(f"{self.base_url}/metrics")
            resp.raise_for_status()
        except httpx.HTTPError:
            return {}
        wanted = ("mindex_search_unscorable_winners", "mindex_search_orphaned_winners")
        found: dict[str, float] = {}
        for line in resp.text.splitlines():
            if line.startswith("#"):
                continue
            name, _, value = line.partition(" ")
            base = name.split("{", 1)[0].removesuffix("_total")
            if base in wanted:
                found[base] = found.get(base, 0.0) + float(value)
        return found


# ---------------------------------------------------------------------------
# Working tree
# ---------------------------------------------------------------------------


class Clone:
    def __init__(self, path: Path) -> None:
        self.path = path

    def checkout(self, sha: str) -> None:
        # -f discards the previous snapshot's modifications; `clean -qfdx`
        # removes anything a previous commit tracked that this one ignores,
        # which would otherwise stay on disk and be indexed as a live file.
        git_out(["checkout", "-f", "-q", sha], cwd=self.path)
        git_out(["clean", "-qfdx"], cwd=self.path)

    def head(self) -> str:
        return git_out(["rev-parse", "HEAD"], cwd=self.path)

    def commit_times(self, shas: list[str]) -> dict[str, int]:
        """Committer timestamps, in one call rather than one call per instance."""
        out = git_out(
            ["log", "--no-walk=unsorted", "--format=%H %ct", *shas], cwd=self.path
        )
        times = {}
        for line in out.splitlines():
            sha, _, ts = line.partition(" ")
            times[sha] = int(ts)
        return times


def run_indexer(
    root: Path, guid: str, server_url: str, *, no_verify: bool, concurrency: int
) -> tuple[int, str]:
    """Index the current working tree. Returns (elapsed_ms, indexer stdout tail).

    mindex-index is the client here rather than a bespoke walker on purpose:
    CLAUDE.md's four-clients rule says the file set, the path spelling and the
    hashed bytes have exactly one definition per client, and a benchmark that
    invented a fifth would be measuring its own scanner.
    """
    cmd = [
        "mindex-index",
        "--server",
        server_url,
        "--project",
        guid,
        "--root",
        str(root),
        "--concurrency",
        str(concurrency),
    ]
    if no_verify:
        cmd.append("--no-verify")
    started = time.perf_counter()
    proc = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        check=False,
        timeout=INDEX_TIMEOUT_S,
        env=indexer_env(),
    )
    elapsed_ms = int((time.perf_counter() - started) * 1000)
    if proc.returncode != 0:
        # The indexer exits non-zero when ANY file failed, which conflates two
        # very different things. "It could not run" must stop the benchmark.
        # "It ran, and n of 3 553 files failed" must not: mindex's retry worker
        # clears transient failures by itself, and both failures observed here
        # were transient — a concurrent harness process, and the embedder being
        # restarted mid-index (`Connection refused`, and the file was
        # re-indexed before anyone looked). Killing an hours-long run for that
        # is worse than recording it, and it IS recorded: every result row
        # carries the server's own count of files still `failed`.
        # It reports on stderr, not stdout — read both rather than guessing.
        output = f"{proc.stdout}\n{proc.stderr}"
        summary = "files ·" in output or "files total" in output
        if not summary:
            raise RuntimeError(
                f"mindex-index failed ({proc.returncode}) without completing:\n"
                f"{proc.stderr[-4000:]}"
            )
        tail = output.strip().splitlines()[-1].strip()
        print(
            f"    WARN: indexer reported failures and exited {proc.returncode}: {tail}"
        )
    return elapsed_ms, proc.stdout[-2000:]


def indexer_env() -> dict[str, str]:
    """The indexer's environment, with this host's live credential removed.

    $MINDEX_TOKEN is exported in the user's shell for the live server. The
    bench server authorizes nothing, so the token is merely inert there — but
    inheriting it silently is how a harness ends up pointed at the wrong
    instance without noticing, so it is dropped rather than tolerated.
    """
    env = dict(os.environ)
    env.pop("MINDEX_TOKEN", None)
    env.pop("MINDEX_TOKEN_FILE", None)
    return env


def drift(
    root: Path, guid: str, server_url: str, *, no_verify: bool
) -> dict[str, list[str]]:
    cmd = [
        "mindex-index",
        "--server",
        server_url,
        "--project",
        guid,
        "--root",
        str(root),
        "--check",
        "--json",
    ]
    if no_verify:
        cmd.append("--no-verify")
    proc = subprocess.run(
        cmd, capture_output=True, text=True, check=False, env=indexer_env()
    )
    # --check exits non-zero when it finds actionable drift, which is the
    # ordinary case here: the whole point is to find the orphans.
    if not proc.stdout.strip():
        raise RuntimeError(f"drift check produced no JSON:\n{proc.stderr[-2000:]}")
    return dict(json.loads(proc.stdout))


# ---------------------------------------------------------------------------
# Index-state equivalence
# ---------------------------------------------------------------------------


@dataclass
class IndexState:
    """Everything about an index that a correct incremental step must preserve."""

    files: set[str]
    chunks: dict[str, list[tuple[int, int]]]

    @property
    def chunk_count(self) -> int:
        return sum(len(v) for v in self.chunks.values())


def read_index_state(db_path: Path, guid: str) -> IndexState:
    """Read active files and chunk spans straight from the benchmark's SQLite.

    Read-only, and against the bench database only. The API cannot answer this:
    equivalence is a claim about chunk boundaries, and no endpoint publishes
    them for a whole project.
    """
    uri = f"file:{db_path}?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    try:
        guid_simple = uuid.UUID(guid).hex
        files = {
            row[0]
            for row in conn.execute(
                "SELECT path FROM project_files "
                "WHERE project_guid = ? AND status != 'deleted'",
                (guid_simple,),
            )
        }
        chunks: dict[str, list[tuple[int, int]]] = {}
        for path, start, end in conn.execute(
            "SELECT file_path, start_line, end_line FROM project_file_chunks "
            "WHERE project_guid = ? AND status = 'active' "
            "ORDER BY file_path, start_line, end_line",
            (guid_simple,),
        ):
            chunks.setdefault(path, []).append((start, end))
        return IndexState(files=files, chunks=chunks)
    finally:
        conn.close()


def compare_states(incremental: IndexState, fresh: IndexState) -> list[str]:
    """Differences that make the incremental index not the one we claim to measure."""
    problems = []
    only_inc = sorted(incremental.files - fresh.files)
    only_fresh = sorted(fresh.files - incremental.files)
    if only_inc:
        problems.append(f"{len(only_inc)} file(s) only in incremental: {only_inc[:5]}")
    if only_fresh:
        problems.append(f"{len(only_fresh)} file(s) only in fresh: {only_fresh[:5]}")
    for path in sorted(set(incremental.chunks) & set(fresh.chunks)):
        if incremental.chunks[path] != fresh.chunks[path]:
            problems.append(
                f"chunk spans differ for {path}: "
                f"{len(incremental.chunks[path])} vs {len(fresh.chunks[path])} chunks"
            )
    return problems


def check_equivalence(
    *,
    clone: Clone,
    guid: str,
    sha: str,
    server: Server,
    db_path: Path,
    server_url: str,
    no_verify: bool,
    concurrency: int,
) -> list[str]:
    """Rebuild this snapshot from scratch into a scratch project and compare.

    The scratch project is deleted immediately, because a second copy of a
    corpus is a second copy of its Qdrant storage and django's is ~47 GiB.
    """
    scratch = str(uuid.uuid4())
    try:
        run_indexer(
            clone.path,
            scratch,
            server_url,
            no_verify=no_verify,
            concurrency=concurrency,
        )
        incremental = read_index_state(db_path, guid)
        fresh = read_index_state(db_path, scratch)
        problems = compare_states(incremental, fresh)
        if problems:
            problems.insert(0, f"snapshot {sha[:10]}")
        return problems
    finally:
        try:
            server.delete_project(scratch)
        except httpx.HTTPError as exc:
            # Worth a line rather than a raise: the comparison already happened,
            # and what is left behind is one scratch collection an operator can
            # drop. Silence would leave it growing across a sweep.
            print(f"    WARN: scratch project {scratch} not deleted: {exc}")


# ---------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------


def project_guid(label: str, corpus: str) -> str:
    return str(uuid.uuid5(BENCH_NAMESPACE, f"{label}/{corpus}"))


def load_qrels(path: Path) -> list[dict[str, Any]]:
    with path.open() as fh:
        return [json.loads(line) for line in fh if line.strip()]


@contextlib.contextmanager
def corpus_lock(clone_path: Path) -> Generator[None]:
    """Exclusive use of one corpus, refused rather than queued.

    Two runs sharing a corpus corrupt each other in two ways that both produce
    plausible numbers instead of errors. They fight over `git checkout`, so
    each queries a tree the other moved; and if they also share a label they
    share a project, where one process's `DELETE /projects/{guid}` lands in the
    middle of the other's `/index` — observed here as chunk inserts failing the
    foreign key and files ending `failed` for reasons that have nothing to do
    with mindex.

    Keyed on the clone rather than on the project, because two runs at
    different labels have different GUIDs and would slip past a project-keyed
    lock while still sharing the one working tree.
    """
    lock_path = clone_path.parent / f".{clone_path.name}.bench.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("w") as handle:
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as exc:
            raise SystemExit(
                f"another benchmark run holds {clone_path.name}; wait for it or "
                f"kill it. Two runs on one corpus produce numbers, not errors."
            ) from exc
        handle.write(f"{os.getpid()}\n")
        handle.flush()
        try:
            yield
        finally:
            fcntl.flock(handle, fcntl.LOCK_UN)


def run_corpus(
    *,
    corpus: dict[str, Any],
    config: dict[str, Any],
    root: Path,
    server: Server,
    provenance: dict[str, Any],
    label: str,
    limit: int | None,
    equivalence_sample: int,
    fresh: bool,
    concurrency: int,
    qrels_suffix: str,
    run_tag: str,
) -> Path:
    name = corpus["name"]
    qrels_name = f"{name}{qrels_suffix}"
    run_cfg = config["run"]
    clone = Clone(root / run_cfg["clone_dir"] / name)
    qrels_path = root / run_cfg["data_dir"] / "qrels" / f"{qrels_name}.jsonl"
    if not qrels_path.exists():
        raise SystemExit(f"no query set for {name}; run build_qrels.py first")

    instances = load_qrels(qrels_path)
    exclude_paths = corpus.get("search_exclude_paths") or None

    # A corpus whose instances all name one commit needs one checkout and one
    # index, not one per query. That is the descriptive tier: documentation
    # describes the code beside it, so there is no "before the fix" to snapshot
    # and nothing to move between queries. It turns django from 1 063 rebuilds
    # into one.
    single_snapshot = len({i["base_commit"] for i in instances}) == 1

    # Commit order, not the SHA order the query set is stored in. Adjacent
    # snapshots share nearly all of their content, which is what turns N full
    # indexes into one index and N diffs; a random walk over history would
    # reindex most of the tree every step and measure the same thing far
    # slower.
    times = clone.commit_times([i["base_commit"] for i in instances])
    instances.sort(key=lambda i: (times.get(i["base_commit"], 0), i["instance_id"]))
    if limit:
        instances = instances[:limit]

    guid = project_guid(label, name)
    if fresh:
        try:
            server.delete_project(guid)
            print(f"  dropped existing project {guid}")
        except httpx.HTTPStatusError:
            pass

    results_dir = root / run_cfg["results_dir"]
    results_dir.mkdir(parents=True, exist_ok=True)
    # The tag names the repetition, the label names the project. They are
    # separate so a run can re-query an index it did not rebuild: that is what
    # separates the noise the query path contributes from the noise indexing
    # contributes, and the two have different remedies.
    out_path = results_dir / f"{label}{run_tag}__{qrels_name}.jsonl"

    # Which snapshots get the from-scratch comparison. Spread evenly rather
    # than sampled randomly: the first steps of a corpus are the ones where
    # incremental and fresh trivially agree, so a random draw concentrated
    # there would pass without testing anything.
    #
    # ON A SINGLE-SNAPSHOT CORPUS THE SAMPLE IS ONE, and this is not a
    # relaxation of the check — it is the check applied to what is actually
    # there. The comparison exists to catch an index that was reached
    # INCREMENTALLY and diverged from a cold build. A descriptive corpus checks
    # out once, indexes once, and then never moves: every later instance re-runs
    # the query set against a byte-identical index (the log says `index=0ms` on
    # each), nothing is soft-deleted, and every GC tick reports
    # `chunks_removed: 0`. So there is exactly one index state in the run, and
    # sampling it twenty times re-verifies the same state nineteen times over —
    # measured on django-docs-short, that is 20 x ~148 s of cold rebuild inside
    # a 55-minute run whose queries take 45 ms each.
    #
    # The one check is placed at the FIRST instance rather than the last, on the
    # grounds that a wrong index should cost three minutes rather than an hour;
    # nothing after that point can change it.
    check_at: set[int] = set()
    if equivalence_sample > 0 and instances:
        if single_snapshot:
            check_at = {0}
        else:
            stride = max(1, len(instances) // equivalence_sample)
            check_at = set(range(stride - 1, len(instances), stride))

    # Same reasoning, same condition: GC reclaims the vectors of chunks a
    # reindex superseded, and a single-snapshot corpus supersedes nothing. Every
    # tick on django-docs-short reported zeros. Skipping them removes a periodic
    # process-wide lock from the middle of a latency measurement.
    gc_every = 0 if single_snapshot else int(run_cfg["gc_every_instances"])
    server_url = run_cfg["server_url"]
    no_verify = bool(run_cfg["no_verify"])
    db_path = Path(config["run"]["db_path"])

    print(f"\n=== {name}: {len(instances)} instances, project {guid} ===")
    # State the plan rather than silently doing less work: a run that skipped
    # its own integrity check without saying so is the failure this check
    # exists to prevent, one level up.
    if single_snapshot:
        print(
            "  single snapshot: one checkout, one index, "
            f"{len(check_at)} equivalence rebuild(s), no periodic GC"
        )
    else:
        print(
            f"  {len(instances)} snapshots: {len(check_at)} equivalence rebuild(s), "
            f"GC every {gc_every}"
        )
    started_all = time.perf_counter()
    equivalence_problems: list[str] = []
    refusals: dict[str, int] = {}

    # Baselines for the two counters that invalidate a run rather than describe
    # it, and for the identity of the process producing the vectors.
    integrity_at_start = server.integrity_counters()
    if not integrity_at_start:
        print("  WARN: /metrics unavailable; NaN and orphaned winners go unchecked")
    embedder_at_start = provenance["embedder_invocation"]
    embedder_url = provenance.get("embedder_server_url")

    with out_path.open("w") as out:
        indexed_sha: str | None = None
        head = ""
        for pos, inst in enumerate(instances, start=1):
            sha = inst["base_commit"]
            index_ms = 0
            pruned = 0
            # The tree and the index are moved only when the commit actually
            # changes. Two instances naming the same sha cannot have different
            # trees, so for the second the checkout, the reindex and the drift
            # prune have nothing to do.
            #
            # This used to be conjoined with `single_snapshot`, which made it
            # fire only for the descriptive tier — where every instance shares
            # one commit. The condition it really needs is the equality alone,
            # and that matters as soon as a corpus holds several *queries* per
            # snapshot: the identifier projections (§3.4) emit four arms per
            # source instance, so on django the conjunct would buy 3 248 full
            # reindexes where 812 are required. `single_snapshot` still governs
            # the equivalence sample and the GC cadence below, which genuinely
            # ask whether the whole corpus is one tree.
            if indexed_sha != sha:
                clone.checkout(sha)
                head = clone.head()
                if head != sha:
                    raise RuntimeError(f"checkout landed on {head}, expected {sha}")

                index_ms, _ = run_indexer(
                    clone.path,
                    guid,
                    server_url,
                    no_verify=no_verify,
                    concurrency=concurrency,
                )

                # Files the checkout removed are still indexed and still
                # answering queries.
                report = drift(clone.path, guid, server_url, no_verify=no_verify)
                orphaned = report.get("orphaned", [])
                if orphaned:
                    pruned = server.delete_files(guid, orphaned)
                    if pruned != len(orphaned):
                        # A path carrying glob metacharacters would delete
                        # nothing and leave a stale file answering queries for
                        # the rest of the corpus. Loud, because it is invisible
                        # in the scores.
                        raise RuntimeError(
                            f"asked to delete {len(orphaned)} orphans, server "
                            f"deleted {pruned}; a path may contain glob "
                            f"metacharacters"
                        )
                indexed_sha = sha

            # The claim the whole design rests on, asserted rather than assumed.
            missing_gold = [
                p for p in inst["gold_files"] if not (clone.path / p).exists()
            ]
            if missing_gold:
                raise RuntimeError(
                    f"{inst['instance_id']}: gold path(s) absent at {sha[:10]}: "
                    f"{missing_gold}"
                )

            failed = server.failed_files(guid) if index_ms else []
            if failed:
                print(f"    WARN: {len(failed)} file(s) failed to index: {failed[:5]}")

            results, latency_ms, refusal = server.search(
                guid, inst["query"], TOP_K, exclude_paths=exclude_paths
            )
            if refusal:
                refusals[refusal] = refusals.get(refusal, 0) + 1
                print(
                    f"    REFUSED {inst['instance_id']}: {refusal} "
                    f"({len(inst['query'].encode())} bytes) — scored as an empty "
                    f"ranking"
                )

            # Stop the moment retrieval stops being trustworthy. Unlike a failed
            # file — which lowers recall and can be reported — a NaN score
            # reorders the answer, and a chunk Qdrant scored without a SQLite
            # row means the two stores disagree. Neither raises anywhere, and
            # every number produced after one is a measurement of the fault.
            integrity = server.integrity_counters()
            for counter, before in integrity_at_start.items():
                now = integrity.get(counter, before)
                if now > before:
                    raise RuntimeError(
                        f"{counter} rose from {before:g} to {now:g} at "
                        f"{inst['instance_id']}. Retrieval is no longer scoring "
                        f"what it retrieved; check the embedder backend (NaN on "
                        f"padded fp16 rows) or Qdrant/SQLite agreement before "
                        f"trusting anything from this run."
                    )

            # The embedder is a separate process that can be restarted, and this
            # host has two backends that are documented as NOT bit-identical.
            # Swapping one mid-corpus makes the first half and the second half
            # different experiments sharing one results file.
            if embedder_invocation(embedder_url) != embedder_at_start:
                raise RuntimeError(
                    f"the embedder changed during {name}: the vectors before "
                    f"{inst['instance_id']} were produced by a different process "
                    f"than the ones after it. Restart the corpus."
                )

            if pos - 1 in check_at:
                problems = check_equivalence(
                    clone=clone,
                    guid=guid,
                    sha=sha,
                    server=server,
                    db_path=db_path,
                    server_url=server_url,
                    no_verify=no_verify,
                    concurrency=concurrency,
                )
                if problems:
                    equivalence_problems.extend(problems)
                    print("    EQUIVALENCE FAILURE:")
                    for line in problems:
                        print(f"      {line}")

            record = {
                "schema": 1,
                "corpus": name,
                "language": corpus["language"],
                "instance_id": inst["instance_id"],
                "datasets": inst["datasets"],
                "base_commit": sha,
                "snapshot_sha": head,
                "query_bytes": len(inst["query"].encode()),
                "gold_files": inst["gold_files"],
                "gold_functions": inst["gold_functions"],
                "n_gold": inst["n_gold"],
                "category": inst["category"],
                "leaks_gold_path": inst["leaks_gold_path"],
                "leaks_gold_basename": inst["leaks_gold_basename"],
                "lexical_overlap": inst.get("lexical_overlap"),
                "overlap_bucket": inst.get("overlap_bucket"),
                # PROTOCOL §3.4/§9.6 — absent on every other tier. `score.py`
                # cuts F10's strata from the result record rather than from the
                # qrels file, so a run that dropped these would be unscoreable
                # for the family it was made for.
                "projection": inst.get("projection"),
                "ident_in_gold": inst.get("ident_in_gold"),
                "ident_df_min": inst.get("ident_df_min"),
                "doc_path": inst.get("doc_path"),
                "results": [
                    {
                        "path": r["path"],
                        "score": r["score"],
                        "start_line": r["start_line"],
                        "end_line": r["end_line"],
                    }
                    for r in results
                ],
                "n_results": len(results),
                # The depth of the ranking that will actually be scored. Without
                # it a recall@k over a shorter ranking is indistinguishable from
                # a system that ranked badly to depth k.
                "distinct_files": len({r["path"] for r in results}),
                "refusal": refusal,
                "search_ms": round(latency_ms, 1),
                "index_ms": index_ms,
                "orphans_pruned": pruned,
                "failed_files": len(failed),
                "failed_paths": failed[:20] if failed else [],
                "prov": provenance,
            }
            out.write(json.dumps(record, sort_keys=True) + "\n")
            out.flush()

            if gc_every and pos % gc_every == 0:
                gc_report = server.gc()
                print(f"  [{pos}/{len(instances)}] gc: {gc_report}")
            elif pos % 10 == 0 or pos == len(instances):
                print(
                    f"  [{pos}/{len(instances)}] {sha[:10]} "
                    f"index={index_ms}ms search={latency_ms:.0f}ms "
                    f"hits={len(results)} pruned={pruned}"
                )

    # One final sweep, so the next configuration does not start on top of this
    # one's orphans.
    print(f"  final gc: {server.gc()}")
    elapsed = time.perf_counter() - started_all
    print(f"  wrote {out_path.relative_to(root)} in {elapsed / 60:.1f} min")

    if refusals:
        total = sum(refusals.values())
        print(
            f"  {total}/{len(instances)} queries were refused and scored zero: "
            f"{refusals}. This is a fact about mindex, not a harness failure — "
            f"but it belongs in the write-up, not only in this line."
        )

    if equivalence_problems:
        raise SystemExit(
            f"\nindex-state equivalence FAILED on {name}: "
            f"{len(equivalence_problems)} problem(s). The incremental index is "
            f"not the index a fresh run would build, so these numbers do not "
            f"describe mindex. Fix the divergence before scoring."
        )
    return out_path


def main() -> int:
    root = repo_root()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=root / "bench" / "corpora.toml")
    parser.add_argument(
        "--server-config",
        type=Path,
        default=root / "bench" / "bench-config.toml",
        help="the mindex config the bench server was started with",
    )
    parser.add_argument("--corpus", action="append", dest="corpora")
    parser.add_argument("--tier", type=int, default=0)
    parser.add_argument(
        "--label",
        default="baseline",
        help="names the configuration; part of the project GUID and the output file",
    )
    parser.add_argument("--limit", type=int, help="first N instances only (debugging)")
    parser.add_argument(
        "--equivalence-sample",
        type=int,
        help="override [run].equivalence_sample; 0 disables the check",
    )
    parser.add_argument(
        "--fresh", action="store_true", help="drop the project before starting"
    )
    parser.add_argument(
        "--qrels-suffix",
        default="",
        help='selects the query set: "" for the issue tier, "-docs" for the '
        "descriptive one",
    )
    parser.add_argument(
        "--run-tag",
        default="",
        help="suffix for the OUTPUT file only; the project GUID still comes "
        "from --label, so a tagged run re-queries the same index",
    )
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--seed", type=int, default=0)
    args = parser.parse_args()

    if not shutil.which("mindex-index"):
        raise SystemExit("mindex-index is not on PATH")

    config = load_config(args.config)
    server_config = tomllib.loads(args.server_config.read_text())
    config["run"]["db_path"] = server_config["database"]["path"]

    corpora = select_corpora(config, args.corpora, args.tier)
    if not corpora:
        raise SystemExit("no corpora selected")

    server = Server(config["run"]["server_url"], verify=not config["run"]["no_verify"])
    provenance = build_provenance(root, args.server_config, args.label, args.seed)
    try:
        provenance["mindex_version"] = server.version()
    except Exception as exc:
        raise SystemExit(
            f"bench server at {config['run']['server_url']} is not answering: {exc}"
        ) from exc

    if provenance["mindex_git_dirty"]:
        print("WARNING: src/ has uncommitted changes; the recorded git SHA is partial")

    equivalence = (
        args.equivalence_sample
        if args.equivalence_sample is not None
        else int(config["run"]["equivalence_sample"])
    )

    print(f"label={args.label}  mindex={provenance['mindex_version']} ")
    print(f"  git={provenance['mindex_git_sha'][:10]} ")
    print(f"  chunks_version={provenance['chunks_derivation_version']} ")
    print(f"  embedder={provenance['embedder_invocation']} ")
    print(f"  qdrant={provenance['qdrant_version']}")

    try:
        for corpus in corpora:
            clone_path = root / config["run"]["clone_dir"] / corpus["name"]
            with corpus_lock(clone_path):
                run_corpus(
                    corpus=corpus,
                    config=config,
                    root=root,
                    server=server,
                    provenance=provenance,
                    label=args.label,
                    limit=args.limit,
                    equivalence_sample=equivalence,
                    fresh=args.fresh,
                    concurrency=args.concurrency,
                    qrels_suffix=args.qrels_suffix,
                    run_tag=args.run_tag,
                )
    finally:
        server.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
