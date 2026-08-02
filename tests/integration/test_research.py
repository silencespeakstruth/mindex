"""
Integration tests for POST /v0/{guid}/research — the SSE research stream driven
by the scripted mock-ollama (see tests/mock_ollama/main.py: first tool turn is a
`search` echoing the question, then `finalize`, then a fixed Markdown report).

The test config pins [research] max_concurrent = 1, which makes the busy/slot
tests possible with just two requests.
"""

import itertools
import json
import os
import time
from collections.abc import Iterator

import httpx
import pytest

conftest = __import__("conftest")
MINDEX_URL = conftest.MINDEX_URL
MOCK_OLLAMA_URL = os.environ.get("MOCK_OLLAMA_URL", "http://localhost:11434")

from test_e2e import FILE_PATH, RUST_V1, RUST_V2, index, search

QUESTION = "how does process_records handle retries"


@pytest.fixture
def ollama_script() -> Iterator[object]:
    """Drive the mock model through an explicit action sequence, cleared after."""

    def set_script(*actions: dict) -> None:
        httpx.post(
            f"{MOCK_OLLAMA_URL}/script", json={"actions": list(actions)}, timeout=5.0
        ).raise_for_status()

    try:
        yield set_script
    finally:
        httpx.post(
            f"{MOCK_OLLAMA_URL}/script", json={"actions": []}, timeout=5.0
        ).raise_for_status()


@pytest.fixture
def ollama_knobs() -> Iterator[object]:
    """Reset the mock-ollama knobs after each test that touches them."""

    def set_knobs(**kwargs: float) -> None:
        httpx.post(
            f"{MOCK_OLLAMA_URL}/config", json=kwargs, timeout=5.0
        ).raise_for_status()

    try:
        yield set_knobs
    finally:
        set_knobs(
            turn_delay_secs=0.0,
            fail_next_chats=0.0,
            tags_down=0.0,
            force_text_calls=0.0,
        )


def research_events(
    client: httpx.Client, project: str, body: dict, timeout: float = 30.0
) -> tuple[int, list[tuple[str, str]]]:
    """POST /research and drain the SSE stream into (status, [(event, data)])."""
    events: list[tuple[str, str]] = []
    with client.stream(
        "POST",
        f"{MINDEX_URL}/v0/{project}/research",
        json=body,
        timeout=timeout,
    ) as resp:
        if resp.status_code != 200:
            resp.read()
            return resp.status_code, [("__body__", resp.text)]
        event = "message"
        for line in resp.iter_lines():
            if line.startswith("event:"):
                event = line[len("event:") :].strip()
            elif line.startswith("data:"):
                events.append((event, line[len("data:") :].strip()))
            # blank lines separate frames; ":" comments are keep-alives — both ignored
    return 200, events


def test_full_run_streams_steps_and_summary(client: httpx.Client, project: str) -> None:
    assert index(client, project, RUST_V1).status_code == 200

    status, events = research_events(
        client, project, {"question": QUESTION, "effort": "low"}
    )
    assert status == 200
    names = [e for e, _ in events]

    # The scripted session: thinking → one search step → thinking → summary → done.
    assert "thinking" in names, f"thinking deltas must be streamed: {names}"
    step_data = [json.loads(d) for e, d in events if e == "step"]
    assert len(step_data) == 1, f"exactly one scripted search step expected: {events}"
    assert step_data[0]["action"] == "search"
    assert step_data[0]["query"] == QUESTION
    assert step_data[0]["hits"] > 0, "the indexed file must be found"

    summary = "".join(json.loads(d)["text"] for e, d in events if e == "summary")
    assert summary.startswith("# Mock Report"), summary
    assert "src/pipeline.rs" in summary

    assert names[-1] == "done", f"the stream must end with done: {names}"
    done = json.loads(events[-1][1])
    assert done["steps"] == 1, done
    # The scripted model finalizes voluntarily, so the run must not be reported as
    # cut short — that distinction is what scout keys on.
    assert done["reason"] == "finalized", done
    assert isinstance(done["elapsed_ms"], int), done


def test_orientation_tools_list_and_outline(
    client: httpx.Client, project: str, ollama_script: object
) -> None:
    """`list_files` and `outline` are steps of their own, each naming its argument
    with its own key on the wire (glob / path, never `query`)."""
    assert index(client, project, RUST_V1).status_code == 200
    ollama_script(  # type: ignore[operator]
        {"action": "list_files", "glob": "*"},
        {"action": "outline", "path": FILE_PATH},
        {"action": "finalize"},
    )

    status, events = research_events(
        client, project, {"question": QUESTION, "effort": "medium"}
    )
    assert status == 200
    steps = [json.loads(d) for e, d in events if e == "step"]
    assert [s["action"] for s in steps] == ["list_files", "outline"], steps

    listing, outline = steps
    assert listing["glob"] == "*", listing
    assert "query" not in listing, "each action names its own argument key"
    assert listing["hits"] >= 1, "the indexed file must be listed"

    assert outline["path"] == FILE_PATH, outline
    assert outline["hits"] >= 1, "the rust fixture declares symbols"

    done = json.loads(events[-1][1])
    assert done["reason"] == "finalized", done


def test_reindexing_during_a_run_is_not_blocked_and_is_reported_as_stale(
    client: httpx.Client, project: str, ollama_script: object, ollama_knobs: object
) -> None:
    """Indexing has priority over research and is never blocked by it, so the run
    reports what moved instead of pretending the corpus held still.

    The whole freshness path end to end, which the loop's own unit tests cannot
    reach: the real `file_versions` SQL against a real `project_files` table. A
    passing run over an untouched corpus proves only that the query does not error —
    a query returning nothing at all would look exactly the same — so this is the
    case that proves it reads something.
    """
    assert index(client, project, RUST_V1).status_code == 200
    # Slow turns, so the reindex below lands inside the run rather than after it.
    ollama_knobs(turn_delay_secs=0.5)  # type: ignore[operator]
    ollama_script(  # type: ignore[operator]
        {"action": "search", "query": "process_records retries"},
        {"action": "outline", "path": FILE_PATH},
        {"action": "finalize"},
    )

    events: list[tuple[str, str]] = []
    steps = 0
    reindexed = False
    with client.stream(
        "POST",
        f"{MINDEX_URL}/v0/{project}/research",
        json={"question": QUESTION, "effort": "medium"},
        timeout=60.0,
    ) as resp:
        assert resp.status_code == 200
        event = "message"
        for line in resp.iter_lines():
            if line.startswith("event:"):
                event = line[len("event:") :].strip()
            elif line.startswith("data:"):
                events.append((event, line[len("data:") :].strip()))
                if event == "step":
                    steps += 1
                # After the SECOND step the first probe has certainly happened (it
                # runs before the turn that produced that step), so the new hash is
                # a *change* rather than the baseline. A separate client: the
                # research stream above is still open on this one.
                if steps == 2 and not reindexed:
                    reindexed = True
                    with httpx.Client(verify=False, timeout=30.0) as writer:
                        assert index(writer, project, RUST_V2).status_code == 200

    assert reindexed, "the run must produce at least two steps"
    names = [e for e, _ in events]
    assert "error" not in names, events

    citations = json.loads(next(d for e, d in events if e == "citations"))
    # The mock's report cites `src/pipeline.rs:1-10` — a location the search really
    # did return, in a file that has since been reindexed. Verified and stale at
    # once, which is why the two verdicts are separate buckets.
    assert citations["stale"] >= 1, citations
    assert FILE_PATH in citations["stale_paths"], citations
    assert citations["unverified"] == 0, citations
    # The report still ships, and the run still finishes on its own terms: nothing
    # about this blocks either side.
    assert any(e == "summary" for e, _ in events), names
    done = json.loads(events[-1][1])
    assert done["reason"] == "finalized", done


def test_outline_of_an_unknown_path_is_answered_not_an_error(
    client: httpx.Client, project: str, ollama_script: object
) -> None:
    """A wrong path guess must come back as a normal step with zero hits — the
    stream stays alive so the model can correct itself."""
    assert index(client, project, RUST_V1).status_code == 200
    ollama_script(  # type: ignore[operator]
        {"action": "outline", "path": "src/does/not/exist.rs"},
        {"action": "finalize"},
    )

    status, events = research_events(
        client, project, {"question": QUESTION, "effort": "low"}
    )
    assert status == 200
    names = [e for e, _ in events]
    assert "error" not in names, events
    steps = [json.loads(d) for e, d in events if e == "step"]
    assert len(steps) == 1 and steps[0]["hits"] == 0, steps
    assert names[-1] == "done"


def test_native_tool_calls_drive_the_loop(
    client: httpx.Client, project: str, ollama_script: object
) -> None:
    """The default path: the mock answers with `message.tool_calls`, so the server
    must act on the native field rather than parsing JSON out of the reply text."""
    assert index(client, project, RUST_V1).status_code == 200
    ollama_script(  # type: ignore[operator]
        {"action": "outline", "path": FILE_PATH},
        {"action": "search", "query": "process_records"},
        {"action": "finalize"},
    )
    status, events = research_events(
        client, project, {"question": QUESTION, "effort": "medium"}
    )
    assert status == 200
    steps = [json.loads(d) for e, d in events if e == "step"]
    assert [s["action"] for s in steps] == ["outline", "search"], steps
    done = json.loads(events[-1][1])
    assert done["reason"] == "finalized", done


def test_a_model_that_cannot_call_tools_fails_with_a_named_cause(
    client: httpx.Client, project: str, ollama_script: object, ollama_knobs: object
) -> None:
    """Some models declare tool support but emit the call as plain text, because
    their Ollama template has no tool support (observed on qwen2.5-coder:32b). That
    is the wrong model for the job, so it must be reported as such rather than
    limped around: guessing at a hand-written call means executing arguments nobody
    validated."""
    assert index(client, project, RUST_V1).status_code == 200
    ollama_knobs(force_text_calls=1.0)  # type: ignore[operator]
    ollama_script(  # type: ignore[operator]
        {"action": "search", "query": "process_records"},
    )
    status, events = research_events(
        client, project, {"question": QUESTION, "effort": "medium"}
    )
    # The stream itself is fine (HTTP 200); the failure arrives as an event.
    assert status == 200
    names = [e for e, _ in events]
    assert names[-1] == "error", events
    err = json.loads(events[-1][1])
    assert err["code"] == "research.model_lacks_tools", err
    assert "template" in err["detail"], err
    assert not [e for e, _ in events if e == "step"], "nothing may be executed"


def test_thinking_precedes_the_first_step(client: httpx.Client, project: str) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    _, events = research_events(
        client, project, {"question": QUESTION, "effort": "low"}
    )
    names = [e for e, _ in events]
    assert names.index("thinking") < names.index("step")


def test_validation_errors_before_the_stream(
    client: httpx.Client, project: str
) -> None:
    resp = client.post(
        f"{MINDEX_URL}/v0/{project}/research", json={"question": "", "effort": "low"}
    )
    assert resp.status_code == 400
    assert resp.json()["code"] == "validation.query_empty"

    resp = client.post(
        f"{MINDEX_URL}/v0/{project}/research",
        json={"question": "x", "effort": "extreme"},
    )
    assert resp.status_code == 400
    assert resp.json()["code"] == "request.malformed_body"

    # A budget override above [research].max_request_* is rejected at the edge,
    # before a slot is taken — the ceilings are what stop one request from holding
    # a research slot for as long as it likes.
    #
    # The boundaries come from GET /config, not from literals here. Config
    # validation refuses a ceiling below [research.effort.high], so raising the
    # high preset raises the ceilings with it — and a hardcoded "one above the
    # ceiling" silently becomes a legal value that streams a 200 instead of
    # failing. That is exactly how this test rotted once.
    caps = client.get(f"{MINDEX_URL}/config").json()["research"]
    for field, value in (
        ("max_steps", caps["max_request_steps"] + 1),
        ("max_seconds", caps["max_request_seconds"] + 1),
        ("max_tokens", caps["max_request_tokens"] + 1),
        ("max_steps", 0),
    ):
        resp = client.post(
            f"{MINDEX_URL}/v0/{project}/research",
            json={"question": "x", "effort": "low", "budget": {field: value}},
        )
        assert resp.status_code == 400, (field, value, resp.text)
        body = resp.json()
        assert body["code"] == "validation.research_budget_out_of_range", body
        assert body["field"] == field, body


def test_disallowed_model_gets_400(client: httpx.Client, project: str) -> None:
    # The test config sets [research].allowed_models = ["mock-*"]; a model outside
    # it is a policy refusal at the edge — before a slot is taken, so the 400 must
    # leave every research slot free.
    resp = client.post(
        f"{MINDEX_URL}/v0/{project}/research",
        json={"question": "x", "effort": "low", "model": "forbidden:1b"},
    )
    assert resp.status_code == 400
    body = resp.json()
    assert body["code"] == "research.model_not_allowed", body
    assert body["field"] == "model", body
    assert body["meta"]["model"] == "forbidden:1b", body
    assert client.get(f"{MINDEX_URL}/research/active").json()["slots_busy"] == 0


def test_progress_reports_the_budget_and_what_it_spent(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_V1).status_code == 200

    status, events = research_events(
        client,
        project,
        # A per-request budget: the effort preset for everything not named here.
        {"question": QUESTION, "effort": "low", "budget": {"max_steps": 3}},
    )
    assert status == 200
    progress = [json.loads(d) for e, d in events if e == "progress"]
    assert progress, f"a live run must report its budget consumption: {events}"

    # `started` names the run before any work; the budget announcement follows it,
    # still before any model turn, and carries the *resolved* budget — the override
    # applied to the effort preset.
    assert [e for e, _ in events[:2]] == ["started", "progress"], [e for e, _ in events]
    first = progress[0]
    assert (first["steps"], first["tokens"], first["turns"]) == (0, 0, 0), first
    assert first["max_steps"] == 3, first
    assert first["max_ms"] > 0 and first["max_tokens"] > 0, first

    # Counters only ever move forward, and the mock reports real token counts.
    for a, b in itertools.pairwise(progress):
        assert b["steps"] >= a["steps"], (a, b)
        assert b["tokens"] >= a["tokens"], (a, b)
        assert b["elapsed_ms"] >= a["elapsed_ms"], (a, b)
    assert progress[-1]["tokens"] > 0, progress[-1]
    assert progress[-1]["binding"] in {"time", "tokens", "steps", "context"}

    # `done` repeats the whole shape, so a consumer reading only the last event
    # still gets the run's cost.
    done = json.loads(events[-1][1])
    assert done["reason"] == "finalized", done
    assert done["steps"] == 1, done
    assert done["max_steps"] == 3, done
    assert done["tokens"] >= progress[-1]["tokens"], done
    assert done["turns"] >= 1, done


def test_a_live_run_can_be_listed_and_cancelled_by_name(
    client: httpx.Client, project: str, ollama_knobs: object
) -> None:
    """The outage this endpoint pair exists for.

    A run used to be invisible while it ran: its id was minted by the journal write
    at the very end, the stored-run list only shows finished runs, and there was no
    cancel endpoint. With `max_concurrent = 1` an occupied slot was therefore a total
    outage of research that could not be attributed to anything or ended short of
    restarting the service.
    """
    assert index(client, project, RUST_V1).status_code == 200
    ollama_knobs(turn_delay_secs=6.0)  # type: ignore[operator]

    with client.stream(
        "POST",
        f"{MINDEX_URL}/v0/{project}/research",
        json={"question": QUESTION, "effort": "low"},
        timeout=30.0,
    ) as resp:
        assert resp.status_code == 200
        # The run names itself in its first frame, before any work. The line
        # iterator is BOUND, not consumed anonymously: breaking out of a bare
        # `resp.iter_lines()` drops the generator, whose close() closes the whole
        # response — httpx hangs up, and the server (correctly) reads that as a
        # disconnect and cancels the very run this test is trying to observe.
        lines = resp.iter_lines()
        run_id = None
        for line in lines:
            if line.startswith("data:"):
                run_id = json.loads(line[len("data:") :])["run_id"]
                break
        assert run_id, "the first frame must carry the run id"

        # It is listed while it runs, with what it is and how long it has been going.
        active = client.get(f"{MINDEX_URL}/research/active").json()
        assert active["slots_busy"] == 1, active
        assert active["slots_total"] >= 1, active
        listed = [r for r in active["runs"] if r["run_id"] == run_id]
        assert listed, active
        assert listed[0]["project_guid"].replace("-", "") == project.replace("-", "")
        assert listed[0]["age_ms"] >= 0 and listed[0]["worst_case_ms"] > 0, listed[0]

        # /health says the same thing, and a busy slot is NOT a degradation.
        health = client.get(f"{MINDEX_URL}/health").json()
        assert health["research"]["slots_busy"] == 1, health
        assert health["status"] == "ok", health

        # And it can be ended by name, without closing this connection.
        assert (
            client.delete(f"{MINDEX_URL}/research/active/{run_id}").status_code == 204
        )

    ollama_knobs(turn_delay_secs=0.0)  # type: ignore[operator]
    deadline = time.monotonic() + 10
    while client.get(f"{MINDEX_URL}/research/active").json()["slots_busy"] > 0:
        assert time.monotonic() < deadline, "a cancelled run never freed its slot"
        time.sleep(0.5)


def test_cancelling_an_unknown_run_is_not_an_error(client: httpx.Client) -> None:
    # "Already finished" and "never existed" are the same observable state a moment
    # later, and neither is something the caller can act on differently.
    resp = client.delete(
        f"{MINDEX_URL}/research/active/00000000-0000-0000-0000-000000000000"
    )
    assert resp.status_code == 204


def test_health_reports_research_slots_when_idle(client: httpx.Client) -> None:
    body = client.get(f"{MINDEX_URL}/health").json()
    assert body["research"]["slots_total"] >= 1, body
    assert body["research"]["slots_busy"] == 0, body
    # Null rather than 0: nothing running is not the same as something running for
    # no time.
    assert body["research"]["oldest_inflight_age_ms"] is None, body


def test_config_publishes_the_research_budgets(client: httpx.Client) -> None:
    # The clients render effort labels from this instead of their own copies —
    # three separate hardcoded ladders had drifted from the server before it.
    cfg = client.get(f"{MINDEX_URL}/config").json()
    research = cfg["research"]
    for level in ("low", "medium", "high"):
        row = research["effort"][level]
        assert row["max_seconds"] > 0 and row["max_tokens"] > 0
        assert row["max_steps"] > 0 and 0 < row["context_fraction"] <= 1.0
    # A ceiling below what an effort level grants would make `effort` unreachable
    # through `budget`; config validation rejects that, so it must hold here.
    assert research["max_request_seconds"] >= research["effort"]["high"]["max_seconds"]
    assert research["max_request_tokens"] >= research["effort"]["high"]["max_tokens"]
    assert research["max_request_steps"] >= research["effort"]["high"]["max_steps"]

    # How many runs may start at once. Without it a caller learns the limit only by
    # being refused, which is no way to plan a queue.
    assert research["max_concurrent"] >= 1, research

    # The wait a caller actually faces: the investigation deadline plus the report
    # window, which bound different phases and were never summed anywhere.
    for level in ("low", "medium", "high"):
        row = research["effort"][level]
        assert (
            row["worst_case_seconds"]
            == row["max_seconds"] + research["report_timeout_ms"] // 1000
        ), row


def test_config_publishes_the_ollama_model_catalog(client: httpx.Client) -> None:
    # A background worker re-reads /api/tags every
    # [research].models_refresh_interval_seconds (5 in the test config) so a client
    # can offer a closed model list. `interval`'s first tick fires at startup, so by
    # the time any test runs the list is populated.
    #
    # A concurrent `tags_down` test cannot flake this: a failed tick keeps the
    # previously published list rather than clearing it.
    cfg = client.get(f"{MINDEX_URL}/config").json()["research"]
    assert "mock-model" in cfg["models"], cfg
    # The timestamp is what separates "Ollama has no models" from "Ollama was never
    # reached" — both of which are an empty `models`.
    assert isinstance(cfg["models_refreshed_at"], int), cfg
    # The whitelist is published raw, and the catalog above is already filtered by
    # it — "mock-model" surviving is the filter keeping what the patterns allow.
    assert cfg["allowed_models"] == ["mock-*"], cfg
    assert all(m.startswith("mock-") for m in cfg["models"]), cfg


def test_busy_second_request_gets_429(
    client: httpx.Client, project: str, ollama_knobs: object
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    ollama_knobs(turn_delay_secs=6.0)  # type: ignore[operator]

    with client.stream(
        "POST",
        f"{MINDEX_URL}/v0/{project}/research",
        json={"question": QUESTION, "effort": "low"},
        timeout=30.0,
    ) as first:
        assert first.status_code == 200
        # The single slot is now held; a second request must be rejected up front.
        second = client.post(
            f"{MINDEX_URL}/v0/{project}/research",
            json={"question": QUESTION, "effort": "low"},
        )
        assert second.status_code == 429
        assert second.json()["code"] == "research.busy"
        # Leaving the `with` closes the first connection → the server cancels.


def test_disconnect_cancels_and_frees_the_slot(
    client: httpx.Client, project: str, ollama_knobs: object
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    ollama_knobs(turn_delay_secs=6.0)  # type: ignore[operator]

    with client.stream(
        "POST",
        f"{MINDEX_URL}/v0/{project}/research",
        json={"question": QUESTION, "effort": "low"},
        timeout=30.0,
    ) as resp:
        assert resp.status_code == 200
        # Abandon immediately — the disconnect is the cancellation interface.

    ollama_knobs(turn_delay_secs=0.0)  # type: ignore[operator]
    # The slot must come free promptly (drop-guard cancellation, not a timeout).
    deadline = time.monotonic() + 10
    while True:
        status, events = research_events(
            client, project, {"question": QUESTION, "effort": "low"}
        )
        if status == 200:
            assert events[-1][0] == "done"
            break
        assert status == 429
        assert time.monotonic() < deadline, "cancelled research never freed its slot"
        time.sleep(0.5)


def test_ollama_failure_becomes_an_error_event(
    client: httpx.Client, project: str, ollama_knobs: object
) -> None:
    assert index(client, project, RUST_V1).status_code == 200
    ollama_knobs(fail_next_chats=1.0)  # type: ignore[operator]

    status, events = research_events(
        client, project, {"question": QUESTION, "effort": "low"}
    )
    assert status == 200, "the failure happens after the stream starts"
    assert events, "an error event must be emitted"
    event, data = events[-1]
    assert event == "error"
    # Ollama's two failure classes are two codes, and the mock produces the second:
    # it *answers*, with a 500. `ollama.error` means Ollama replied with an error —
    # nearly always a model that is not pulled — while `ollama.unavailable` means it
    # could not be reached or stayed mute. Collapsed into one, a client could neither
    # word the message nor decide whether re-reading `/health` would say anything.
    assert "ollama.error" in data, data
    assert "ollama.unavailable" not in data, data


def test_dead_ollama_degrades_health_and_carries_no_detail(
    client: httpx.Client, project: str, ollama_knobs: object
) -> None:
    """Ollama is the one optional dependency, and "degraded" is what that costs.

    It is precisely the state in which a client should keep offering search and
    stop offering research — which is why the verdict has a word for it instead
    of collapsing into the one that means "nothing works".
    """
    ollama_knobs(tags_down=1.0)  # type: ignore[operator]

    body = client.get(f"{MINDEX_URL}/health").json()
    # Exact equality, not a prefix: the reason a probe failed is logged, never
    # returned, and this is the assertion that notices it coming back.
    assert body["checks"]["ollama"] == "error", body
    assert body["status"] == "degraded", body
    assert body["checks"]["sqlite"] == "ok"
    assert body["checks"]["qdrant"] == "ok"
    assert body["checks"]["embedder"] == "ok"

    # The half nothing pinned before: a degraded server is still a working one.
    assert index(client, project, RUST_V1).status_code == 200
    assert search(client, project, "process records in batches").status_code == 200
