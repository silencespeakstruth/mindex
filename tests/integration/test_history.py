"""
Integration tests for POST /v0/{guid}/history — the git-history channel.

Reconciliation is a **set difference on shas**, not a hash comparison: a sha is
the hash of its own content, so there is no "same commit, different bytes" case.
That single property is what these tests keep honest — it is why a re-post is
free, why a rewritten branch needs no special handling, and why `since` has to
bound the deletion half.

The regression guard that matters most is `test_commit_paths_never_surface_in_drift`:
commits deliberately live in their own tables rather than as `project_files` rows,
and if that ever changes, every commit path becomes permanently `orphaned` drift
that no reindex can clear.
"""

import hashlib

import httpx
from test_e2e import RUST_V1

MINDEX_URL = __import__("conftest").MINDEX_URL


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def sha_of(seed: str) -> str:
    """A syntactically valid 40-hex commit sha, derived from a label."""
    return hashlib.sha1(seed.encode()).hexdigest()


def commit(
    seed: str,
    committed_at: int,
    paths: list[str],
    *,
    subject: str | None = None,
    body: str = "",
    parent_count: int = 1,
    change_type: str = "modified",
    old_path: str | None = None,
) -> dict:
    entry: dict = {
        "sha": sha_of(seed),
        "author_name": "A U Thor",
        "author_email": "a@example.com",
        "authored_at": committed_at,
        "committed_at": committed_at,
        "parent_count": parent_count,
        "subject": subject or f"commit {seed}",
        "body": body,
    }
    touches: list[dict] = []
    for p in paths:
        touch: dict = {"path": p, "change_type": change_type}
        if old_path is not None:
            touch["old_path"] = old_path
        touches.append(touch)
    entry["paths"] = touches
    return entry


def post_history(
    client: httpx.Client,
    project: str,
    commits: list[dict],
    since: int | None = None,
) -> httpx.Response:
    body: dict = {"commits": commits}
    if since is not None:
        body["since"] = since
    return client.post(f"{MINDEX_URL}/v0/{project}/history", json=body)


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


# ---------------------------------------------------------------------------
# Reconciliation
# ---------------------------------------------------------------------------


def test_first_post_indexes_everything(client: httpx.Client, project: str) -> None:
    resp = post_history(
        client,
        project,
        [commit("a", 100, ["src/a.rs"]), commit("b", 200, ["src/b.rs", "src/a.rs"])],
    )
    assert resp.status_code == 200, resp.text
    assert resp.json() == {"indexed": 2, "unchanged": 0, "removed": 0}


def test_reposting_the_same_history_is_a_no_op(
    client: httpx.Client, project: str
) -> None:
    """A commit's content is its identity, so the client may re-post its whole
    window every run rather than negotiating a diff with the server first."""
    commits = [commit("a", 100, ["src/a.rs"]), commit("b", 200, ["src/b.rs"])]
    post_history(client, project, commits)

    resp = post_history(client, project, commits)
    assert resp.json() == {"indexed": 0, "unchanged": 2, "removed": 0}


def test_a_rewritten_history_orphans_the_old_shas(
    client: httpx.Client, project: str
) -> None:
    """Force-push and rebase are not special cases: the refs simply reach a
    different set of shas, and reconciliation drops the ones nobody named."""
    post_history(
        client,
        project,
        [commit("a", 100, ["src/a.rs"]), commit("b", 200, ["src/b.rs"])],
    )

    resp = post_history(
        client,
        project,
        [commit("c", 100, ["src/a.rs"]), commit("d", 200, ["src/b.rs"])],
    )
    assert resp.json() == {"indexed": 2, "unchanged": 0, "removed": 2}

    # And the new set is now the whole history.
    resp = post_history(
        client,
        project,
        [commit("c", 100, ["src/a.rs"]), commit("d", 200, ["src/b.rs"])],
    )
    assert resp.json() == {"indexed": 0, "unchanged": 2, "removed": 0}


def test_a_windowed_post_leaves_older_commits_alone(
    client: httpx.Client, project: str
) -> None:
    """`since` bounds the deletion half. Without it a client walking only the
    recent window would wipe everything older on every pass — from the server's
    side an unmentioned commit and one outside the walk look identical."""
    post_history(
        client,
        project,
        [commit("old", 100, ["src/a.rs"]), commit("new", 500, ["src/b.rs"])],
    )

    # Speaks only for t >= 400.
    resp = post_history(client, project, [commit("new", 500, ["src/b.rs"])], since=400)
    assert resp.json()["removed"] == 0

    # The same post claiming to speak for all of history drops the old one.
    resp = post_history(client, project, [commit("new", 500, ["src/b.rs"])])
    assert resp.json()["removed"] == 1


def test_history_works_on_a_project_with_no_files(
    client: httpx.Client, project: str
) -> None:
    """The git walk does not depend on the working tree, so history may arrive
    before anything has been indexed. The project row is created here."""
    resp = post_history(client, project, [commit("a", 100, ["src/a.rs"])])
    assert resp.status_code == 200
    assert resp.json()["indexed"] == 1


def test_deleting_the_project_takes_its_history(
    client: httpx.Client, project: str
) -> None:
    post_history(client, project, [commit("a", 100, ["src/a.rs"])])
    assert client.delete(f"{MINDEX_URL}/projects/{project}").status_code == 204

    # The project is gone, so its commits are too: the next post starts fresh.
    resp = post_history(client, project, [commit("a", 100, ["src/a.rs"])])
    assert resp.json()["indexed"] == 1


# ---------------------------------------------------------------------------
# Retention (DELETE)
# ---------------------------------------------------------------------------


def prune(
    client: httpx.Client,
    project: str,
    keep_last: int | None = None,
    older_than: int | None = None,
) -> httpx.Response:
    params: dict[str, int] = {}
    if keep_last is not None:
        params["keep_last"] = keep_last
    if older_than is not None:
        params["older_than"] = older_than
    return client.delete(f"{MINDEX_URL}/v0/{project}/history", params=params)


def test_a_prune_with_no_bound_is_refused(client: httpx.Client, project: str) -> None:
    """A request that forgot its parameters and a request meaning "drop
    everything" must not be the same request — the `selector.empty` rule."""
    post_history(client, project, [commit("a", 100, ["src/a.rs"])])

    resp = prune(client, project)
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.history_bound_missing"

    # And nothing moved.
    assert post_history(client, project, [commit("a", 100, ["src/a.rs"])]).json() == {
        "indexed": 0,
        "unchanged": 1,
        "removed": 0,
    }


def test_keep_last_keeps_the_newest(client: httpx.Client, project: str) -> None:
    post_history(
        client,
        project,
        [
            commit("a", 100, ["src/a.rs"]),
            commit("b", 200, ["src/b.rs"]),
            commit("c", 300, ["src/c.rs"]),
        ],
    )

    resp = prune(client, project, keep_last=1)
    assert resp.status_code == 200, resp.text
    assert resp.json() == {"removed": 2, "remaining": 1}

    # The survivor is the newest: re-posting it alone is a no-op.
    assert post_history(client, project, [commit("c", 300, ["src/c.rs"])]).json() == {
        "indexed": 0,
        "unchanged": 1,
        "removed": 0,
    }


def test_older_than_prunes_by_the_clock(client: httpx.Client, project: str) -> None:
    post_history(
        client,
        project,
        [commit("a", 100, ["src/a.rs"]), commit("b", 500, ["src/b.rs"])],
    )

    assert prune(client, project, older_than=300).json() == {
        "removed": 1,
        "remaining": 1,
    }


def test_the_bounds_intersect_so_keep_last_is_a_floor(
    client: httpx.Client, project: str
) -> None:
    """Given two rules a destructive endpoint takes the conservative reading:
    "prune anything this old, but never leave me with fewer than N"."""
    post_history(
        client,
        project,
        [
            commit("a", 100, ["src/a.rs"]),
            commit("b", 200, ["src/b.rs"]),
            commit("c", 300, ["src/c.rs"]),
        ],
    )

    # The clock condemns all three; the floor saves the two newest.
    assert prune(client, project, keep_last=2, older_than=10_000).json() == {
        "removed": 1,
        "remaining": 2,
    }


def test_keep_last_zero_clears_the_channel(client: httpx.Client, project: str) -> None:
    """`keep_last=0` is the explicit spelling of "drop the whole history" — and
    the repository is the source of truth, so the next reconciliation refills
    it."""
    commits = [commit("a", 100, ["src/a.rs"]), commit("b", 200, ["src/b.rs"])]
    post_history(client, project, commits)

    assert prune(client, project, keep_last=0).json() == {"removed": 2, "remaining": 0}
    assert post_history(client, project, commits).json()["indexed"] == 2


# ---------------------------------------------------------------------------
# The two-table decision
# ---------------------------------------------------------------------------


def test_commit_paths_never_surface_in_drift(
    client: httpx.Client, project: str
) -> None:
    """The regression guard for the whole design.

    A commit names paths the working tree may not contain — deleted long ago,
    excluded by `.mindex`, in an unsupported language. Had commits been modelled
    as `project_files` rows, each of those paths would be reported `orphaned` by
    every drift check forever, `mindex-index --check` would exit non-zero on a
    clean tree, and the watcher would keep trying to delete them.
    """
    index_files(client, project, {"rust": {"a.rs": RUST_V1}})
    post_history(
        client,
        project,
        [commit("a", 100, ["deleted/long/ago.rs", "vendor/excluded.rs", "a.rs"])],
    )

    manifest = {"a.rs": hashlib.sha256(RUST_V1.encode()).hexdigest()}
    resp = client.post(
        f"{MINDEX_URL}/projects/{project}/drift", json={"files": manifest}
    )
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["orphaned"] == [], body
    assert body["stale"] == [] and body["missing"] == [], body


def test_commits_do_not_appear_in_the_file_listing(
    client: httpx.Client, project: str
) -> None:
    """The other half of the same guard, from the inventory side."""
    post_history(client, project, [commit("a", 100, ["src/never-indexed.rs"])])

    resp = client.get(f"{MINDEX_URL}/projects/{project}/files")
    assert resp.status_code in (200, 404), resp.text
    if resp.status_code == 200:
        paths = [f["path"] for f in resp.json()["files"]]
        assert "src/never-indexed.rs" not in paths


# ---------------------------------------------------------------------------
# Edge validation (caps come from mindex-test-config.toml)
# ---------------------------------------------------------------------------


def test_a_bad_sha_is_rejected(client: httpx.Client, project: str) -> None:
    bad = commit("a", 100, ["src/a.rs"])
    bad["sha"] = "not-a-sha"
    resp = post_history(client, project, [bad])
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.commit_invalid"


def test_an_empty_subject_is_rejected(client: httpx.Client, project: str) -> None:
    bad = commit("a", 100, ["src/a.rs"], subject="   ")
    resp = post_history(client, project, [bad])
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.commit_invalid"


def test_old_path_is_required_exactly_for_renames(
    client: httpx.Client, project: str
) -> None:
    """The biconditional. The `Some`-on-a-modification half is the one that
    matters: it is how a client that mis-parsed git's `--raw -z` arity — where a
    rename emits two paths and everything else emits one — is caught at the edge
    rather than storing a whole desynchronised stream."""
    renamed_no_source = commit("a", 100, ["src/b.rs"], change_type="renamed")
    resp = post_history(client, project, [renamed_no_source])
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.commit_invalid"

    modified_with_source = commit(
        "b", 100, ["src/b.rs"], change_type="modified", old_path="src/a.rs"
    )
    resp = post_history(client, project, [modified_with_source])
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.commit_invalid"

    ok = commit("c", 100, ["src/b.rs"], change_type="renamed", old_path="src/a.rs")
    assert post_history(client, project, [ok]).status_code == 200


def test_an_absolute_path_is_rejected(client: httpx.Client, project: str) -> None:
    resp = post_history(client, project, [commit("a", 100, ["/etc/passwd"])])
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.path_invalid"


def test_too_many_commits_is_rejected(client: httpx.Client, project: str) -> None:
    # mindex-test-config.toml caps this at 50.
    commits = [commit(str(i), 100 + i, ["src/a.rs"]) for i in range(51)]
    resp = post_history(client, project, commits)
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.too_many_commits"
    assert resp.json()["meta"]["max"] == 50


def test_an_oversized_message_is_rejected(client: httpx.Client, project: str) -> None:
    # mindex-test-config.toml caps subject+body at 4096 bytes.
    huge = commit("a", 100, ["src/a.rs"], body="x" * 5000)
    resp = post_history(client, project, [huge])
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.commit_message_too_large"


def test_an_unknown_change_type_is_a_malformed_body(
    client: httpx.Client, project: str
) -> None:
    """`ChangeType` is a closed serde enum, so a value the schema's CHECK would
    reject never reaches SQLite."""
    bad = commit("a", 100, ["src/a.rs"], change_type="teleported")
    resp = post_history(client, project, [bad])
    assert resp.status_code == 400
    assert resp.json()["code"] == "request.malformed_body"
