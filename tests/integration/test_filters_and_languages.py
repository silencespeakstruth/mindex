"""
Integration tests for multi-language indexing and the search include/exclude
filters (programming languages + path GLOBs).

Complements test_e2e.py (which covers the rust-only happy path). Fixtures
(client, project, wait_for_mindex) come from conftest.py.
"""

import httpx

from test_e2e import RUST_V1, RUST_V2  # reuse the validated rust snippets

MINDEX_URL = __import__("conftest").MINDEX_URL

# ---------------------------------------------------------------------------
# Fixtures: non-rust source snippets, each validated to produce >= 1 chunk
# (128-512 BGE-M3 tokens) by the real tree-sitter slicer.
# ---------------------------------------------------------------------------

PYTHON_SRC = """\
def process_records(records, config, output):
    stats = {"processed": 0, "batches": 0, "retries": 0}
    batch_size = config.get("batch_size", 64)
    max_retries = config.get("max_retries", 3)
    for batch_idx, batch in enumerate(chunked(records, batch_size)):
        attempt = 0
        while True:
            try:
                processed = process_batch(batch, config)
                output.extend(processed)
                stats["processed"] += len(batch)
                stats["batches"] += 1
                break
            except TransientError as err:
                if attempt < max_retries:
                    attempt += 1
                    stats["retries"] += 1
                    logging.warning("retry batch %s attempt %s: %s", batch_idx, attempt, err)
                    continue
                raise PipelineError(batch_index=batch_idx) from err
    return stats
"""

SQL_SRC = """\
CREATE TABLE analytics_events (
    id BIGINT PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
    event_uuid UUID NOT NULL UNIQUE,
    session_id UUID NOT NULL,
    user_id BIGINT REFERENCES users (id) ON DELETE SET NULL,
    event_type TEXT NOT NULL CHECK (event_type IN ('click', 'view', 'purchase', 'signup')),
    payload JSONB NOT NULL DEFAULT '{}',
    ip_address INET,
    user_agent TEXT,
    referrer TEXT,
    country_code CHAR(2),
    region TEXT,
    city TEXT,
    latitude DOUBLE PRECISION,
    longitude DOUBLE PRECISION,
    device_type TEXT,
    os_name TEXT,
    os_version TEXT,
    browser_name TEXT,
    browser_version TEXT,
    screen_width INTEGER,
    screen_height INTEGER,
    is_bot BOOLEAN NOT NULL DEFAULT FALSE,
    revenue_cents BIGINT NOT NULL DEFAULT 0,
    currency CHAR(3) NOT NULL DEFAULT 'USD',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    processed_at TIMESTAMPTZ
);
"""


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


def search(
    client: httpx.Client,
    project: str,
    query: str,
    top_k: int = 10,
    include: dict | None = None,
    exclude: dict | None = None,
) -> httpx.Response:
    body: dict = {"query": query, "top_k": top_k}
    if include is not None:
        body["include"] = include
    if exclude is not None:
        body["exclude"] = exclude
    return client.post(f"{MINDEX_URL}/v0/{project}/search", json=body)


# ---------------------------------------------------------------------------
# Non-rust languages
# ---------------------------------------------------------------------------


def test_index_python_file_returns_chunks(client: httpx.Client, project: str) -> None:
    resp = index_files(client, project, {"python": {"app.py": PYTHON_SRC}})
    assert resp.status_code == 200
    assert resp.json()["files"]["python"]["app.py"] >= 1


def test_index_sql_file_returns_chunks(client: httpx.Client, project: str) -> None:
    resp = index_files(client, project, {"sql": {"schema.sql": SQL_SRC}})
    assert resp.status_code == 200
    assert resp.json()["files"]["sql"]["schema.sql"] >= 1


def test_search_finds_python_content(client: httpx.Client, project: str) -> None:
    index_files(client, project, {"python": {"app.py": PYTHON_SRC}})
    resp = search(client, project, "process records batch retry")
    assert resp.status_code == 200
    results = resp.json()["results"]
    assert len(results) >= 1
    assert any(r["path"] == "app.py" for r in results)
    assert any("process_records" in r["code"] for r in results)


def test_search_results_sorted_by_score_desc(
    client: httpx.Client, project: str
) -> None:
    index_files(
        client,
        project,
        {"rust": {"a.rs": RUST_V1, "b.rs": RUST_V2}, "python": {"c.py": PYTHON_SRC}},
    )
    resp = search(client, project, "process records batch", top_k=10)
    assert resp.status_code == 200
    scores = [r["score"] for r in resp.json()["results"]]
    assert scores == sorted(scores, reverse=True), scores


def test_multi_language_single_request(client: httpx.Client, project: str) -> None:
    resp = index_files(
        client,
        project,
        {
            "rust": {"a.rs": RUST_V1},
            "python": {"b.py": PYTHON_SRC},
            "sql": {"c.sql": SQL_SRC},
        },
    )
    assert resp.status_code == 200
    files = resp.json()["files"]
    assert files["rust"]["a.rs"] >= 1
    assert files["python"]["b.py"] >= 1
    assert files["sql"]["c.sql"] >= 1


# ---------------------------------------------------------------------------
# Search filters: programming language
# ---------------------------------------------------------------------------


def test_search_include_language_returns_only_that_language(
    client: httpx.Client, project: str
) -> None:
    index_files(
        client, project, {"rust": {"a.rs": RUST_V1}, "python": {"b.py": PYTHON_SRC}}
    )
    resp = search(
        client,
        project,
        "process records",
        include={"programming_languages": ["python"]},
    )
    assert resp.status_code == 200
    results = resp.json()["results"]
    assert len(results) >= 1
    # Only the python file's chunks may appear.
    assert all(r["path"] == "b.py" for r in results), [r["path"] for r in results]


def test_search_exclude_language_omits_that_language(
    client: httpx.Client, project: str
) -> None:
    index_files(
        client, project, {"rust": {"a.rs": RUST_V1}, "python": {"b.py": PYTHON_SRC}}
    )
    resp = search(
        client,
        project,
        "process records",
        exclude={"programming_languages": ["python"]},
    )
    assert resp.status_code == 200
    results = resp.json()["results"]
    assert len(results) >= 1
    assert all(r["path"] != "b.py" for r in results), [r["path"] for r in results]


# ---------------------------------------------------------------------------
# Search filters: path GLOB
# ---------------------------------------------------------------------------


def test_search_include_path_glob_restricts_results(
    client: httpx.Client, project: str
) -> None:
    index_files(client, project, {"rust": {"src/a.rs": RUST_V1, "tests/b.rs": RUST_V2}})
    resp = search(client, project, "process records", include={"paths": ["src/**"]})
    assert resp.status_code == 200
    results = resp.json()["results"]
    assert len(results) >= 1
    assert all(r["path"].startswith("src/") for r in results), [
        r["path"] for r in results
    ]


def test_search_exclude_path_glob_omits_matches(
    client: httpx.Client, project: str
) -> None:
    index_files(client, project, {"rust": {"src/a.rs": RUST_V1, "tests/b.rs": RUST_V2}})
    resp = search(client, project, "process records", exclude={"paths": ["tests/**"]})
    assert resp.status_code == 200
    results = resp.json()["results"]
    assert len(results) >= 1
    assert all(not r["path"].startswith("tests/") for r in results), [
        r["path"] for r in results
    ]


def test_search_invalid_language_in_filter_is_rejected(
    client: httpx.Client, project: str
) -> None:
    # An unknown language enum value must be refused, not silently ignored. The
    # ApiJson extractor renders any body-deserialization failure as the uniform
    # problem+json envelope: 400 request.malformed_body (not axum's default 422).
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    resp = client.post(
        f"{MINDEX_URL}/v0/{project}/search",
        json={"query": "x", "include": {"programming_languages": ["cobol"]}},
    )
    assert resp.status_code == 400, resp.text
    assert resp.json()["code"] == "request.malformed_body", resp.text
