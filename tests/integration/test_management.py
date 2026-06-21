"""
Integration tests for the management endpoints:

  GET    /projects/{guid}          — stats (files by status, chunks by language)
  DELETE /projects/{guid}          — hard delete (rows + Qdrant collection), idempotent
  DELETE /projects/{guid}/files    — soft delete by include/exclude selector
  POST   /gc                       — forced, blocking garbage collection

Deletions are soft (mark + GC), so every deletion test calls POST /gc explicitly
and asserts the data is then physically gone (stats / search).
"""

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
