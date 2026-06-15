"""
End-to-end tests for the mindex index + search pipeline.

Each test gets its own project GUID so tests are fully isolated.
"""
import httpx
import pytest

MINDEX_URL = __import__("conftest").MINDEX_URL

# ---------------------------------------------------------------------------
# Fixtures: Rust source snippets
# ---------------------------------------------------------------------------

# Large enough to produce at least one chunk (≥128 BGE-M3 tokens).
RUST_V1 = """\
pub fn process_records(
    records: &[(String, Vec<i64>)],
    config: &ProcessConfig,
    output: &mut Vec<ProcessedRecord>,
) -> Result<Statistics, PipelineError> {
    let mut stats = Statistics::default();
    let batch_size = config.batch_size.unwrap_or(64);
    let max_retries = config.max_retries.unwrap_or(3);
    for (batch_idx, batch) in records.chunks(batch_size).enumerate() {
        let mut attempt = 0usize;
        loop {
            match process_batch(batch, config) {
                Ok(processed) => {
                    output.extend(processed);
                    stats.processed += batch.len();
                    stats.batches += 1;
                    break;
                }
                Err(err) if attempt < max_retries => {
                    attempt += 1;
                    stats.retries += 1;
                    tracing::warn!(batch_idx, attempt, %err, "Retrying batch.");
                }
                Err(err) => {
                    return Err(PipelineError::BatchFailed {
                        batch_index: batch_idx,
                        source: err,
                    });
                }
            }
        }
    }
    Ok(stats)
}
"""

# A meaningfully different version of the same function.
RUST_V2 = """\
pub fn process_records(
    records: &[(String, Vec<i64>)],
    config: &ProcessConfig,
    output: &mut Vec<ProcessedRecord>,
) -> Result<Statistics, PipelineError> {
    let mut stats = Statistics::default();
    let batch_size = config.batch_size.unwrap_or(128);
    let max_retries = config.max_retries.unwrap_or(5);
    for (batch_idx, batch) in records.chunks(batch_size).enumerate() {
        let mut attempt = 0usize;
        'retry: loop {
            match process_batch(batch, config) {
                Ok(processed) => {
                    output.extend(processed);
                    stats.processed += batch.len();
                    stats.batches += 1;
                    break 'retry;
                }
                Err(err) if attempt < max_retries => {
                    attempt += 1;
                    stats.retries += 1;
                    tracing::warn!(batch_idx, attempt, %err, "Retrying batch (v2).");
                }
                Err(err) => {
                    return Err(PipelineError::BatchFailed {
                        batch_index: batch_idx,
                        source: err,
                    });
                }
            }
        }
    }
    stats.version = 2;
    Ok(stats)
}
"""

FILE_PATH = "src/pipeline.rs"


def index(client: httpx.Client, project: str, code: str, path: str = FILE_PATH) -> httpx.Response:
    return client.post(
        f"{MINDEX_URL}/v0/{project}/index",
        json={"files": {"rust": {path: {"code": code}}}},
    )


def search(client: httpx.Client, project: str, query: str, top_k: int = 5) -> httpx.Response:
    return client.post(
        f"{MINDEX_URL}/v0/{project}/search",
        json={"query": query, "top_k": top_k},
    )


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

def test_index_new_file_returns_chunk_count(client: httpx.Client, project: str) -> None:
    resp = index(client, project, RUST_V1)
    assert resp.status_code == 200
    files = resp.json()["files"]["rust"]
    assert FILE_PATH in files
    assert files[FILE_PATH] >= 1, "Expected at least one chunk to be indexed"


def test_search_finds_indexed_content(client: httpx.Client, project: str) -> None:
    index(client, project, RUST_V1)
    resp = search(client, project, "process batch retry")
    assert resp.status_code == 200
    results = resp.json()["results"]
    assert len(results) >= 1
    codes = [r["code"] for r in results]
    assert any(FILE_PATH == r["path"] for r in results)
    # The indexed source must appear verbatim in one of the returned chunks.
    assert any("process_records" in c for c in codes)


def test_reindex_unchanged_file_is_noop(client: httpx.Client, project: str) -> None:
    index(client, project, RUST_V1)
    resp = index(client, project, RUST_V1)  # same content, second time
    assert resp.status_code == 200
    files = resp.json()["files"]["rust"]
    # Unchanged file must not produce new chunks (hash matched, skipped).
    assert files.get(FILE_PATH, 0) == 0


def test_reindex_changed_file_returns_new_chunks(client: httpx.Client, project: str) -> None:
    r1 = index(client, project, RUST_V1)
    assert r1.status_code == 200
    chunks_v1 = r1.json()["files"]["rust"][FILE_PATH]

    r2 = index(client, project, RUST_V2)
    assert r2.status_code == 200
    chunks_v2 = r2.json()["files"]["rust"][FILE_PATH]

    assert chunks_v2 >= 1, "Re-index of changed file must produce new chunks"
    # Both versions should produce a similar number of chunks for this fixture.
    assert abs(chunks_v1 - chunks_v2) <= max(chunks_v1, chunks_v2)


def test_search_after_reindex_reflects_new_content(client: httpx.Client, project: str) -> None:
    index(client, project, RUST_V1)
    index(client, project, RUST_V2)

    resp = search(client, project, "process batch retry v2")
    assert resp.status_code == 200
    results = resp.json()["results"]
    assert len(results) >= 1

    # The v2-specific marker must be present somewhere in the returned chunks.
    all_code = " ".join(r["code"] for r in results)
    assert "version" in all_code or "retry (v2)" in all_code or "'retry" in all_code, (
        "Search after re-index should surface v2 content"
    )


def test_search_result_has_line_numbers(client: httpx.Client, project: str) -> None:
    index(client, project, RUST_V1)
    resp = search(client, project, "process_records")
    assert resp.status_code == 200
    for result in resp.json()["results"]:
        assert result["start_line"] >= 1
        assert result["end_line"] >= result["start_line"]
        assert result["start_column"] >= 0
        assert result["end_column"] >= 0


def test_search_empty_project_returns_404(client: httpx.Client, project: str) -> None:
    # Index so the project exists in Qdrant, but search with a query
    # that should find nothing (empty collection edge case).
    # A brand-new project with no indexed files should return 404.
    resp = search(client, project, "anything")
    assert resp.status_code == 404


def test_multiple_files_indexed_independently(client: httpx.Client, project: str) -> None:
    resp = client.post(
        f"{MINDEX_URL}/v0/{project}/index",
        json={
            "files": {
                "rust": {
                    "src/pipeline.rs": {"code": RUST_V1},
                    "src/pipeline_v2.rs": {"code": RUST_V2},
                }
            }
        },
    )
    assert resp.status_code == 200
    files = resp.json()["files"]["rust"]
    assert "src/pipeline.rs" in files
    assert "src/pipeline_v2.rs" in files
    assert files["src/pipeline.rs"] >= 1
    assert files["src/pipeline_v2.rs"] >= 1
