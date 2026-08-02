"""
Scripted Ollama mock for the /research integration tests.

Speaks just enough of Ollama's `POST /api/chat` (streaming NDJSON) for the
research loop: the reply is chosen from the conversation shape, so no state is
carried between requests and parallel tests can't interfere:

  - the final-report turn (the last user message asks for the report) streams a
    thinking delta plus a fixed Markdown summary in several content chunks;
  - the first tool turn (no tool results in the conversation yet) answers with a
    `search` action that reuses the research question as the query;
  - any later tool turn answers `finalize`.

It also answers ``GET /api/tags``, which mindex reads for two things: the liveness
ping behind the optional `ollama` check in ``GET /health``, and the model catalog a
background worker refreshes for ``GET /config``'s ``research.models``. So the
payload is load-bearing, not a stub — ``tags_down`` therefore also exercises "a
failed catalog tick keeps the previously published list".

Test knobs via ``POST /config`` (reset by posting zeros):
  turn_delay_secs — sleep at the start of every /api/chat; widens the window a
                    research slot stays busy (429 / cancellation tests).
  fail_next_chats — number of /api/chat calls to fail with HTTP 500; drives the
                    stream's in-band `error` event path.
  tags_down       — non-zero makes /api/tags fail with HTTP 503, so /health sees
                    a dead Ollama (and must still report status "ok") and the
                    catalog worker's tick fails.
"""

import asyncio
import json
from collections.abc import AsyncIterator
from typing import Any

# fastapi is only present inside this component's Docker image, never alongside
# the local/CI mypy run, so its stubs are legitimately unresolvable here.
from fastapi import FastAPI, HTTPException  # type: ignore[import-not-found]
from fastapi.responses import StreamingResponse  # type: ignore[import-not-found]

app = FastAPI()

_config: dict[str, float] = {
    "turn_delay_secs": 0.0,
    "fail_next_chats": 0.0,
    "tags_down": 0.0,
    # Emit calls as JSON in `content` instead of native `tool_calls`, imitating a
    # model whose Ollama template has no tool support (observed for real on
    # qwen2.5-coder:32b). Exercises the server's fallback parser.
    "force_text_calls": 0.0,
}

# An explicit action script, one entry per tool turn, consumed in order. Empty =
# the default heuristic below (one `search`, then `finalize`). This is the only
# way to exercise actions the heuristic never emits, such as outline/list_files.
_script: list[dict[str, Any]] = []

# Report-turn override, consumed in order (draft, then rewrite); the last entry
# repeats. Empty = the fixed SUMMARY_CHUNKS. This is the only way to exercise the
# server's markdown gate (a broken draft) and the stored-title extraction (a
# custom heading). Cleared by every /script call that does not set it.
_report_chunks: list[list[str]] = []
_reports_served = 0

SUMMARY_CHUNKS = [
    "# Mock Report\n\n",
    "The research question was answered from the indexed code.\n\n",
    "Evidence: `src/pipeline.rs:1-10`.\n",
]


@app.post("/config")
async def config(body: dict[str, float]) -> dict[str, float]:
    _config.update({k: float(v) for k, v in body.items() if k in _config})
    return _config


@app.post("/script")
async def script(body: dict[str, Any]) -> dict[str, int]:
    """Set (or clear, with an empty list) the scripted action sequence, and
    optionally the report turns' texts (a list of strings, one per report turn)."""
    global _reports_served
    _script.clear()
    _script.extend(body.get("actions", []))
    _report_chunks.clear()
    _report_chunks.extend([[text] for text in body.get("reports", [])])
    _reports_served = 0
    return {"actions": len(_script), "reports": len(_report_chunks)}


@app.post("/api/show")
async def show(body: dict[str, Any]) -> dict[str, Any]:
    """The slice of /api/show mindex reads: the model's own context length, which
    the server clamps its configured num_ctx to. Namespaced key, as real Ollama
    reports it."""
    return {"model_info": {"mockarch.context_length": 32768}}


@app.get("/api/tags")
async def tags() -> dict[str, Any]:
    if _config["tags_down"] > 0:
        raise HTTPException(status_code=503, detail="scripted outage")
    return {"models": [{"name": "mock-model", "model": "mock-model"}]}


def _line(payload: dict[str, Any]) -> str:
    return json.dumps(payload) + "\n"


def _chunk(
    content: str = "",
    thinking: str | None = None,
    done: bool = False,
    tool_calls: list[dict[str, Any]] | None = None,
) -> str:
    message: dict[str, Any] = {"role": "assistant", "content": content}
    if thinking is not None:
        message["thinking"] = thinking
    if tool_calls is not None:
        message["tool_calls"] = tool_calls
    payload: dict[str, Any] = {"message": message, "done": done}
    if done:
        # Real Ollama reports both counts on the final line only; the server
        # reads them for its per-run token tally and truncation warning.
        payload["prompt_eval_count"] = 512
        payload["eval_count"] = 32
        # Nanoseconds, as real Ollama reports them. Without these the server's
        # throughput fields stay at zero and no integration test can see them —
        # 32 tokens in 2 s is 16 tok/s, an unremarkable healthy rate.
        payload["load_duration"] = 10_000_000
        payload["prompt_eval_duration"] = 500_000_000
        payload["eval_duration"] = 2_000_000_000
        payload["total_duration"] = 2_600_000_000
    return _line(payload)


def _prose(text: str) -> list[str]:
    """One toolless turn answering in plain content — a plan or a verdict."""
    return [_chunk(content=text), _chunk(done=True)]


_TOOL_RESULT_MARKS = (
    " file(s) matching ",
    "No indexed file matches",
    "is not an indexed file",
    "declares no symbols",
)


def _is_tool_result(text: str) -> bool:
    """Whether a user message is a tool result the server fed back (as opposed to
    the question or an instruction). Mirrors research.rs's formatters."""
    # Some formatters lead with the path or a count, so not every marker can be a
    # prefix; `_TOOL_RESULT_MARKS` are matched anywhere in the first line.
    if text.startswith(
        ("Results for", "No results", "Symbol ", "No symbol", "Outline of")
    ):
        return True
    head = text.split("\n", 1)[0]
    return any(m in head for m in _TOOL_RESULT_MARKS)


async def _stream(lines: list[str]) -> AsyncIterator[bytes]:
    for line in lines:
        yield line.encode()
        await asyncio.sleep(0)  # flush chunk boundaries deterministically


@app.post("/api/chat")
async def chat(body: dict[str, Any]) -> StreamingResponse:
    if _config["turn_delay_secs"] > 0:
        await asyncio.sleep(_config["turn_delay_secs"])
    if _config["fail_next_chats"] > 0:
        _config["fail_next_chats"] -= 1
        raise HTTPException(status_code=500, detail="scripted failure")

    messages: list[dict[str, Any]] = body.get("messages", [])
    user_texts = [m.get("content", "") for m in messages if m.get("role") == "user"]
    last_user = user_texts[-1] if user_texts else ""

    # Three turns are sent WITHOUT tools — that is the production contract
    # (omitting `tools` is how "there is nothing to call" is expressed): the plan
    # turn before the loop, the sufficiency check after it, and the report itself.
    # They are told apart the same way production tells them apart, by what was
    # last asked.
    if not body.get("tools") and last_user.startswith("Before you touch a tool:"):
        return StreamingResponse(
            _stream(_prose("1. What does the pipeline do? — src/pipeline.rs")),
            media_type="application/x-ndjson",
        )
    if not body.get("tools") and last_user.startswith("The investigation is paused."):
        # Nothing left open, so the loop is not re-entered: a mock that asked for
        # more work would make every integration test's step count depend on it.
        return StreamingResponse(
            _stream(_prose("1. ANSWERED src/pipeline.rs:1-10")),
            media_type="application/x-ndjson",
        )

    # The report turn. The text check stays as a belt for the fallback path.
    if not body.get("tools") or "final report" in last_user:
        global _reports_served
        if _report_chunks:
            idx = min(_reports_served, len(_report_chunks) - 1)
            chunks = _report_chunks[idx]
            _reports_served += 1
        else:
            chunks = SUMMARY_CHUNKS
        lines = [_chunk(thinking="composing the report")]
        lines += [_chunk(content=c) for c in chunks]
        lines.append(_chunk(done=True))
        return StreamingResponse(_stream(lines), media_type="application/x-ndjson")

    # Tool turns: a `search` first (echoing the research question as the query),
    # `finalize` once any tool result is already in the conversation.
    question = next(
        (
            t.removeprefix("Research question:\n")
            for t in user_texts
            if t.startswith("Research question:")
        ),
        "mock",
    )
    # Where we are in the session: one tool result per executed call. Native
    # results arrive as role:"tool" messages, fallback ones as user text.
    done = sum(1 for m in messages if m.get("role") == "tool") or sum(
        1 for t in user_texts if _is_tool_result(t)
    )
    if _script:
        action = _script[done] if done < len(_script) else {"action": "finalize"}
    else:
        action = (
            {"action": "finalize"}
            if done
            else {"action": "search", "query": question.strip()}
        )

    # Native tool calls unless the server sent no `tools` (it does that only for
    # the report turn, handled above) or the text-fallback knob is set.
    native = bool(body.get("tools")) and _config["force_text_calls"] <= 0
    if native:
        name = action.pop("action")
        lines = [
            _chunk(thinking="choosing the next step"),
            _chunk(
                tool_calls=[
                    {
                        "id": f"call_{done}",
                        "function": {"index": 0, "name": name, "arguments": action},
                    }
                ]
            ),
            _chunk(done=True),
        ]
    else:
        lines = [
            _chunk(thinking="choosing the next step"),
            _chunk(content=json.dumps(action)),
            _chunk(done=True),
        ]
    return StreamingResponse(_stream(lines), media_type="application/x-ndjson")
