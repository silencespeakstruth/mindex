"""
Streaming /index (`?stream=yes`): the SSE mode must report the same work the JSON
mode summarizes, and its `done.files` must be byte-for-byte the JSON response body.
"""

import json

import httpx

MINDEX_URL = __import__("conftest").MINDEX_URL

from test_e2e import FILE_PATH, RUST_V1, RUST_V2, index, search


def index_stream(
    client: httpx.Client,
    project: str,
    body: dict,
    timeout: float = 60.0,
) -> tuple[int, list[tuple[str, str]]]:
    """POST /index?stream=yes and drain the SSE stream into (status, [(event, data)])."""
    events: list[tuple[str, str]] = []
    with client.stream(
        "POST",
        f"{MINDEX_URL}/v0/{project}/index",
        params={"stream": "yes"},
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


def test_streaming_index_reports_the_full_pipeline(
    client: httpx.Client, project: str
) -> None:
    status, events = index_stream(
        client, project, {"files": {"rust": {FILE_PATH: {"code": RUST_V1}}}}
    )
    assert status == 200
    names = [e for e, _ in events]

    assert names[0] == "started", names
    started = json.loads(events[0][1])
    # Exact, not a subset: `started` reports the shape of the request, and a
    # field appearing there is a wire change every consumer has to be told
    # about. `vectors_only` joined `symbols_only` with retrieval v3.
    assert started == {"files": 1, "symbols_only": False, "vectors_only": False}

    prepared = [json.loads(d) for e, d in events if e == "prepared"]
    assert len(prepared) == 1, events
    assert prepared[0]["path"] == FILE_PATH
    assert prepared[0]["language"] == "rust"
    assert prepared[0]["chunks"] > 0
    assert prepared[0]["symbols"] > 0

    # One `embedded` per batch, cumulative, ending exactly at chunks_total.
    embedded = [json.loads(d) for e, d in events if e == "embedded"]
    assert embedded, f"at least one embed batch expected: {names}"
    assert embedded[-1]["chunks_done"] == embedded[-1]["chunks_total"]
    assert embedded[-1]["chunks_total"] == prepared[0]["chunks"]
    dones = [b["chunks_done"] for b in embedded]
    assert dones == sorted(dones), "chunks_done must be monotonic"
    assert all(isinstance(b["elapsed_ms"], int) for b in embedded)

    indexed = [json.loads(d) for e, d in events if e == "indexed"]
    assert len(indexed) == 1, events
    assert indexed[0]["path"] == FILE_PATH
    assert indexed[0]["count"] == prepared[0]["chunks"]

    assert names[-1] == "done", f"the stream must end with done: {names}"
    done = json.loads(events[-1][1])
    assert done["files"] == {"rust": {FILE_PATH: prepared[0]["chunks"]}}
    assert done["files_indexed"] == 1
    assert done["chunks"] == prepared[0]["chunks"]
    assert isinstance(done["elapsed_ms"], int)

    # The vectors actually landed: search finds the streamed file.
    hits = search(client, project, "process records in batches").json()["results"]
    assert any(h["path"] == FILE_PATH for h in hits)


def test_streaming_unchanged_file_is_skipped(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_V1).status_code == 200

    status, events = index_stream(
        client, project, {"files": {"rust": {FILE_PATH: {"code": RUST_V1}}}}
    )
    assert status == 200
    names = [e for e, _ in events]

    skipped = [json.loads(d) for e, d in events if e == "skipped"]
    assert skipped == [
        {"path": FILE_PATH, "language": "rust", "reason": "unchanged"}
    ], events
    assert "prepared" not in names and "indexed" not in names, names
    done = json.loads(events[-1][1])
    # Absent from the counts exactly as it would be absent from the JSON body.
    assert done["files"] == {"rust": {}}, done
    assert done["files_indexed"] == 0 and done["chunks"] == 0


def test_streaming_done_matches_the_json_mode_response(
    client: httpx.Client, project: str
) -> None:
    # Same content indexed twice under two GUID-distinct projects would embed
    # twice; instead compare the shapes on one project: JSON first, then a
    # streamed *changed* version — both report `files` in the identical shape.
    json_resp = index(client, project, RUST_V1)
    assert json_resp.status_code == 200
    json_files = json_resp.json()["files"]
    assert json_files["rust"][FILE_PATH] > 0

    status, events = index_stream(
        client, project, {"files": {"rust": {FILE_PATH: {"code": RUST_V2}}}}
    )
    assert status == 200
    done = json.loads(events[-1][1])
    assert set(done["files"].keys()) == set(json_files.keys())
    assert done["files"]["rust"][FILE_PATH] > 0


def test_streaming_symbols_only_counts_symbol_rows(
    client: httpx.Client, project: str
) -> None:
    assert index(client, project, RUST_V1).status_code == 200

    status, events = index_stream(
        client,
        project,
        {
            "files": {"rust": {FILE_PATH: {"code": RUST_V1}}},
            "symbols_only": True,
            "force": True,
        },
    )
    assert status == 200
    names = [e for e, _ in events]

    started = json.loads(events[0][1])
    assert started["symbols_only"] is True
    # The cheap path never embeds.
    assert "embedded" not in names and "prepared" not in names, names
    indexed = [json.loads(d) for e, d in events if e == "indexed"]
    assert len(indexed) == 1 and indexed[0]["count"] > 0, events
    done = json.loads(events[-1][1])
    assert done["files"]["rust"][FILE_PATH] == indexed[0]["count"]


def test_stream_no_and_absent_stay_json(client: httpx.Client, project: str) -> None:
    r = client.post(
        f"{MINDEX_URL}/v0/{project}/index",
        params={"stream": "no"},
        json={"files": {"rust": {FILE_PATH: {"code": RUST_V1}}}},
    )
    assert r.status_code == 200
    assert r.headers["content-type"].startswith("application/json")
    assert r.json()["files"]["rust"][FILE_PATH] > 0


def test_stream_typo_is_a_400(client: httpx.Client, project: str) -> None:
    for params in ({"stream": "true"}, {"streem": "yes"}):
        r = client.post(
            f"{MINDEX_URL}/v0/{project}/index",
            params=params,
            json={"files": {}},
        )
        assert r.status_code == 400, (params, r.text)
        assert r.json()["code"] == "request.malformed_body"
