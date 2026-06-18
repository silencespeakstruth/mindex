"""
Integration tests for POST /projects/{guid}/cancel — best-effort, transactional
cancellation of *in-flight* indexing, scoped by an include/exclude selector.

Cancel only ever touches files in `status='indexing'`: an already-indexed file is
left untouched (a too-late cancel is a no-op), and matched files move
`indexing → cancelled` with their chunks soft-deleted for GC. The "live" test widens
the in-flight window with the mock embedder's `encode_delay_secs` knob (the
`embed_delay` fixture) so the request can be caught mid-flight.
"""

import threading
import time
from collections.abc import Callable

import httpx
from test_e2e import RUST_V1, RUST_V2
from test_management import index_files, run_gc, search, stats

MINDEX_URL = __import__("conftest").MINDEX_URL


def cancel(
    client: httpx.Client,
    project: str,
    include: dict | None = None,
    exclude: dict | None = None,
) -> httpx.Response:
    body: dict = {}
    if include is not None:
        body["include"] = include
    if exclude is not None:
        body["exclude"] = exclude
    return client.post(f"{MINDEX_URL}/projects/{project}/cancel", json=body)


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
# Selector / no-op behaviour (no delay needed)
# ---------------------------------------------------------------------------


def test_cancel_empty_selector_is_rejected(client: httpx.Client, project: str) -> None:
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    # An empty body must not be allowed to blanket-cancel the whole project.
    assert cancel(client, project).status_code == 400


def test_cancel_indexed_file_is_noop(client: httpx.Client, project: str) -> None:
    # The file finishes indexing (no delay), so cancel matches nothing -> 204, and
    # the already-indexed content is preserved and still searchable.
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    assert search(client, project, "process records").status_code == 200

    resp = cancel(client, project, include={"paths": ["**"]})
    assert resp.status_code == 204  # nothing was 'indexing'

    after = stats(client, project).json()
    assert after["files"]["indexed"] == 1, after
    assert after["files"]["cancelled"] == 0, after
    assert search(client, project, "process records").status_code == 200


# ---------------------------------------------------------------------------
# Live cancellation of an in-flight index (mid-embed)
# ---------------------------------------------------------------------------


def test_cancel_live_indexing_file(
    client: httpx.Client, project: str, embed_delay: Callable[[float], None]
) -> None:
    embed_delay(3.0)  # hold each /encode for 3s so the file lingers in 'indexing'

    def do_index() -> None:
        with httpx.Client(verify=False, timeout=30.0) as c:
            index_files(c, project, {"rust": {"src/a.rs": RUST_V1}})

    worker = threading.Thread(target=do_index)
    worker.start()
    try:
        _wait_for_indexing(client, project)

        resp = cancel(client, project, include={"paths": ["src/**"]})
        assert resp.status_code == 200, resp.text
        assert resp.json()["cancelled_files"] == 1
    finally:
        worker.join(timeout=30)
    assert not worker.is_alive(), "the /index request did not finish"

    # The file is 'cancelled' (the racing mark_indexed -> cancelled→indexed was
    # rejected by the state machine), not 'indexed', and its chunks are soft-deleted.
    after = stats(client, project).json()
    assert after["files"]["cancelled"] == 1, after
    assert after["files"]["indexed"] == 0, after
    assert after["chunks"].get("rust", {}).get("active", 0) == 0, after

    # No active chunks -> search returns nothing; GC then physically reclaims any
    # vectors a racing embed upserted before the cancel landed.
    assert search(client, project, "process records").status_code == 404
    assert run_gc(client).status_code == 200
    final = stats(client, project).json()
    assert final["chunks"].get("rust", {}).get("deleted", 0) == 0, final


def test_cancel_live_respects_selector(
    client: httpx.Client, project: str, embed_delay: Callable[[float], None]
) -> None:
    # Two in-flight files; cancel only the one matching the selector, the other
    # finishes indexing normally once the delay clears.
    embed_delay(3.0)

    def do_index() -> None:
        with httpx.Client(verify=False, timeout=30.0) as c:
            index_files(
                c, project, {"rust": {"keep/a.rs": RUST_V1, "drop/b.rs": RUST_V1}}
            )

    worker = threading.Thread(target=do_index)
    worker.start()
    try:
        _wait_for_indexing(client, project)
        resp = cancel(client, project, include={"paths": ["drop/**"]})
        assert resp.status_code == 200, resp.text
        assert resp.json()["cancelled_files"] == 1
    finally:
        worker.join(timeout=30)
    assert not worker.is_alive(), "the /index request did not finish"

    # Both files share one /index batch: cancelling drop/b.rs mid-embed must not
    # poison the batch — keep/a.rs still reaches 'indexed' (mark_indexed skips the
    # cancelled row rather than erroring), while drop/b.rs ends 'cancelled'.
    after = stats(client, project).json()
    assert after["files"]["cancelled"] == 1, after
    assert after["files"]["indexed"] == 1, after
    assert search(client, project, "process records").status_code == 200


def test_cancel_then_reindex_fresh_content(
    client: httpx.Client, project: str, embed_delay: Callable[[float], None]
) -> None:
    # The motivating workflow: a stale in-flight index is cancelled, then the path is
    # re-indexed on fresh content. `cancelled → indexing` is legal, so the re-push
    # cleanly resurrects the file to 'indexed' (no leftover 'cancelled' state).
    embed_delay(3.0)

    def do_index(code: str) -> None:
        with httpx.Client(verify=False, timeout=30.0) as c:
            index_files(c, project, {"rust": {"src/a.rs": code}})

    worker = threading.Thread(target=do_index, args=(RUST_V1,))
    worker.start()
    try:
        _wait_for_indexing(client, project)
        assert cancel(client, project, include={"paths": ["src/**"]}).status_code == 200
    finally:
        worker.join(timeout=30)
    assert stats(client, project).json()["files"]["cancelled"] == 1

    # Re-index the same path with changed content (different hash, so not skipped).
    embed_delay(0.0)
    resp = index_files(client, project, {"rust": {"src/a.rs": RUST_V2}})
    assert resp.status_code == 200, resp.text

    after = stats(client, project).json()
    assert after["files"]["indexed"] == 1, after
    assert after["files"]["cancelled"] == 0, after
    assert search(client, project, "process records").status_code == 200
