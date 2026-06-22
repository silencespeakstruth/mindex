"""
Request validation + the RFC 7807 error envelope.

Every rejection should be ``application/problem+json`` carrying a stable, namespaced
``code`` (the localization key) — not an opaque status with an empty/plain body. These
checks run before any embedder/Qdrant work, so they need only a live mindex.
"""

import httpx

MINDEX_URL = __import__("conftest").MINDEX_URL


def assert_problem(resp: httpx.Response, status: int, code: str) -> None:
    """Assert the response is the documented RFC 7807 problem with ``code``."""
    content_type = resp.headers["content-type"]
    body = resp.json()
    assert resp.status_code == status
    assert content_type.startswith("application/problem+json")
    assert body["code"] == code
    assert body["status"] == status
    assert body["type"].endswith(code)


def index_files(client: httpx.Client, project: str, files: dict) -> httpx.Response:
    return client.post(f"{MINDEX_URL}/v0/{project}/index", json={"files": files})


def search(client: httpx.Client, project: str, payload: dict) -> httpx.Response:
    return client.post(f"{MINDEX_URL}/v0/{project}/search", json=payload)


# ── path / content validation (was an opaque 500 from a SQLite CHECK) ──────────


def test_absolute_path_rejected(client: httpx.Client, project: str) -> None:
    resp = index_files(
        client, project, {"rust": {"/etc/passwd": {"code": "fn x() {}"}}}
    )
    assert_problem(resp, 400, "validation.path_invalid")


def test_traversal_path_rejected(client: httpx.Client, project: str) -> None:
    resp = index_files(
        client, project, {"rust": {"../escape.rs": {"code": "fn x() {}"}}}
    )
    assert_problem(resp, 400, "validation.path_invalid")


# ── search bounds ──────────────────────────────────────────────────────────────


def test_top_k_over_cap_rejected(client: httpx.Client, project: str) -> None:
    resp = search(client, project, {"query": "x", "top_k": 1_000_000})
    assert_problem(resp, 400, "validation.top_k_out_of_range")
    assert resp.json()["meta"]["max"] >= 1


def test_empty_query_rejected(client: httpx.Client, project: str) -> None:
    resp = search(client, project, {"query": ""})
    assert_problem(resp, 400, "validation.query_empty")


# ── selector emptiness on the destructive management endpoints ──────────────────


def test_delete_files_empty_selector_rejected(
    client: httpx.Client, project: str
) -> None:
    resp = client.request("DELETE", f"{MINDEX_URL}/projects/{project}/files", json={})
    assert_problem(resp, 400, "selector.empty")


# ── drift sha256 format ──────────────────────────────────────────────────────────


def test_drift_bad_sha_rejected(client: httpx.Client, project: str) -> None:
    resp = client.post(
        f"{MINDEX_URL}/projects/{project}/drift",
        json={"files": {"src/a.rs": "not-a-real-sha"}},
    )
    assert_problem(resp, 400, "validation.sha256_invalid")


# ── extractor rejections also carry a code ──────────────────────────────────────


def test_malformed_json_body(client: httpx.Client, project: str) -> None:
    resp = client.post(
        f"{MINDEX_URL}/v0/{project}/search",
        content=b"{not valid json",
        headers={"content-type": "application/json"},
    )
    assert_problem(resp, 400, "request.malformed_body")


def test_bad_uuid_path(client: httpx.Client) -> None:
    resp = client.post(
        f"{MINDEX_URL}/v0/not-a-uuid/search",
        json={"query": "x"},
    )
    assert_problem(resp, 400, "request.malformed_path")
