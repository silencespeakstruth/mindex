"""
Integration tests for the management endpoints:

  GET    /projects/{guid}          — stats (files by status, chunks by language)
  GET    /projects/{guid}/files    — per-file listing (status/lang/hash/chunks/retries)
  POST   /projects/{guid}/retry    — requeue failed files (reset retry_count)
  DELETE /projects/{guid}          — hard delete (rows + Qdrant collection), idempotent
  DELETE /projects/{guid}/files    — soft delete by include/exclude selector
  POST   /gc                       — forced, blocking garbage collection
  GET    /status                   — live runtime/concurrency state
  GET    /config                   — static capabilities + tuning knobs

Deletions are soft (mark + GC), so every deletion test calls POST /gc explicitly
and asserts the data is then physically gone (stats / search).
"""

import threading
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor

import httpx

from test_e2e import RUST_V1, RUST_V2
from test_filters_and_languages import PYTHON_SRC

MINDEX_URL = __import__("conftest").MINDEX_URL


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def index_files(
    client: httpx.Client, project: str, files: dict[str, dict[str, str]]
) -> httpx.Response:
    """files = {language: {path: code}}."""
    body = {
        "files": {
            lang: {path: {"code": code} for path, code in paths.items()}
            for lang, paths in files.items()
        }
    }
    return client.post(f"{MINDEX_URL}/v0/{project}/index", json=body)


def stats(client: httpx.Client, project: str) -> httpx.Response:
    return client.get(f"{MINDEX_URL}/projects/{project}")


def run_gc(client: httpx.Client) -> httpx.Response:
    return client.post(f"{MINDEX_URL}/gc")


def delete_project(client: httpx.Client, project: str) -> httpx.Response:
    return client.delete(f"{MINDEX_URL}/projects/{project}")


def delete_files(
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
    # DELETE with a JSON body: globs do not fit in the path, so the selector is sent
    # as a body (httpx's .delete() has no json= kwarg, hence .request()).
    return client.request("DELETE", f"{MINDEX_URL}/projects/{project}/files", json=body)


def search(client: httpx.Client, project: str, query: str) -> httpx.Response:
    return client.post(f"{MINDEX_URL}/v0/{project}/search", json={"query": query})


def list_files(
    client: httpx.Client,
    project: str,
    status: str | None = None,
    language: str | None = None,
) -> httpx.Response:
    params = {}
    if status is not None:
        params["status"] = status
    if language is not None:
        params["language"] = language
    return client.get(f"{MINDEX_URL}/projects/{project}/files", params=params)


def retry_files(
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
    return client.post(f"{MINDEX_URL}/projects/{project}/retry", json=body)


# ---------------------------------------------------------------------------
# GET /projects/{guid} — stats
# ---------------------------------------------------------------------------


def test_stats_aggregates_by_status_and_language(
    client: httpx.Client, project: str
) -> None:
    index_files(
        client, project, {"rust": {"a.rs": RUST_V1}, "python": {"b.py": PYTHON_SRC}}
    )
    resp = stats(client, project)
    assert resp.status_code == 200
    body = resp.json()
    assert body["files"]["indexed"] == 2
    assert body["chunks"]["rust"]["active"] >= 1
    assert body["chunks"]["python"]["active"] >= 1
    assert body["chunks"]["rust"]["deleted"] == 0


def test_stats_404_for_unknown_project(client: httpx.Client, project: str) -> None:
    assert stats(client, project).status_code == 404


def test_projects_list_includes_indexed_project(
    client: httpx.Client, project: str
) -> None:
    # Absent before indexing (other tests' projects may be present — assert on ours).
    before = client.get(f"{MINDEX_URL}/projects").json()["projects"]
    assert all(p["project_guid"] != project for p in before)

    index_files(client, project, {"rust": {"a.rs": RUST_V1}})

    after = {
        p["project_guid"]: p
        for p in client.get(f"{MINDEX_URL}/projects").json()["projects"]
    }
    assert project in after, after
    assert after[project]["files"] >= 1
    assert after[project]["active_chunks"] >= 1


def test_reindex_shows_deleted_chunks_until_gc(
    client: httpx.Client, project: str
) -> None:
    # First index, then reindex the same path with different content: the old chunks
    # are soft-deleted (append-only), so they show as deleted until GC purges them.
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    index_files(client, project, {"rust": {"a.rs": RUST_V2}})

    before = stats(client, project).json()
    assert before["chunks"]["rust"]["deleted"] >= 1, before
    assert before["chunks"]["rust"]["active"] >= 1, before

    assert run_gc(client).status_code == 200

    after = stats(client, project).json()
    assert after["chunks"]["rust"]["deleted"] == 0, after
    assert after["chunks"]["rust"]["active"] >= 1, after


# ---------------------------------------------------------------------------
# DELETE /projects/{guid}
# ---------------------------------------------------------------------------


def test_delete_project_is_hard_and_idempotent(
    client: httpx.Client, project: str
) -> None:
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    assert search(client, project, "process records").status_code == 200

    assert delete_project(client, project).status_code == 204
    # Gone: stats 404 and search 404 (collection dropped, rows removed).
    assert stats(client, project).status_code == 404
    assert search(client, project, "process records").status_code == 404

    # Idempotent: deleting again (and a never-seen project) is still 204.
    assert delete_project(client, project).status_code == 204


# ---------------------------------------------------------------------------
# DELETE /projects/{guid}/files
# ---------------------------------------------------------------------------


def test_delete_files_by_path_glob_then_gc(client: httpx.Client, project: str) -> None:
    index_files(client, project, {"rust": {"src/a.rs": RUST_V1, "tests/b.rs": RUST_V2}})

    resp = delete_files(client, project, include={"paths": ["tests/**"]})
    assert resp.status_code == 200
    assert resp.json()["deleted_files"] == 1

    # Soft until GC: the file is 'deleted', its chunks 'deleted', the kept file active.
    before = stats(client, project).json()
    assert before["files"]["deleted"] == 1, before
    assert before["files"]["indexed"] == 1, before
    assert before["chunks"]["rust"]["deleted"] >= 1, before

    assert run_gc(client).status_code == 200

    # Physically gone: the deleted file row is removed, only src/a.rs survives.
    after = stats(client, project).json()
    assert after["files"]["deleted"] == 0, after
    assert after["files"]["indexed"] == 1, after
    assert after["chunks"]["rust"]["deleted"] == 0, after
    results = search(client, project, "process records").json()["results"]
    assert all(r["path"] == "src/a.rs" for r in results), [r["path"] for r in results]


def test_delete_files_by_language(client: httpx.Client, project: str) -> None:
    index_files(
        client, project, {"rust": {"a.rs": RUST_V1}, "python": {"b.py": PYTHON_SRC}}
    )
    resp = delete_files(client, project, include={"programming_languages": ["python"]})
    assert resp.status_code == 200
    assert resp.json()["deleted_files"] == 1

    run_gc(client)
    after = stats(client, project).json()
    assert "python" not in after["chunks"] or after["chunks"]["python"]["active"] == 0
    assert after["chunks"]["rust"]["active"] >= 1


def test_delete_files_no_match_is_204(client: httpx.Client, project: str) -> None:
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    resp = delete_files(client, project, include={"paths": ["does/not/exist/**"]})
    assert resp.status_code == 204


def test_delete_files_empty_selector_is_rejected(
    client: httpx.Client, project: str
) -> None:
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    # An empty body must not be allowed to wipe the whole project.
    resp = client.request("DELETE", f"{MINDEX_URL}/projects/{project}/files", json={})
    assert resp.status_code == 400


# ---------------------------------------------------------------------------
# POST /gc
# ---------------------------------------------------------------------------


def test_gc_returns_counts(client: httpx.Client, project: str) -> None:
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    delete_files(client, project, include={"paths": ["**"]})

    resp = run_gc(client)
    assert resp.status_code == 200
    body = resp.json()
    assert body["chunks_removed"] >= 1
    assert body["files_removed"] >= 1


def test_concurrent_gc_is_serialized(client: httpx.Client, project: str) -> None:
    """GC is global, so a pass is serialized process-wide by GcGuard: of N
    concurrent POST /gc, exactly the ones that win the flag return 200+counts and
    the rest get 409 — never a 5xx, never overlapping sweeps. The race may resolve
    so fast that all serialize cleanly (all 200), so we don't *require* a 409; we
    pin the invariant that every response is 200-or-409 and the 200s carry a body.
    (The strict win/lose semantics are unit-tested in `gc_guard_serializes_and_releases`.)
    """
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    delete_files(client, project, include={"paths": ["**"]})

    with ThreadPoolExecutor(max_workers=8) as pool:
        responses = [
            r.result() for r in [pool.submit(run_gc, client) for _ in range(8)]
        ]

    assert all(r.status_code in (200, 409) for r in responses)
    assert any(r.status_code == 200 for r in responses)
    for r in responses:
        if r.status_code == 200:
            assert set(r.json()) == {
                "chunks_removed",
                "files_removed",
                "status_log_pruned",
            }


# ---------------------------------------------------------------------------
# GET /version and GET /health
# ---------------------------------------------------------------------------


def test_version_reports_a_version(client: httpx.Client) -> None:
    resp = client.get(f"{MINDEX_URL}/version")
    assert resp.status_code == 200
    assert isinstance(resp.json()["version"], str)
    assert resp.json()["version"]


def test_health_checks_all_dependencies(client: httpx.Client) -> None:
    resp = client.get(f"{MINDEX_URL}/health")
    assert resp.status_code == 200
    body = resp.json()
    # All three dependencies are up in the test stack (SQLite, Qdrant, mock embedder).
    assert body["status"] == "ok", body
    assert body["checks"] == {"sqlite": "ok", "qdrant": "ok", "embedder": "ok"}, body
    assert isinstance(body["indexing_files"], int)


# ---------------------------------------------------------------------------
# GET /projects/{guid}/files
# ---------------------------------------------------------------------------


def test_files_lists_metadata_and_filters(client: httpx.Client, project: str) -> None:
    index_files(
        client, project, {"rust": {"a.rs": RUST_V1}, "python": {"b.py": PYTHON_SRC}}
    )

    resp = list_files(client, project)
    assert resp.status_code == 200
    files = {f["path"]: f for f in resp.json()["files"]}
    assert set(files) == {"a.rs", "b.py"}, files
    a = files["a.rs"]
    assert a["status"] == "indexed"
    assert a["programming_language"] == "rust"
    assert a["chunk_count"] >= 1
    assert a["retry_count"] == 0
    assert len(a["sha256"]) == 64
    assert isinstance(a["status_updated_at"], int)

    # Filters: by status and by language.
    indexed = list_files(client, project, status="indexed").json()["files"]
    assert {f["path"] for f in indexed} == {"a.rs", "b.py"}
    assert list_files(client, project, status="failed").json()["files"] == []
    rust_only = list_files(client, project, language="rust").json()["files"]
    assert {f["path"] for f in rust_only} == {"a.rs"}


def test_files_404_for_unknown_project(client: httpx.Client, project: str) -> None:
    assert list_files(client, project).status_code == 404


# ---------------------------------------------------------------------------
# POST /projects/{guid}/retry
# ---------------------------------------------------------------------------


def test_retry_requeues_failed_file(
    client: httpx.Client, project: str, embed_fail: Callable[[int], None]
) -> None:
    # Drive a file to 'failed' by failing its embed; the /index call then returns 503.
    embed_fail(1)
    assert index_files(client, project, {"rust": {"a.rs": RUST_V1}}).status_code == 503

    failed = list_files(client, project, status="failed").json()["files"]
    assert len(failed) == 1, failed
    assert failed[0]["retry_count"] >= 1

    # Requeue (empty selector = all failed): retry_count is reset to 0. status stays
    # 'failed' until the retry worker (next sweep) re-embeds it.
    resp = retry_files(client, project)
    assert resp.status_code == 200
    assert resp.json()["requeued_files"] == 1

    after = list_files(client, project, status="failed").json()["files"]
    assert len(after) == 1, after
    assert after[0]["retry_count"] == 0


def test_retry_no_failed_files_is_204(client: httpx.Client, project: str) -> None:
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    assert retry_files(client, project).status_code == 204


# ---------------------------------------------------------------------------
# GET /status and GET /config
# ---------------------------------------------------------------------------


def test_status_reports_runtime_state(client: httpx.Client, project: str) -> None:
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    body = client.get(f"{MINDEX_URL}/status").json()
    assert set(body) == {
        "indexing_claims",
        "gc_running",
        "pool_available",
        "pool_size",
        "indexing_files",
        "files_by_status",
    }
    assert body["pool_size"] >= 1
    assert 0 <= body["pool_available"] <= body["pool_size"]
    assert isinstance(body["gc_running"], bool)
    assert body["files_by_status"]["indexed"] >= 1


def test_config_lists_languages_and_knobs(client: httpx.Client) -> None:
    body = client.get(f"{MINDEX_URL}/config").json()
    assert "rust" in body["languages"]
    assert "python" in body["languages"]
    assert isinstance(body["model_id"], str) and body["model_id"]
    assert body["max_retries"] >= 1
    assert body["db_pool_size"] >= 1
    assert isinstance(body["embed_batch"], int)
    assert isinstance(body["stuck_grace_mins"], int)


# ---------------------------------------------------------------------------
# DELETE /projects/{guid}/files — exclude-only selector
# ---------------------------------------------------------------------------


def test_delete_files_exclude_only_selector(client: httpx.Client, project: str) -> None:
    # An exclude-only body (no include) is a valid non-empty selector: it deletes
    # every file that does NOT match the exclusion.
    index_files(
        client,
        project,
        {"rust": {"src/keep.rs": RUST_V1, "tests/drop.rs": RUST_V2}},
    )

    # Exclude src/**: only tests/drop.rs survives the filter, so it gets deleted.
    resp = delete_files(client, project, exclude={"paths": ["src/**"]})
    assert resp.status_code == 200, resp.text
    assert resp.json()["deleted_files"] == 1

    assert run_gc(client).status_code == 200

    after = stats(client, project).json()
    assert after["files"]["indexed"] == 1, after
    assert after["files"].get("deleted", 0) == 0, after

    # The surviving file is src/keep.rs; tests/drop.rs is gone.
    files = {f["path"]: f for f in list_files(client, project).json()["files"]}
    assert set(files) == {"src/keep.rs"}, files


# ---------------------------------------------------------------------------
# POST /projects/{guid}/retry — scoped selector
# ---------------------------------------------------------------------------


def test_retry_with_path_selector_requeues_only_matching_files(
    client: httpx.Client, project: str, embed_fail: Callable[[int], None]
) -> None:
    # Fail 2 files in one batch, then retry only one of them by path selector.
    embed_fail(2)
    assert (
        index_files(
            client,
            project,
            {"rust": {"src/a.rs": RUST_V1, "src/b.rs": RUST_V2}},
        ).status_code
        == 503
    )

    failed = list_files(client, project, status="failed").json()["files"]
    assert len(failed) == 2, failed
    assert all(f["retry_count"] >= 1 for f in failed), failed

    # Requeue only src/a.rs; src/b.rs must keep its elevated retry_count.
    resp = retry_files(client, project, include={"paths": ["src/a.rs"]})
    assert resp.status_code == 200, resp.text
    assert resp.json()["requeued_files"] == 1

    files_after = {
        f["path"]: f
        for f in list_files(client, project, status="failed").json()["files"]
    }
    assert files_after["src/a.rs"]["retry_count"] == 0, files_after
    assert files_after["src/b.rs"]["retry_count"] >= 1, files_after


# ---------------------------------------------------------------------------
# GET /projects/{guid}/files — combined status + language filter
# ---------------------------------------------------------------------------


def test_list_files_combined_status_and_language_filter(
    client: httpx.Client, project: str
) -> None:
    index_files(
        client,
        project,
        {"rust": {"a.rs": RUST_V1}, "python": {"b.py": PYTHON_SRC}},
    )
    # ?status=indexed&language=rust must return only a.rs, not b.py.
    resp = list_files(client, project, status="indexed", language="rust")
    assert resp.status_code == 200
    paths = {f["path"] for f in resp.json()["files"]}
    assert paths == {"a.rs"}, paths

    # ?status=failed&language=rust — nothing matches.
    empty = list_files(client, project, status="failed", language="rust").json()[
        "files"
    ]
    assert empty == [], empty


# ---------------------------------------------------------------------------
# GET /status — indexing_claims counter during live indexing
# ---------------------------------------------------------------------------


def test_status_shows_indexing_claims_during_live_embed(
    client: httpx.Client, project: str, embed_delay: Callable[[float], None]
) -> None:
    # Hold the embed long enough to read /status mid-flight and observe a claim.
    embed_delay(3.0)

    def do_index() -> None:
        with httpx.Client(verify=False, timeout=30.0) as c:
            index_files(c, project, {"rust": {"src/a.rs": RUST_V1}})

    worker = threading.Thread(target=do_index)
    worker.start()
    try:
        # Poll until the file enters 'indexing', then check /status.
        import time

        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline:
            s = stats(client, project)
            if s.status_code == 200 and s.json()["files"]["indexing"] >= 1:
                break
            time.sleep(0.05)

        body = client.get(f"{MINDEX_URL}/status").json()
        assert body["indexing_claims"] >= 1, body
        assert body["indexing_files"] >= 1, body
        assert body["files_by_status"]["indexing"] >= 1, body
    finally:
        worker.join(timeout=30)
    assert not worker.is_alive()
    embed_delay(0.0)
