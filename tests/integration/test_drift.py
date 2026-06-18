"""
Integration tests for POST /projects/{guid}/drift.

Drift compares a posted working-tree manifest (path -> sha256) against the index
and returns four buckets: stale / missing / orphaned / indexing. The server stays
filesystem-agnostic — it only diffs hashes — so these tests post hashes directly
(the same `sha256(code.encode())` the indexer's upload would produce).
"""

import concurrent.futures
import hashlib

import httpx

from test_e2e import RUST_V1, RUST_V2
from test_filters_and_languages import PYTHON_SRC

MINDEX_URL = __import__("conftest").MINDEX_URL


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


def drift(client: httpx.Client, project: str, files: dict[str, str]) -> httpx.Response:
    return client.post(f"{MINDEX_URL}/projects/{project}/drift", json={"files": files})


def sha(code: str) -> str:
    """The hash the server stores: hex(sha256(code bytes))."""
    return hashlib.sha256(code.encode()).hexdigest()


def test_drift_in_sync_reports_nothing(client: httpx.Client, project: str) -> None:
    index_files(
        client, project, {"rust": {"a.rs": RUST_V1}, "python": {"b.py": PYTHON_SRC}}
    )

    resp = drift(client, project, {"a.rs": sha(RUST_V1), "b.py": sha(PYTHON_SRC)})
    assert resp.status_code == 200
    body = resp.json()
    assert body == {"stale": [], "missing": [], "orphaned": [], "indexing": []}, body


def test_drift_classifies_stale_missing_orphaned(
    client: httpx.Client, project: str
) -> None:
    index_files(client, project, {"rust": {"a.rs": RUST_V1, "gone.rs": RUST_V2}})

    body = drift(
        client,
        project,
        {
            "a.rs": sha("totally different content"),  # changed → stale
            "new.rs": sha("brand new"),  # not indexed → missing
            # gone.rs omitted → indexed but absent locally → orphaned
        },
    ).json()

    assert body["stale"] == ["a.rs"], body
    assert body["missing"] == ["new.rs"], body
    assert body["orphaned"] == ["gone.rs"], body
    assert body["indexing"] == [], body


def test_drift_empty_project_makes_everything_missing(
    client: httpx.Client, project: str
) -> None:
    # No indexing at all: every posted file is missing, and it is NOT a 404
    # (unlike search) — an empty baseline is a valid answer.
    body = drift(client, project, {"x.rs": sha("x"), "y.rs": sha("y")}).json()
    assert sorted(body["missing"]) == ["x.rs", "y.rs"], body
    assert body["stale"] == [] and body["orphaned"] == [] and body["indexing"] == []


def test_drift_empty_manifest_orphans_everything_indexed(
    client: httpx.Client, project: str
) -> None:
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    body = drift(client, project, {}).json()
    assert body["orphaned"] == ["a.rs"], body
    assert body["stale"] == [] and body["missing"] == [] and body["indexing"] == []


def test_concurrent_reindex_same_file_converges_without_corruption(
    client: httpx.Client, project: str
) -> None:
    """Hammer one path with two different contents from many parallel `/index`
    requests at once. The per-file claim must serialize the whole prepare→embed→
    mark_indexed pipeline, so:

      * every response is 200 (won the slot) or 429 (slot busy, retry) — never a
        500 from an illegal status transition, which is exactly what an interleaved
        reindex would trigger;
      * the index converges to ONE coherent version: the stored sha256 equals a full
        version's hash (V1 or V2), proving mark_indexed committed a single request's
        chunk set rather than a mix.

    `/index` is synchronous (it returns only after mark_indexed), so once all threads
    join the index is settled — no polling needed.
    """
    path = "race.rs"
    versions = [RUST_V1, RUST_V2]
    n = 16

    def push(i: int) -> int:
        code = versions[i % 2]
        # A separate client per thread — independent connections, true concurrency.
        with httpx.Client(verify=False, timeout=60.0) as c:
            r = c.post(
                f"{MINDEX_URL}/v0/{project}/index",
                json={"files": {"rust": {path: {"code": code}}}},
            )
        return r.status_code

    with concurrent.futures.ThreadPoolExecutor(max_workers=n) as ex:
        codes = list(ex.map(push, range(n)))

    assert all(c in (200, 429) for c in codes), f"unexpected statuses: {codes}"
    assert 200 in codes, "at least one request must win the slot"

    # The stored hash must be exactly one of the two full versions (never a partial /
    # mixed chunk set). Drift reports `stale` iff the posted sha differs from stored,
    # so exactly one of the two manifests is in-sync, and neither is mid-flight.
    in_sync = []
    for label, code in (("V1", RUST_V1), ("V2", RUST_V2)):
        body = drift(client, project, {path: sha(code)}).json()
        assert body["indexing"] == [], body  # nothing left mid-flight after join
        if body["stale"] == []:
            in_sync.append(label)

    # Exactly one version is in-sync ⇒ the stored sha is one coherent version, not a mix.
    assert in_sync in (["V1"], ["V2"]), in_sync
