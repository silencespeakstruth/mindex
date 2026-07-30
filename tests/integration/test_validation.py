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


def test_bad_uuid_path_on_management_endpoint(client: httpx.Client) -> None:
    # The ApiPath extractor wraps every route: management endpoints must also return
    # request.malformed_path, not a plain-text 400.
    resp = client.get(f"{MINDEX_URL}/projects/not-a-uuid")
    assert_problem(resp, 400, "request.malformed_path")

    resp2 = client.request(
        "DELETE",
        f"{MINDEX_URL}/projects/not-a-uuid/files",
        json={"include": {"paths": ["**"]}},
    )
    assert_problem(resp2, 400, "request.malformed_path")


# ── query length cap ───────────────────────────────────────────────────────────


def test_query_too_long_rejected(client: httpx.Client, project: str) -> None:
    # Default max_query_bytes = 32768; send one byte over the cap.
    resp = search(client, project, {"query": "a" * 32769})
    assert_problem(resp, 400, "validation.query_too_long")
    meta = resp.json()["meta"]
    assert meta["max"] == 32768


# ── selector size cap ──────────────────────────────────────────────────────────


def test_selector_too_large_on_delete_files(client: httpx.Client, project: str) -> None:
    # Default max_selector_patterns = 256; send 257 path globs to cross the cap.
    too_many = [f"src/f{i}/**" for i in range(257)]
    resp = client.request(
        "DELETE",
        f"{MINDEX_URL}/projects/{project}/files",
        json={"include": {"paths": too_many}},
    )
    assert_problem(resp, 400, "validation.selector_too_large")
    meta = resp.json()["meta"]
    assert meta["max"] == 256


def test_selector_too_large_on_cancel(client: httpx.Client, project: str) -> None:
    too_many = [f"src/f{i}/**" for i in range(257)]
    resp = client.post(
        f"{MINDEX_URL}/projects/{project}/cancel",
        json={"include": {"paths": too_many}},
    )
    assert_problem(resp, 400, "validation.selector_too_large")


# ── selector emptiness on /cancel ─────────────────────────────────────────────


def test_cancel_empty_selector_problem_json(client: httpx.Client, project: str) -> None:
    # Empty body (no include/exclude) on POST /cancel returns selector.empty, matching
    # the same rule enforced on DELETE /files and POST /cancel.
    resp = client.post(f"{MINDEX_URL}/projects/{project}/cancel", json={})
    assert_problem(resp, 400, "selector.empty")


# ── malformed body on /index ──────────────────────────────────────────────────


def test_malformed_json_body_on_index(client: httpx.Client, project: str) -> None:
    # ApiJson rejects invalid JSON on every endpoint — not just /search.
    resp = client.post(
        f"{MINDEX_URL}/v0/{project}/index",
        content=b"{bad",
        headers={"content-type": "application/json"},
    )
    assert_problem(resp, 400, "request.malformed_body")


# ── drift path validation ──────────────────────────────────────────────────────


def test_drift_path_invalid_rejected(client: httpx.Client, project: str) -> None:
    # The drift validator runs validate_path before validate_sha256_hex.
    # A traversal path must return path_invalid, not sha256_invalid.
    good_sha = "a" * 64
    resp = client.post(
        f"{MINDEX_URL}/projects/{project}/drift",
        json={"files": {"../escape.rs": good_sha}},
    )
    assert_problem(resp, 400, "validation.path_invalid")


# ── request-shape limits ([limits] / [server], mindex-test-config.toml) ───────
# The test stack mounts a config that shrinks the TOML-only limits so these caps
# can be tripped cheaply: max_code_bytes = 64 KiB, max_files_per_request = 50,
# max_drift_files = 50, max_body_mib = 2.


def test_code_too_large_rejected(client: httpx.Client, project: str) -> None:
    big = "// filler\n" * 7_000  # ~70 KiB, over the 64 KiB cap
    resp = index_files(client, project, {"rust": {"src/big.rs": {"code": big}}})
    assert_problem(resp, 400, "validation.code_too_large")
    assert resp.json()["meta"]["max"] == 65536


def test_too_many_files_rejected(client: httpx.Client, project: str) -> None:
    files = {f"src/f{i}.rs": {"code": "fn x() {}"} for i in range(51)}
    resp = index_files(client, project, {"rust": files})
    assert_problem(resp, 400, "validation.too_many_files")


def test_drift_too_many_files_rejected(client: httpx.Client, project: str) -> None:
    manifest = {f"src/f{i}.rs": "a" * 64 for i in range(51)}
    resp = client.post(
        f"{MINDEX_URL}/projects/{project}/drift", json={"files": manifest}
    )
    assert_problem(resp, 400, "validation.too_many_files")


def test_oversized_body_returns_413_problem_json(
    client: httpx.Client, project: str
) -> None:
    # ~3 MiB body against the 2 MiB cap: DefaultBodyLimit rejects it before any
    # handler runs, and the rejection must still be the problem+json envelope.
    big = "x" * (3 * 1024 * 1024)
    resp = index_files(client, project, {"rust": {"src/big.rs": {"code": big}}})
    assert_problem(resp, 413, "request.body_too_large")
