"""
Concurrency / interleaving tests for the indexing pipeline.

These exercise the per-file `IndexClaim` mutual exclusion and the recovery paths
under genuinely concurrent requests, plus the failure → recovery flows. The point
is to pin the invariants that make the various "stale write clobbers a later index"
interleavings impossible:

  * the per-file claim is held for the *whole* pipeline (prepare → embed →
    mark_indexed/recover), so a second same-file request gets 429 and never runs
    concurrently — it can only re-run after the first fully terminates;
  * regardless of how requests interleave, a file settles to exactly one terminal
    state with exactly one set of `active` chunks (no double-insert, no orphan);
  * an embed failure marks the file `failed` (chunks inserted, no vectors) and is
    later cleanly superseded by a reindex or recovered by the retry worker — a stale
    `failed` can never override a successful index.

Timing is widened with the mock embedder's `encode_delay_secs` knob (the
`embed_delay` fixture) so collisions are forced deterministically; embed failures
are injected with `fail_next_encodes` (the `embed_fail` fixture).
"""

import threading
import time
import uuid
from collections.abc import Callable

import httpx
from test_e2e import RUST_V1, RUST_V2
from test_management import index_files, run_gc, search, stats

MINDEX_URL = __import__("conftest").MINDEX_URL


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _files(client: httpx.Client, project: str) -> dict:
    """The `files`-by-status map from GET /projects/{guid}."""
    return stats(client, project).json()["files"]


def _rust_chunks(client: httpx.Client, project: str) -> dict:
    """The rust `chunks` counts (active/deleted), defaulting to zeros."""
    return (
        stats(client, project).json()["chunks"].get("rust", {"active": 0, "deleted": 0})
    )


def _baseline_active_chunks(client: httpx.Client, code: str) -> int:
    """Active rust chunk count from indexing `code` once into a throwaway project."""
    ctrl = uuid.uuid4().hex
    assert index_files(client, ctrl, {"rust": {"a.rs": code}}).status_code == 200
    return _rust_chunks(client, ctrl)["active"]


def _concurrent(n: int, work: Callable[[int], object]) -> list:
    """Run `work(i)` on `n` threads released simultaneously; collect results in order.

    A result is the value `work` returned, or the Exception it raised. A `Barrier`
    makes all threads fire at once so same-file requests actually collide.
    """
    barrier = threading.Barrier(n)
    results: list = [None] * n
    lock = threading.Lock()

    def runner(i: int) -> None:
        barrier.wait()
        try:
            r = work(i)
        except Exception as exc:  # noqa: BLE001 - recorded and asserted by caller
            r = exc
        with lock:
            results[i] = r

    threads = [threading.Thread(target=runner, args=(i,)) for i in range(n)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=60)
    assert all(not t.is_alive() for t in threads), "a concurrent request hung"
    return results


def _index_same(project: str, code: str) -> int:
    """Index one fixed-path file in a fresh client; return the HTTP status code."""
    with httpx.Client(verify=False, timeout=60.0) as c:
        return index_files(c, project, {"rust": {"src/a.rs": code}}).status_code


def _wait_for_indexing(
    client: httpx.Client, project: str, timeout: float = 10.0
) -> None:
    """Block until at least one file in the project is reported `indexing`."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        resp = stats(client, project)
        if resp.status_code == 200 and resp.json()["files"]["indexing"] >= 1:
            return
        time.sleep(0.05)
    raise AssertionError("no file entered 'indexing' within the timeout")


# ---------------------------------------------------------------------------
# Same-file concurrency — the claim serializes everything
# ---------------------------------------------------------------------------


def test_concurrent_same_file_one_winner_consistent(
    client: httpx.Client, project: str, embed_delay: Callable[[float], None]
) -> None:
    # Hold the winner's embed for 1s so the 5 losers collide on the held claim.
    embed_delay(1.0)
    statuses = _concurrent(6, lambda _i: _index_same(project, RUST_V1))
    embed_delay(0.0)

    # Every response is a clean 200 or 429 — never a 5xx from a concurrent clash.
    assert all(s in (200, 429) for s in statuses), statuses
    assert 200 in statuses, statuses
    # The claim is held across the whole pipeline, so the simultaneous siblings are
    # refused rather than running in parallel.
    assert 429 in statuses, statuses

    # Exactly one file, indexed, no failed/cancelled — a single consistent outcome.
    f = _files(client, project)
    assert f["indexed"] == 1, f
    assert f.get("failed", 0) == 0, f
    assert f.get("cancelled", 0) == 0, f
    assert search(client, project, "process records").status_code == 200


def test_concurrent_same_file_no_chunk_leak(
    client: httpx.Client, project: str, embed_delay: Callable[[float], None]
) -> None:
    # A storm of identical concurrent indexes must leave exactly one active chunk
    # set — no double-insert by the winner, no leak from the refused losers.
    baseline = _baseline_active_chunks(client, RUST_V1)

    embed_delay(0.8)
    statuses = _concurrent(6, lambda _i: _index_same(project, RUST_V1))
    embed_delay(0.0)
    assert all(s in (200, 429) for s in statuses), statuses

    run_gc(client)
    chunks = _rust_chunks(client, project)
    assert chunks["active"] == baseline, chunks
    assert chunks.get("deleted", 0) == 0, chunks


def test_concurrent_reindex_changing_content_settles(
    client: httpx.Client, project: str, embed_delay: Callable[[float], None]
) -> None:
    # Seed, then fire concurrent reindexes alternating unchanged (V1, hash-skipped)
    # and changed (V2) content at the same path. The claim serializes them; the file
    # must settle to one indexed state with one active set and no orphaned chunks.
    base_v1 = _baseline_active_chunks(client, RUST_V1)
    base_v2 = _baseline_active_chunks(client, RUST_V2)

    assert (
        index_files(client, project, {"rust": {"src/a.rs": RUST_V1}}).status_code == 200
    )

    embed_delay(0.6)
    statuses = _concurrent(
        6, lambda i: _index_same(project, RUST_V1 if i % 2 else RUST_V2)
    )
    embed_delay(0.0)
    assert all(s in (200, 429) for s in statuses), statuses

    run_gc(client)
    f = _files(client, project)
    assert f["indexed"] == 1, f
    assert f.get("failed", 0) == 0, f
    chunks = _rust_chunks(client, project)
    # Whichever content won last, there is exactly one active set and zero orphans.
    assert chunks["active"] in (base_v1, base_v2), chunks
    assert chunks.get("deleted", 0) == 0, chunks
    assert search(client, project, "process records").status_code == 200


# ---------------------------------------------------------------------------
# Distinct-file concurrency — the project-row creation race
# ---------------------------------------------------------------------------


def test_concurrent_distinct_files_new_project_all_succeed(
    client: httpx.Client, project: str
) -> None:
    # N first-time indexes of *distinct* paths into the same brand-new project: no
    # claim collision (different keys), and the project row is created exactly once
    # despite N racing `INSERT ... ON CONFLICT DO NOTHING`s — all must succeed.
    n = 6

    def work(i: int) -> int:
        with httpx.Client(verify=False, timeout=60.0) as c:
            return index_files(
                c, project, {"rust": {f"src/f{i}.rs": RUST_V1}}
            ).status_code

    statuses = _concurrent(n, work)
    assert all(s == 200 for s in statuses), statuses

    f = _files(client, project)
    assert f["indexed"] == n, f
    assert f.get("failed", 0) == 0, f


# ---------------------------------------------------------------------------
# Index racing a soft delete
# ---------------------------------------------------------------------------


def test_concurrent_index_and_delete_files(
    client: httpx.Client, project: str, embed_delay: Callable[[float], None]
) -> None:
    # DELETE /files (any → deleted) lands while a file is mid-embed. The delete does
    # not take the claim; the live index's mark_indexed is guarded by status =
    # 'indexing', so the file stays 'deleted' (not resurrected to 'indexed').
    embed_delay(2.5)

    def do_index() -> None:
        with httpx.Client(verify=False, timeout=30.0) as c:
            index_files(c, project, {"rust": {"src/a.rs": RUST_V1}})

    worker = threading.Thread(target=do_index)
    worker.start()
    try:
        _wait_for_indexing(client, project)
        resp = client.request(
            "DELETE",
            f"{MINDEX_URL}/projects/{project}/files",
            json={"include": {"paths": ["src/**"]}},
        )
        assert resp.status_code == 200, resp.text
        assert resp.json()["deleted_files"] == 1, resp.text
    finally:
        worker.join(timeout=30)
    assert not worker.is_alive(), "the /index request did not finish"
    embed_delay(0.0)

    f = _files(client, project)
    assert f.get("indexed", 0) == 0, f
    assert search(client, project, "process records").status_code == 404

    # GC physically reclaims the soft-deleted row + chunks.
    assert run_gc(client).status_code == 200
    final = stats(client, project).json()
    assert final["files"].get("deleted", 0) == 0, final
    assert final["chunks"].get("rust", {}).get("active", 0) == 0, final


# ---------------------------------------------------------------------------
# Embed failure → recovery (the original "stale failed" scenario)
# ---------------------------------------------------------------------------


def test_embed_failure_marks_failed_then_reindex_recovers(
    client: httpx.Client, project: str, embed_fail: Callable[[int], None]
) -> None:
    # First /encode fails → file 'failed' (chunks inserted, no vectors), so search is
    # empty (404). A reindex of the *same* content must NOT be hash-skipped (the skip
    # is gated on status='indexed'); it re-embeds back to 'indexed'.
    embed_fail(1)
    r = index_files(client, project, {"rust": {"src/a.rs": RUST_V1}})
    assert r.status_code == 503, r.text  # EmbedUpsertError::Embed → 503

    f = _files(client, project)
    assert f["failed"] == 1, f
    assert f.get("indexed", 0) == 0, f
    assert search(client, project, "process records").status_code == 404

    r2 = index_files(client, project, {"rust": {"src/a.rs": RUST_V1}})
    assert r2.status_code == 200, r2.text
    # Re-indexed, not treated as unchanged — the failed file is not trapped by its hash.
    assert r2.json()["files"]["rust"]["src/a.rs"] >= 1, r2.text

    f2 = _files(client, project)
    assert f2["indexed"] == 1, f2
    assert f2.get("failed", 0) == 0, f2
    assert search(client, project, "process records").status_code == 200


def test_failed_index_does_not_clobber_a_later_index(
    client: httpx.Client,
    project: str,
    embed_delay: Callable[[float], None],
    embed_fail: Callable[[int], None],
) -> None:
    # The exact "concurrent stale write" interleaving, made concrete:
    #   Request 1 goes in-flight then fails its embed (ends 'failed').
    #   Request 2 fires for the same file *while Request 1 holds the claim* — it is
    #   refused (429), so it can never run concurrently and there is no window for
    #   Request 1's later 'failed' write to race a Request 2 success.
    # After Request 1 finishes, a fresh reindex cleanly wins; the earlier 'failed'
    # state is gone and the file ends 'indexed' with one consistent chunk set.
    embed_delay(1.5)
    embed_fail(1)  # the in-flight embed (the only /encode in the window) will 503

    doomed: dict = {}

    def request_one() -> None:
        with httpx.Client(verify=False, timeout=30.0) as c:
            doomed["status"] = index_files(
                c, project, {"rust": {"src/a.rs": RUST_V1}}
            ).status_code

    worker = threading.Thread(target=request_one)
    worker.start()
    try:
        _wait_for_indexing(client, project)
        # Request 2 collides with the held claim while Request 1 is still embedding.
        with httpx.Client(verify=False, timeout=30.0) as c2:
            r2 = index_files(c2, project, {"rust": {"src/a.rs": RUST_V2}})
        assert r2.status_code == 429, r2.text
    finally:
        worker.join(timeout=30)
    assert not worker.is_alive(), "Request 1 did not finish"
    assert doomed["status"] == 503, doomed  # Request 1 failed its embed
    assert _files(client, project)["failed"] == 1

    # Now the path is free: reindex on fresh content supersedes the 'failed' state.
    embed_fail(0)
    embed_delay(0.0)
    r3 = index_files(client, project, {"rust": {"src/a.rs": RUST_V2}})
    assert r3.status_code == 200, r3.text

    f = _files(client, project)
    assert f["indexed"] == 1, f
    assert f.get("failed", 0) == 0, f

    run_gc(client)
    chunks = _rust_chunks(client, project)
    assert chunks["active"] >= 1, chunks
    assert chunks.get("deleted", 0) == 0, chunks  # the failed attempt's chunks are GC'd
    assert search(client, project, "process records").status_code == 200


def test_failed_file_recovered_by_retry_worker(
    client: httpx.Client, project: str, embed_fail: Callable[[int], None]
) -> None:
    # No reindex this time: a 'failed' file (chunks inserted, retry_count=1 < MAX) is
    # picked up by the retry worker, re-embedded, and reaches 'indexed' on its own.
    # Slow: a 'failed' row has a 60s cooldown (status_updated_at < now-60) *and* the
    # sweep runs every 60s, so worst-case recovery is ~120s when the cooldown boundary
    # just misses a tick. Poll well past two cycles so the test isn't phase-sensitive.
    embed_fail(1)
    assert (
        index_files(client, project, {"rust": {"src/a.rs": RUST_V1}}).status_code == 503
    )
    assert _files(client, project)["failed"] == 1

    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        f = _files(client, project)
        if f.get("indexed", 0) == 1 and f.get("failed", 0) == 0:
            break
        time.sleep(3)
    else:
        raise AssertionError(f"retry worker did not recover the failed file: {f}")

    assert search(client, project, "process records").status_code == 200
