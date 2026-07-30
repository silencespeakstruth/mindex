"""
Integration tests for POST /v0/{guid}/symbols — the exact-name symbol lookup over
definitions/references extracted at indexing time from tree-sitter tags.

Symbols are extracted per file regardless of chunking, so the fixture files can be
small (they slice to 0 chunks — irrelevant here). Each test gets a fresh project.
"""

import httpx

MINDEX_URL = __import__("conftest").MINDEX_URL

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

# Definitions + a call site; `helper` is called from inside `greet`.
RUST_GREETER = """\
pub fn greet() {
    helper();
}

pub fn helper() {}
"""

# A second definition of `helper` (name collision across files).
RUST_OTHER_HELPER = """\
pub fn helper() {
    let _ = 42;
}
"""

RUST_RENAMED = """\
pub fn salute() {
    helper();
}
"""


def index(client: httpx.Client, project: str, code: str, path: str) -> httpx.Response:
    return client.post(
        f"{MINDEX_URL}/v0/{project}/index",
        json={"files": {"rust": {path: {"code": code}}}},
    )


def symbols(client: httpx.Client, project: str, **body: object) -> httpx.Response:
    return client.post(f"{MINDEX_URL}/v0/{project}/symbols", json=body)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_definitions_and_references_after_index(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_GREETER, "src/greeter.rs").status_code == 200

    resp = symbols(client, project, name="helper")
    assert resp.status_code == 200
    body = resp.json()

    defs = body["definitions"]
    assert body["total_definitions"] == 1
    assert defs[0]["path"] == "src/greeter.rs"
    assert defs[0]["kind"] == "function"

    refs = body["references"]
    assert body["total_references"] == 1
    assert refs[0]["kind"] == "call"
    # The call site sits inside fn greet — the enclosing definition is reported.
    assert refs[0]["parent_name"] == "greet"
    assert refs[0]["parent_kind"] == "function"


def test_collision_returns_all_candidates_anchor_ranks_first(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_GREETER, "src/greeter.rs").status_code == 200
    assert index(client, project, RUST_OTHER_HELPER, "lib/other.rs").status_code == 200

    body = symbols(client, project, name="helper", role="definition").json()
    assert body["total_definitions"] == 2, "both definitions must be candidates"

    anchored = symbols(
        client, project, name="helper", role="definition", anchor_path="lib/other.rs"
    ).json()
    assert [d["path"] for d in anchored["definitions"]] == [
        "lib/other.rs",
        "src/greeter.rs",
    ], "the anchor file's candidate must rank first"


def test_reindex_replaces_symbols(client: httpx.Client, project: str) -> None:
    assert index(client, project, RUST_GREETER, "src/greeter.rs").status_code == 200
    assert index(client, project, RUST_RENAMED, "src/greeter.rs").status_code == 200

    gone = symbols(client, project, name="greet").json()
    assert gone["total_definitions"] == 0, "the old definition must be replaced"

    fresh = symbols(client, project, name="salute").json()
    assert fresh["total_definitions"] == 1


def test_delete_files_removes_symbols(client: httpx.Client, project: str) -> None:
    assert index(client, project, RUST_GREETER, "src/greeter.rs").status_code == 200
    resp = client.request(
        "DELETE",
        f"{MINDEX_URL}/projects/{project}/files",
        json={"include": {"paths": ["src/greeter.rs"]}},
    )
    assert resp.status_code == 200

    body = symbols(client, project, name="helper").json()
    assert body["total_definitions"] == 0
    assert body["total_references"] == 0


def test_role_and_kind_filters(client: httpx.Client, project: str) -> None:
    assert index(client, project, RUST_GREETER, "src/greeter.rs").status_code == 200

    refs_only = symbols(client, project, name="helper", role="reference").json()
    assert refs_only["definitions"] == []
    assert refs_only["total_references"] == 1

    wrong_kind = symbols(client, project, name="helper", kind="class").json()
    assert wrong_kind["total_definitions"] == 0
    right_kind = symbols(
        client, project, name="helper", kind="function", role="definition"
    ).json()
    assert right_kind["total_definitions"] == 1


def test_unknown_project_and_unknown_name_are_empty_200(
    client: httpx.Client, project: str
) -> None:
    # Never-indexed project: an empty answer, not a 404 (mirrors /drift semantics).
    resp = symbols(client, project, name="anything")
    assert resp.status_code == 200
    body = resp.json()
    assert body["definitions"] == [] and body["references"] == []


def test_validation_errors_carry_stable_codes(
    client: httpx.Client, project: str
) -> None:
    resp = symbols(client, project, name="")
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.symbol_name_empty"

    # mindex-test-config.toml caps the name at 64 bytes.
    resp = symbols(client, project, name="x" * 65)
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.symbol_name_too_long"

    # ... and limit at 10 per role.
    resp = symbols(client, project, name="helper", limit=11)
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.symbol_limit_out_of_range"
    resp = symbols(client, project, name="helper", limit=0)
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.symbol_limit_out_of_range"


def test_limit_truncates_but_totals_are_full(
    client: httpx.Client, project: str
) -> None:
    for i in range(3):
        assert (
            index(client, project, RUST_OTHER_HELPER, f"mod{i}/helper.rs").status_code
            == 200
        )
    body = symbols(client, project, name="helper", role="definition", limit=2).json()
    assert len(body["definitions"]) == 2
    assert body["total_definitions"] == 3
