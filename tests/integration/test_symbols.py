"""
Integration tests for POST /v0/{guid}/symbols — the exact-name lookup over the
DEFINITIONS extracted at indexing time from tree-sitter tags.

The reference half of the table was withdrawn in 1.1.0 (migration 6): the edges it
recorded were lexical, so "who calls X" was never a question it could answer
honestly, and `grep` answers it instead. What survives on a definition is
`parent_name`/`parent_kind` — the enclosing definition, which is what makes a
method name readable.

Symbols are extracted per file regardless of chunking, so the fixture files can be
small (they slice to 0 chunks — irrelevant here). Each test gets a fresh project.
"""

import httpx

MINDEX_URL = __import__("conftest").MINDEX_URL

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

# Two definitions; `helper` is also called from inside `greet`, which no longer
# produces a row of its own — the call site is only here to prove that it doesn't.
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

# A method inside a class, for parent_name/parent_kind.
PYTHON_GREETER = """\
class Greeter:
    def helper(self):
        return 42
"""


def index(client: httpx.Client, project: str, code: str, path: str) -> httpx.Response:
    return client.post(
        f"{MINDEX_URL}/v0/{project}/index",
        json={"files": {"rust": {path: {"code": code}}}},
    )


def index_python(
    client: httpx.Client, project: str, code: str, path: str
) -> httpx.Response:
    return client.post(
        f"{MINDEX_URL}/v0/{project}/index",
        json={"files": {"python": {path: {"code": code}}}},
    )


def symbols(client: httpx.Client, project: str, **body: object) -> httpx.Response:
    return client.post(f"{MINDEX_URL}/v0/{project}/symbols", json=body)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_definitions_after_index(client: httpx.Client, project: str) -> None:
    assert index(client, project, RUST_GREETER, "src/greeter.rs").status_code == 200

    resp = symbols(client, project, name="helper")
    assert resp.status_code == 200
    body = resp.json()

    defs = body["definitions"]
    assert body["total_definitions"] == 1
    assert defs[0]["path"] == "src/greeter.rs"
    assert defs[0]["kind"] == "function"

    # The reference half is gone from the wire entirely, not merely empty: a client
    # reading `total_references == 0` as "nothing calls it" would be told a falsehood
    # by a field that no longer means anything.
    assert "references" not in body
    assert "total_references" not in body


def test_a_call_site_is_not_a_symbol(client: httpx.Client, project: str) -> None:
    """`greet` calls `helper`, and that call produces no row of its own."""
    assert index(client, project, RUST_GREETER, "src/greeter.rs").status_code == 200

    # Exactly one row for `helper`: its definition. Before 1.1.0 there were two.
    assert symbols(client, project, name="helper").json()["total_definitions"] == 1


def test_parent_names_the_enclosing_definition(
    client: httpx.Client, project: str
) -> None:
    assert (
        index_python(client, project, PYTHON_GREETER, "app/greeter.py").status_code
        == 200
    )

    body = symbols(client, project, name="helper").json()
    assert body["total_definitions"] == 1
    method = body["definitions"][0]
    assert method["parent_name"] == "Greeter"
    assert method["parent_kind"] == "class"


def test_collision_returns_all_candidates_anchor_ranks_first(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_GREETER, "src/greeter.rs").status_code == 200
    assert index(client, project, RUST_OTHER_HELPER, "lib/other.rs").status_code == 200

    body = symbols(client, project, name="helper").json()
    assert body["total_definitions"] == 2, "both definitions must be candidates"

    anchored = symbols(
        client, project, name="helper", anchor_path="lib/other.rs"
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


def test_kind_filter(client: httpx.Client, project: str) -> None:
    assert index(client, project, RUST_GREETER, "src/greeter.rs").status_code == 200

    wrong_kind = symbols(client, project, name="helper", kind="class").json()
    assert wrong_kind["total_definitions"] == 0
    right_kind = symbols(client, project, name="helper", kind="function").json()
    assert right_kind["total_definitions"] == 1


def test_role_is_refused_rather_than_ignored(
    client: httpx.Client, project: str
) -> None:
    """The one wire change in 1.1.0, and the reason the body denies unknown fields.

    Accepting `role` and ignoring it would answer a `role: "reference"` query with
    the DEFINITIONS — the one wrong answer that costs nothing to detect and looks
    exactly like a right one.
    """
    assert index(client, project, RUST_GREETER, "src/greeter.rs").status_code == 200

    for role in ("definition", "reference"):
        resp = symbols(client, project, name="helper", role=role)
        assert resp.status_code == 400, f"role={role!r} must not be silently accepted"
        assert resp.json()["code"] == "request.malformed_body"


def test_unknown_project_and_unknown_name_are_empty_200(
    client: httpx.Client, project: str
) -> None:
    # Never-indexed project: an empty answer, not a 404 (mirrors /drift semantics).
    resp = symbols(client, project, name="anything")
    assert resp.status_code == 200
    body = resp.json()
    assert body["definitions"] == []
    assert body["total_definitions"] == 0


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

    # ... and limit at 10.
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
    body = symbols(client, project, name="helper", limit=2).json()
    assert len(body["definitions"]) == 2
    assert body["total_definitions"] == 3
