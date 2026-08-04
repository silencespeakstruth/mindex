"""
Integration tests for the stored-research half of /research: the runs a finished
investigation leaves behind, and their reuse as context for a later one.

Covers the browse endpoints (`GET /projects/{guid}/research[/{run_id}]`,
`POST …/pin`, `DELETE …`), the per-path staleness verdict, keyset paging, and the
`context_run_ids` request field. The mock ollama's scripted session is the same one
`test_research.py` documents: one `search` turn, then a fixed Markdown report.
"""

import json
import uuid
from collections.abc import Iterator

import httpx
import pytest

conftest = __import__("conftest")
MINDEX_URL = conftest.MINDEX_URL

from test_e2e import FILE_PATH, RUST_V1, RUST_V2, index
from test_research import MOCK_OLLAMA_URL, QUESTION, research_events


@pytest.fixture
def ollama_session() -> Iterator[object]:
    """Script the mock model's tool turns and/or its report texts, cleared after."""

    def set_session(
        actions: list[dict] | None = None, reports: list[str] | None = None
    ) -> None:
        body: dict = {"actions": actions or []}
        if reports is not None:
            body["reports"] = reports
        httpx.post(
            f"{MOCK_OLLAMA_URL}/script", json=body, timeout=5.0
        ).raise_for_status()

    try:
        yield set_session
    finally:
        httpx.post(
            f"{MOCK_OLLAMA_URL}/script", json={"actions": []}, timeout=5.0
        ).raise_for_status()


def run_once(client: httpx.Client, project: str, question: str) -> dict:
    """Drive one full research run and return its `done` payload.

    Deliberately through the **default** (non-streaming) mode, while `run_with`
    below stays on the stream: what this file tests is what a finished run leaves
    in the corpus, and that must not depend on how the caller chose to be told
    about it. Splitting the two runners is what makes a difference show up here
    rather than in nobody's test.
    """
    resp = client.post(
        f"{MINDEX_URL}/v0/{project}/research",
        json={"question": question, "effort": "low"},
        timeout=30.0,
    )
    assert resp.status_code == 200, resp.text
    return resp.json()["done"]


def list_runs(client: httpx.Client, project: str, **params: str | int | bool) -> dict:
    resp = client.get(f"{MINDEX_URL}/projects/{project}/research", params=params)
    assert resp.status_code == 200, resp.text
    return resp.json()


def test_a_finished_run_is_listed_and_readable(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    done = run_once(client, project, QUESTION)

    # `done` names the stored run. Without this a client that just watched the run
    # stream by cannot offer it back, which is the whole point of storing it.
    assert done["run_id"], done
    assert done["seq"] == 1, done

    page = list_runs(client, project)
    assert page["next_before_seq"] is None, "one run is not a full page"
    assert len(page["runs"]) == 1, page
    run = page["runs"][0]
    assert run["id"] == done["run_id"]
    assert run["seq"] == 1
    assert run["question"] == QUESTION
    # The stored title is the mock report's own heading, not the question.
    assert run["title"] == "Mock Report", run
    assert run["done_reason"] == "finalized"
    assert run["valid"] is True, run
    assert run["invalid_reason"] is None, run
    assert run["context"] == [], "a cold run has no ancestry"
    # Fresh: nothing has touched the file since the run read it.
    assert run["stale"] is False, run
    assert run["files_moved"] == 0, run
    assert run["files_total"] >= 1, "the run read the indexed file"
    # The list must not carry report bodies — that is why it is a separate endpoint.
    assert "report" not in run, run

    detail = client.get(f"{MINDEX_URL}/projects/{project}/research/{run['id']}")
    assert detail.status_code == 200, detail.text
    body = detail.json()
    assert body["report"].startswith("# Mock Report"), body["report"][:80]
    assert body["context_run_ids"] == []
    assert [f["state"] for f in body["files"]] == ["fresh"] * len(body["files"])


def test_reindexing_a_file_the_run_read_makes_it_stale(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    done = run_once(client, project, QUESTION)
    assert list_runs(client, project)["runs"][0]["stale"] is False

    # The tree moves under the stored report. This is the whole reason baselines are
    # persisted per path: a global version counter would have marked every run of
    # every project stale on this one edit.
    assert index(client, project, RUST_V2).status_code == 200

    run = list_runs(client, project)["runs"][0]
    assert run["stale"] is True, run
    assert run["files_moved"] >= 1, run

    detail = client.get(
        f"{MINDEX_URL}/projects/{project}/research/{done['run_id']}"
    ).json()
    moved = [f for f in detail["files"] if f["state"] != "fresh"]
    assert moved, detail["files"]
    # `changed`, not `removed`: the file is still indexed, at a different hash.
    assert any(f["path"] == FILE_PATH and f["state"] == "changed" for f in moved), moved

    # And the filter must agree with the flag.
    assert list_runs(client, project, freshness="stale")["runs"], "stale filter"
    assert list_runs(client, project, freshness="fresh")["runs"] == []


def test_search_is_literal_and_paging_is_keyset(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    questions = [f"question number {n} about read_chunks" for n in range(3)]
    for q in questions:
        run_once(client, project, q)

    # `_` is a LIKE wildcard, so an unescaped pattern would also match `readXchunks`.
    # The escaping is what makes this a literal search over identifiers.
    hits = list_runs(client, project, q="read_chunks")["runs"]
    assert len(hits) == 3, hits
    assert list_runs(client, project, q="readXchunks")["runs"] == []
    assert len(list_runs(client, project, q="number 1")["runs"]) == 1

    # Keyset: two pages of one concatenate to the whole set, in order, with no
    # overlap and no gap.
    first = list_runs(client, project, limit=2)
    assert len(first["runs"]) == 2
    assert first["next_before_seq"] == first["runs"][-1]["seq"]
    second = list_runs(client, project, limit=2, before_seq=first["next_before_seq"])
    seqs = [r["seq"] for r in first["runs"] + second["runs"]]
    assert seqs == sorted(seqs, reverse=True), seqs
    assert len(set(seqs)) == len(seqs), f"a row was returned twice: {seqs}"
    assert seqs == [3, 2, 1], seqs
    assert second["next_before_seq"] is None, "a short page ends the walk"


def test_pinning_clears_the_expiry_and_delete_is_idempotent(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    run_id = run_once(client, project, QUESTION)["run_id"]
    base = f"{MINDEX_URL}/projects/{project}/research/{run_id}"

    listed = list_runs(client, project)["runs"][0]
    assert listed["pinned"] is False
    assert listed["expires_at"] is not None

    pinned = client.post(f"{base}/pin", json={"pinned": True})
    assert pinned.status_code == 200, pinned.text
    # Pinned is `expires_at IS NULL`, not a separate flag the two could disagree on.
    assert pinned.json()["expires_at"] is None
    assert pinned.json()["pinned"] is True
    assert list_runs(client, project, pinned=True)["runs"][0]["id"] == run_id
    assert list_runs(client, project, pinned=False)["runs"] == []

    unpinned = client.post(f"{base}/pin", json={"pinned": False})
    assert unpinned.status_code == 200, unpinned.text
    assert unpinned.json()["expires_at"] is not None

    # `pinned` defaults to true, so the obvious call on an endpoint named `/pin`
    # works. It used to be required, which made `{}` a 400 naming a field the
    # caller had no reason to guess.
    defaulted = client.post(f"{base}/pin", json={})
    assert defaulted.status_code == 200, defaulted.text
    assert defaulted.json()["pinned"] is True
    assert client.post(f"{base}/pin", json={"pinned": False}).status_code == 200

    assert client.delete(base).status_code == 204
    # Idempotent, matching DELETE /projects/{guid}: deleting what is already gone is
    # the outcome the caller asked for.
    assert client.delete(base).status_code == 204
    assert list_runs(client, project)["runs"] == []
    assert client.get(base).status_code == 404


def test_a_later_run_can_be_given_an_earlier_report(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    first = run_once(client, project, QUESTION)

    status, events = research_events(
        client,
        project,
        {
            "question": "a follow-up question",
            "effort": "low",
            "context_run_ids": [first["run_id"]],
        },
    )
    assert status == 200, events
    second = json.loads(events[-1][1])
    assert second["seq"] == 2, second

    # The reuse is journalled, so "does this feature get used" is answerable from the
    # corpus rather than only from a counter.
    detail = client.get(
        f"{MINDEX_URL}/projects/{project}/research/{second['run_id']}"
    ).json()
    assert detail["context_run_ids"] == [first["run_id"]], detail


def test_an_unknown_or_foreign_run_is_refused(
    client: httpx.Client, project: str
) -> None:
    other = uuid.uuid4().hex
    assert index(client, project, RUST_V1).status_code == 200
    assert index(client, other, RUST_V1).status_code == 200
    mine = run_once(client, other, QUESTION)["run_id"]

    for run_id in ("11111111-1111-4111-8111-111111111111", mine):
        status, events = research_events(
            client,
            project,
            {"question": QUESTION, "effort": "low", "context_run_ids": [run_id]},
        )
        # One code for "no such run" and "another project's run": the distinction is
        # not something the caller can act on, and separating them would let one
        # project probe another's ids by their error codes.
        assert status == 404, events
        assert json.loads(events[0][1])["code"] == "research.run_not_found"

    # And a run of another project is invisible to this one's list.
    assert list_runs(client, project)["runs"] == []


@pytest.mark.parametrize("limit", [0, 100000])
def test_an_out_of_range_page_size_is_a_400(
    client: httpx.Client, project: str, limit: int
) -> None:
    resp = client.get(
        f"{MINDEX_URL}/projects/{project}/research", params={"limit": limit}
    )
    assert resp.status_code == 400, resp.text
    assert resp.json()["code"] == "validation.research_list_limit_out_of_range"


def test_an_unknown_project_is_a_404(client: httpx.Client) -> None:
    resp = client.get(
        f"{MINDEX_URL}/projects/11111111-1111-4111-8111-111111111111/research"
    )
    assert resp.status_code == 404, resp.text
    assert resp.json()["code"] == "project.not_found"


def run_with(
    client: httpx.Client, project: str, question: str, context: list[str]
) -> dict:
    """One run with earlier reports as context; returns the `done` payload."""
    status, events = research_events(
        client,
        project,
        {"question": question, "effort": "low", "context_run_ids": context},
    )
    assert status == 200, events
    assert events[-1][0] == "done", events
    return json.loads(events[-1][1])


def runs_by_id(client: httpx.Client, project: str, **params: str | int | bool) -> dict:
    return {r["id"]: r for r in list_runs(client, project, **params)["runs"]}


def test_the_title_comes_from_the_report_heading(
    client: httpx.Client, project: str, ollama_session: object
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    # The default mock report is headed "# Mock Report" — stored as the title
    # (asserted exactly in test_a_finished_run_is_listed_and_readable). A report
    # whose heading merely echoes the question stores NO title, and the wire falls
    # back to the question-derived one.
    ollama_session(reports=[f"# {QUESTION}\n\nNothing further to add."])  # type: ignore[operator]
    run_once(client, project, QUESTION)
    run = list_runs(client, project)["runs"][0]
    assert run["title"] == QUESTION, run


def test_deleting_a_context_run_invalidates_its_descendants(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    a = run_once(client, project, "question A")["run_id"]
    b = run_with(client, project, "question B", [a])["run_id"]
    c = run_with(client, project, "question C", [b])["run_id"]

    by_id = runs_by_id(client, project)
    assert all(r["valid"] for r in by_id.values()), by_id
    # C's ancestry is transitive and flat: both B and A, each currently valid.
    c_ctx = {d["id"]: d for d in by_id[c]["context"]}
    assert set(c_ctx) == {a, b}, c_ctx
    assert all(d["state"] == "valid" for d in c_ctx.values()), c_ctx

    assert (
        client.delete(f"{MINDEX_URL}/projects/{project}/research/{b}").status_code
        == 204
    )

    by_id = runs_by_id(client, project)
    assert by_id[a]["valid"] is True, by_id[a]
    # C is invalidated by the dangling reference alone — no write touched its row.
    assert by_id[c]["valid"] is False, by_id[c]
    assert by_id[c]["invalid_reason"] == "context_deleted", by_id[c]
    dead = {d["id"]: d for d in by_id[c]["context"]}[b]
    assert dead["state"] == "deleted", dead
    assert dead["title"] is None and dead["seq"] is None, dead

    assert set(runs_by_id(client, project, valid=True)) == {a}
    assert set(runs_by_id(client, project, valid=False)) == {c}


def test_a_stale_ancestor_invalidates_the_chain(
    client: httpx.Client, project: str, ollama_session: object
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    a = run_once(client, project, "question A")["run_id"]
    # B reads nothing (a glob that matches no file), so its own baselines are
    # empty and it can only go invalid through its ancestry.
    ollama_session(actions=[{"action": "list_files", "glob": "zzz-no-such"}])  # type: ignore[operator]
    b = run_with(client, project, "question B", [a])["run_id"]

    assert index(client, project, RUST_V2).status_code == 200

    by_id = runs_by_id(client, project)
    assert by_id[a]["stale"] is True and by_id[a]["invalid_reason"] == "stale", by_id[a]
    assert by_id[b]["files_moved"] == 0 and by_id[b]["stale"] is False, by_id[b]
    assert by_id[b]["valid"] is False, by_id[b]
    assert by_id[b]["invalid_reason"] == "context_invalid", by_id[b]
    # Freshness keeps its meaning — B's own files are current — while validity is
    # the transitive verdict. The two filters answer different questions.
    assert set(runs_by_id(client, project, freshness="fresh")) == {b}
    assert runs_by_id(client, project, valid=True) == {}


def test_invalid_context_is_refused(client: httpx.Client, project: str) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    a = run_once(client, project, QUESTION)["run_id"]
    assert index(client, project, RUST_V2).status_code == 200

    status, events = research_events(
        client,
        project,
        {"question": "a follow-up", "effort": "low", "context_run_ids": [a]},
    )
    assert status == 400, events
    body = json.loads(events[0][1])
    assert body["code"] == "validation.research_context_invalid", body
    assert body["meta"]["runs"] == [{"id": a, "reason": "stale"}], body
    # The refused run left no row behind.
    assert len(list_runs(client, project)["runs"]) == 1


def test_a_broken_report_is_not_journalled(
    client: httpx.Client, project: str, ollama_session: object
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    # Both the draft and the rewrite are JSON, so the markdown gate holds twice.
    ollama_session(reports=['{"finding": 1}', '{"finding": 2}'])  # type: ignore[operator]
    status, events = research_events(
        client, project, {"question": QUESTION, "effort": "low"}
    )
    assert status == 200, events
    done = json.loads(events[-1][1])
    # Streamed but never stored: the same wire shape as a failed journal write.
    assert done["run_id"] is None, done
    assert done["seq"] is None, done
    assert list_runs(client, project)["runs"] == []


def test_the_model_can_browse_stored_reports(
    client: httpx.Client, project: str, ollama_session: object
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    run_once(client, project, "the first question")

    ollama_session(  # type: ignore[operator]
        actions=[
            {"action": "list_research"},
            {"action": "read_research", "seq": 1},
        ]
    )
    status, events = research_events(
        client, project, {"question": "a follow-up question", "effort": "low"}
    )
    assert status == 200, events
    steps = [json.loads(d) for e, d in events if e == "step"]
    browse = {s["action"]: s for s in steps if s["action"].endswith("research")}
    assert set(browse) == {"list_research", "read_research"}, steps
    assert browse["list_research"]["hits"] >= 1, browse
    assert browse["read_research"]["seq"] == "1", browse
    assert browse["read_research"]["hits"] == 1, browse
    done = json.loads(events[-1][1])
    assert done["run_id"], "the browsing run itself is journalled"
    assert json.loads(events[-1][1])["seq"] == 2
