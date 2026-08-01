"""scout MCP server — one tool, ``research``, that does the caller's codebase
investigation *off* the caller's context window.

The caller (an expensive frontier model) sends one thing: a question. The
mindex server then runs the whole investigation locally — a local Ollama model
queries the index step by step (semantic search + exact symbol lookup), reads
the matching code, and writes a report. Only that report and a one-line-per-step
trace cross the MCP boundary back.

    question (a few dozen tokens)  →  mindex POST /research (SSE)
                                   →  local model loops search/symbols, reads code
                                   →  Markdown report + step trace come back

The raw chunks — and the local model's thinking, which can run to thousands of
tokens — never reach the caller. That is where the saving comes from: the caller
pays for a question in and a briefing out, while the reading runs on local
hardware for free.

This server is a thin SSE client; all the machinery lives in mindex itself
(``src/research.rs``). It is a sibling of ``tools/mcp/mindex`` (raw search +
symbols), not a replacement: when the caller needs verbatim code to *edit*, that
is what the raw tools are for.
"""

from __future__ import annotations

import json
import os
from typing import Any

import httpx
from mcp.server.fastmcp import FastMCP

# ── mindex — same MINDEX_* conventions as tools/mcp/mindex & mindex-search.sh ──
SERVER = os.environ.get("MINDEX_SERVER", "https://127.0.0.1:11111").rstrip("/")
PROTOCOL = os.environ.get("MINDEX_PROTOCOL", "v0")

# Research is a long job: the local model takes several turns, each of which may
# think for a while. mindex sends an SSE keep-alive every 15 s, so the read
# timeout only has to outlast that, while the total budget has to outlast the
# whole investigation.
CONNECT_TIMEOUT = float(os.environ.get("RESEARCH_CONNECT_TIMEOUT", "10"))
READ_TIMEOUT = float(os.environ.get("RESEARCH_READ_TIMEOUT", "120"))
# Must outlast the server's own worst case, which is `effort.high.max_seconds` plus
# `report_timeout_ms` — 3600 + 120 s at the shipped defaults. This is a CLIENT
# ceiling and it has no idea what the server is configured for, so it is set above
# the default ladder with room to spare: too low and every high-effort run dies in
# flight, which is exactly what happened when the ladder was raised and this was
# left at 1800.
TOTAL_TIMEOUT = float(os.environ.get("RESEARCH_TOTAL_TIMEOUT", "3900"))

# Effort used when the caller doesn't say. It selects a server-configured budget
# (wall-clock, local tokens and tool calls), so it is also the main latency lever.
# The numbers live in the server's [research.effort.*] and are deliberately not
# repeated here: three separate copies of them had drifted before this comment.
DEFAULT_EFFORT = os.environ.get("RESEARCH_DEFAULT_EFFORT", "medium")

_EFFORTS = {"low", "medium", "high"}
_TRUTHY = {"1", "true", "yes", "on"}

# Fields kept from a `step` event. Each action names its argument differently —
# search/grep/symbols/outline/list_files/note/revise_plan/read_research →
# query/pattern/name/path/glob/text/plan/seq (list_research reuses query) — and
# dropping an unknown key would silently blank the trace for that action rather
# than error. A new tool on the server means a new key here, in the same commit.
_STEP_KEYS = (
    "n",
    "action",
    "query",
    "pattern",
    "name",
    "path",
    "glob",
    "text",
    "plan",
    "seq",
    "hits",
)

# Fields kept from the last `progress`/`done` snapshot, reported as `usage`. Only
# the last one is kept: this layer exists to save the caller's tokens, and a
# per-step consumption trace is exactly the kind of bulk it is here to prevent.
# `binding` says which budget axis was closest to exhausted, which is what makes
# "the run nearly ran out" visible *before* it becomes a `done_reason`.
_USAGE_KEYS = (
    "steps",
    "max_steps",
    "elapsed_ms",
    "max_ms",
    "tokens",
    "max_tokens",
    "prompt_tokens",
    "eval_tokens",
    "turns",
    "binding",
    # Which prompt generation drove the run. Cheap to carry and impossible to
    # recover later: two reports written under different instructions are not
    # comparable, and without this a prompt change reads as model variance.
    "prompt_version",
)

# Fields kept from the `citations` event — the server's provenance check on the
# report's `path:start-end` references. This is what makes "trust the report,
# don't spot-check it" an honest instruction rather than a hope: the counts say
# how much of the report is backed by locations the investigation actually saw,
# and `unverified_paths` names the ones it did not.
# `stale`/`stale_paths` are the freshness half, and independent of the counts above:
# a run reads the index for minutes while mindex-index and mindex-watch keep writing
# to it (indexing is never blocked by research), so a citation the investigation
# really did see can still point into a file that has since been reindexed.
# `draft_*` and `revalidation_steps` are set only when the first draft failed this
# same check and was sent back for correction. The counts above always describe the
# report that shipped, so these are the only way to tell a report that was right
# the first time from one that had to be repaired.
# `server_written` has to be read BEFORE any of the counts. A report the server
# assembled (the report window expired before the model produced one) contains no
# `path:start-end` at all, so it scores total/verified/unverified = 0 — byte-for-byte
# what a perfectly clean report scores, in the one field this layer tells the caller
# to trust. Field reports of "verified: 0 even though it read the files" are that
# collision and nothing else.
_CITATION_KEYS = (
    "server_written",
    "total",
    "verified",
    "path_only",
    "unverified",
    "unverified_paths",
    "stale",
    "stale_paths",
    "draft_unverified",
    "draft_path_only",
    "draft_stale",
    "revalidation_steps",
)

# Fields kept from one entry of the `excerpts` event: the indexed code at a verified
# citation, verbatim, read from the index rather than written by the model.
_EXCERPT_KEYS = ("path", "start_line", "end_line", "code")

# mindex's `done` event says why the loop stopped. Anything but "finalized" means
# the report was written on partial evidence, and the caller — who is told not to
# re-verify reports — can only know that if we say so.
# Mid-stream failures worth explaining to the caller rather than just raising.
_ERROR_HINTS = {
    "research.model_lacks_tools": (
        "The configured local model cannot call tools (its Ollama template lacks "
        "support). This is a server configuration problem, not something to retry: "
        "report it and stop."
    ),
}

_INCOMPLETE_HINTS = {
    "time_exhausted": (
        "The local model ran out of its wall-clock budget before it was satisfied. "
        "Re-ask at a higher effort, or split the question into narrower ones."
    ),
    "context_exhausted": (
        "The evidence filled the local model's context window, so the investigation "
        "was stopped before it could read more. Ask a narrower question — a higher "
        "effort will not help, and may hit the same wall sooner."
    ),
    "tokens_exhausted": (
        "The local model spent its whole token budget before it was satisfied — its "
        "transcript grew faster than the evidence it gathered. Split the question "
        "into narrower ones rather than re-asking this one at a higher effort."
    ),
    "budget_exhausted": (
        "The local model used up its lookup budget before it was satisfied. "
        "Re-ask at a higher effort, or split the question."
    ),
    "unparseable": (
        "The local model broke protocol and was forced to write the report early. "
        "Re-ask; if it repeats, the configured model is a poor fit for this loop."
    ),
    "repeated_calls": (
        "The local model kept repeating the same lookups and was stopped. Its "
        "queries were probably not finding the material — re-ask more concretely, "
        "naming files or identifiers if you know any."
    ),
}


def _verify() -> bool | str:
    """TLS verification for mindex, mirroring tools/mcp/mindex: a CA-bundle path
    if ``MINDEX_CACERT`` is set, else off when ``MINDEX_NO_VERIFY`` is truthy
    (the self-signed cert), else on."""
    cacert = os.environ.get("MINDEX_CACERT")
    if cacert:
        return cacert
    return os.environ.get("MINDEX_NO_VERIFY", "").lower() not in _TRUTHY


def _headers() -> dict[str, str]:
    """``X-Api-Key`` when ``MINDEX_API_KEY`` is set, nothing otherwise.

    mindex has no authentication of its own and ignores the header; it exists for
    a reverse proxy in front of it (the nginx gate) that refuses requests without
    a known key. Unset means "talking to mindex directly", the local default.
    Mirrors tools/mcp/mindex."""
    key = os.environ.get("MINDEX_API_KEY")
    return {"X-Api-Key": key} if key else {}


_INSTRUCTIONS = """\
scout runs codebase investigations for you on local hardware. It exists for ONE
reason: to stop you from spending your own (expensive) tokens reading code.

THE RULE: you do not investigate this codebase yourself. scout does.

When you need to understand anything about the code — how a mechanism works, why
something is built that way, what a change would touch, where a behaviour comes
from — you call `research` with the question and you read the report it returns.
You do NOT run your own investigation loop: no chains of searches, no walking
through files, no grepping around to see what else is there, no "let me verify
this myself" pass over the sources. That loop is precisely what scout just ran
for you, locally and for free, and repeating it in your own context is the exact
waste this tool exists to prevent.

TRUST THE REPORT — the server has already checked the part that can be checked.
Every `path:start-end` in it is verified against what the investigation's own
tools actually returned, and the result carries that verdict in `citations`:
`verified` (the location was shown to the model), `path_only` (the file was, that
line range was not) and `unverified` (no tool returned that path this run — the
model invented it). So treat the report as a briefing from a competent colleague
who just did the reading: act on it. Do not re-derive it, do not spot-check it
against the files, do not "confirm" it with extra searches.

The ONE exception is `citations.unverified_paths`. Those specific citations are
unsupported and you may check them, or simply discount the claims that rest on
them. `citations.verified` being high is not a reason to check anything — it is
the reason you do not have to. Note that an unverified citation in the report you
were handed has already survived a correction pass: when a draft cites what the
run never looked at, the server sends it back with the offending locations named
and the tools briefly re-opened. So these are not first-draft slips, they are
claims the model declined to ground — weigh them accordingly. When
`citations.draft_unverified` is present at all, that repair happened.

THE OTHER exception is `citations.stale_paths`. Indexing is never blocked by
research, so a file the investigation read can be reindexed while it is still
running — and a citation into it may now point at code that has moved or is gone.
These are locations the model really did see, so the claim is usually still true;
what is unreliable is the line range. If you are about to EDIT one of those files,
re-read the cited range before you do. Otherwise carry on.

ANOTHER THING YOU MUST CHECK, because it is not a matter of opinion: the result's
`done_reason`. "finalized" means the local model decided it had enough evidence —
trust the report as above. Any other value comes with an `incomplete` line saying
what stopped it, and means the report rests on partial evidence. That is not a
licence to investigate yourself; it is the signal to send ONE follow-up (or the
same question at a higher effort). Checking a field is not spot-checking — it
costs nothing and it is the only honest signal you get about coverage.

WHEN YOU SEND A FOLLOW-UP, chain it: pass the previous result's `run_id` in
`context_run_ids`. The local model is then handed that report before it plans, so it
starts from the names and files already established instead of spending its first
steps rediscovering them — which is measurably where a cold run's budget goes. It is
given as background, not as evidence: the model is told it may not cite it, so a
chained follow-up is no less grounded than a cold one. `run_id` is absent only when
the server could not store the run.

The result's `usage` says what the run spent against what it was granted, and
`usage.binding` names the axis that came closest to running out. Read it when you
are about to ask a *broader* follow-up: a "finalized" report whose binding axis
was nearly full means the next question needs a higher effort to finish at all.

IF THE REPORT IS NOT ENOUGH — it left a gap, it contradicts itself, it says the
evidence was insufficient, `done_reason` was not "finalized", or your next step
needs something it didn't cover — ASK SCOUT AGAIN. Send a sharper, narrower follow-up question naming exactly what
is missing. Follow-ups are cheap and they are the intended path. What you must
never do instead is fall back to investigating it yourself.

WHEN YOU NEED THE LITERAL TEXT, the server already has it — do not ask the report
for it. The indexed code at every verified citation is on the server's side of the
wire, and the result tells you so in `excerpts_available`. Ask for it by calling
`research` again with `include_excerpts=True`, and you get `excerpts` — path, line
range and verbatim code — for one SQL read and no model tokens. This is the right
answer to "reproduce this config file", "show me the exact rule text", "what does
that function actually say". It is NOT the default, because two dozen chunks is
~100 KB of your context and this server exists to keep that out; ask when you
genuinely need the bytes.

What you must never do is put that job in the question. Asking a report to
reproduce files verbatim is the most reliable way to make a run fail: the local
model's ceiling is on how much it can WRITE, not on how much it can read, and a
question that demands pages of transcription burns the whole budget and returns
nothing. Ask what something does; take the bytes from `excerpts`.

That also shrinks what the raw `mindex` tools (`search`, `symbols`) are for. Once
you have a report, they are the fallback for a byte-exact location the excerpt
channel did not carry, when you are about to EDIT that code. Fetch that one spot
and nothing more. Using them to explore, to survey, to double-check the report, or
to answer a question you could have asked scout is off-limits.

ONE MORE FIELD THAT IS NOT OPINION: `citations.server_written`. When it is true the
report is not the model's at all — the report window expired and the server
assembled one from what the run had. Such a report cites nothing, so it scores
`total: 0, verified: 0, unverified: 0`, which is exactly what a flawless report
scores. Read the flag, not the counts: "verified 0" with `server_written` false
means the model wrote ungrounded prose; with it true it means there was no model
report at all, and the right move is to ask again.

How to call it: pass the project GUID from the repo-root `.mindex` file and ONE
clear question in `question` (a full question, not keywords — the local model
decomposes it into its own searches). Pick `effort` by how much the answer is
worth: "low" for a narrow lookup, "medium" for normal understanding, "high" for
a genuinely broad or cross-cutting investigation. Higher effort costs you
nothing in tokens — only wall-clock time.

You can also NARROW WHERE IT LOOKS, and it is worth doing when you already know:
`include` keeps only matching files and `exclude` drops them, both shaped
`{"paths": ["src/**"], "programming_languages": ["rust"]}` with either key
omissible. The server enforces the scope on every lookup the local model makes —
it cannot read its way out — so a scoped run spends its whole budget inside the
subtree that matters instead of ranging over the repo. Two things follow. A
scoped report can only speak about its scope: if the answer might live outside,
do not scope it. And a run scoped to the wrong place will say so rather than
inventing an answer, so read the report's own caveats. Standing project-wide
scope belongs in the repo-root `.mindex` file, not in every call.

If a call returns a connection error, mindex or its local model is down: report
that and stop. Don't retry blindly, and don't substitute your own investigation.
"""

mcp = FastMCP("scout", instructions=_INSTRUCTIONS)


def _problem(resp_text: str, status: int) -> str:
    """Render mindex's problem+json (or whatever came back) as one line."""
    try:
        body = json.loads(resp_text)
        code = body.get("code", f"http.{status}")
        detail = body.get("detail") or body.get("title") or ""
        return f"{code}: {detail}".strip().rstrip(":")
    except json.JSONDecodeError:
        return f"http.{status}: {resp_text[:200]}"


@mcp.tool()
async def research(
    project_guid: str,
    question: str,
    effort: str = DEFAULT_EFFORT,
    model: str | None = None,
    include: dict[str, Any] | None = None,
    exclude: dict[str, Any] | None = None,
    budget: dict[str, Any] | None = None,
    context_run_ids: list[str] | None = None,
    include_excerpts: bool = False,
) -> dict:
    """Investigate the codebase and return a report — without spending your tokens.

    THIS IS HOW YOU LEARN ANYTHING ABOUT THIS CODEBASE. A local model runs the
    whole investigation on local hardware: it queries the index step by step
    (semantic search + exact symbol lookup), reads the matching code, and writes
    a cited Markdown report. Only the report and a compact step trace come back —
    the code it read, and its own reasoning, never enter your context. That is
    the entire point: one question in, one briefing out.

    TRUST THE REPORT and act on it. Do not re-verify it by reading files or by
    running your own searches — that throws away everything this call just saved.

    IF IT ISN'T ENOUGH, CALL THIS TOOL AGAIN with a sharper follow-up question
    naming exactly what was missing. Follow-ups are the intended path. Never
    substitute your own investigation loop. When you do, pass the previous call's
    ``run_id`` in ``context_run_ids``: the local model then starts from what it
    already established instead of rediscovering the same file names, which is where
    a cold run spends its first steps.

    NEED THE LITERAL TEXT of something the report cites? Call this again with
    ``include_excerpts=True`` and the same question, or read ``excerpts_available``
    on the result: the server holds the indexed code at every verified citation and
    hands it over verbatim for one SQL read and no model tokens. That is cheaper
    than `mindex.search`, and far cheaper than asking a report to reproduce a file —
    which is the single most reliable way to make a run fail. Never open the files
    yourself.

    The raw `mindex` tools are only for pulling the byte-exact text of a location
    this report already cited when the excerpt channel did not carry it,
    immediately before you edit that code.

    Args:
        project_guid: The project's mindex GUID (from the repo-root .mindex file).
        question: ONE full question, in plain language. Not keywords — the local
            model does its own query decomposition. A precise question ("how are
            deleted chunks reclaimed, and what stops orphaned vectors?") gets a
            precise report; a vague one gets a vague one.
        effort: "low" (narrow lookup), "medium" (default, normal understanding),
            or "high" (broad, cross-cutting). Selects the server-configured budget
            — wall-clock, local tokens and index lookups, whichever runs out first.
            Costs you no tokens, only wall-clock time.
        model: Optional Ollama model override; omit to use the server's default.
        include: Optional scope to KEEP for every lookup, as
            ``{"paths": ["src/**", ...], "programming_languages": ["rust", ...]}``.
            Standing project scope lives in the repo-root `.mindex` file.
        exclude: Optional scope to DROP, same shape (e.g. ``{"paths": ["tools/**"]}``).
        budget: Optional per-axis override of the effort preset, as
            ``{"max_seconds": 1800, "max_tokens": 3000000, "max_steps": 40}`` — any
            subset; an omitted axis keeps the preset. Reach for it when `effort`
            alone is the wrong shape (a question that needs time but not depth, say).
            The server owns the ceilings and rejects an over-large value with a 400;
            they are deliberately not duplicated here, because three separate copies
            of these numbers have drifted from the server before.
        context_run_ids: Optional ``run_id`` values of earlier runs on this project
            whose reports should be handed to this one as background — normally the
            ``run_id`` a previous call returned. They save the new run the work of
            rediscovering names, and they are NOT evidence: the local model is told it
            may not cite them, and anything it copies from one is reported back as
            ``unverified``. The server caps how many may be given.
        include_excerpts: Return the verbatim indexed code at every verified
            citation, not just the count of it. Default False on purpose: two dozen
            chunks is ~100 KB landing in YOUR context, which is the cost this server
            exists to prevent. Set it when you actually need the literal text —
            about to edit that code, or reproducing a config/schema file — and leave
            it off when you only need to understand something.

    Returns ``{"report", "steps": [{n, action, query|name|path|glob, hits}],
    "elapsed_ms", "done_reason", "usage"}``. ``steps`` is just the trace of what
    the local model looked at — read it only if you need to judge how well the
    question was covered.

    ``done_reason`` is "finalized" when the local model judged the evidence
    sufficient. Any other value means it was stopped
    ("time_exhausted" / "tokens_exhausted" / "budget_exhausted" /
    "context_exhausted" / "unparseable" / "repeated_calls"), an ``"incomplete"``
    key explains which, and the report rests on partial evidence — the one case
    where a follow-up question (or the same question at a higher effort) is worth
    the wall-clock rather than a waste of it.

    ``usage`` is what the run spent against what it was granted (time, local
    tokens, steps, turns) plus ``binding``: the axis that came closest to
    exhausted. On a "finalized" report a nearly-exhausted axis means the next,
    broader question needs a higher effort to finish at all.

    ``run_id`` (with a short per-project ``seq``) names the stored report. Pass it as
    ``context_run_ids`` on a follow-up so the next run reads this one first. Absent
    when the server could not store the run, which only means it cannot be referenced.

    ``excerpts_available`` counts the verified citations whose verbatim indexed code
    the server is holding. With ``include_excerpts=True`` that code comes back in
    ``excerpts`` as ``[{path, start_line, end_line, code}]`` (``excerpts_truncated``
    if the server's caps dropped some). This is how you get literal text — never by
    asking the report to reproduce it.
    """
    question = question.strip()
    if not question:
        raise ValueError("question must not be empty")
    if effort not in _EFFORTS:
        raise ValueError(f"effort must be one of {sorted(_EFFORTS)}, got {effort!r}")

    body: dict[str, Any] = {"question": question, "effort": effort}
    if model:
        body["model"] = model
    if include:
        body["include"] = include
    if exclude:
        body["exclude"] = exclude
    if budget:
        body["budget"] = budget
    if context_run_ids:
        body["context_run_ids"] = context_run_ids
    # `seed` is deliberately NOT exposed. An agent has no use for repeatability, and
    # a pinned seed would make the "ask again, sharper" path this tool's own
    # instructions sell return the same report for the same question. Do not add it
    # as an oversight-fix.

    url = f"{SERVER}/{PROTOCOL}/{project_guid}/research"
    timeout = httpx.Timeout(TOTAL_TIMEOUT, connect=CONNECT_TIMEOUT, read=READ_TIMEOUT)

    report_parts: list[str] = []
    steps: list[dict[str, Any]] = []
    usage: dict[str, Any] = {}
    citations: dict[str, Any] = {}
    excerpts: list[dict[str, Any]] = []
    excerpts_truncated = False
    elapsed_ms = 0
    done_reason: str | None = None
    run_id: str | None = None
    run_seq: int | None = None
    failure: str | None = None

    try:
        async with (
            httpx.AsyncClient(
                verify=_verify(), timeout=timeout, headers=_headers()
            ) as client,
            client.stream("POST", url, json=body) as resp,
        ):
            if resp.status_code != 200:
                await resp.aread()
                raise RuntimeError(
                    f"mindex research failed — {_problem(resp.text, resp.status_code)}"
                )
            event = "message"
            async for line in resp.aiter_lines():
                if line.startswith("event:"):
                    event = line[len("event:") :].strip()
                    continue
                if not line.startswith("data:"):
                    # blank frame separators and ":" keep-alive comments
                    continue
                try:
                    data = json.loads(line[len("data:") :].strip())
                except json.JSONDecodeError:
                    continue
                if event == "summary":
                    report_parts.append(data.get("text", ""))
                elif event == "step":
                    steps.append({k: v for k, v in data.items() if k in _STEP_KEYS})
                elif event == "progress":
                    # Overwritten, never appended: only the latest snapshot matters.
                    usage = {k: v for k, v in data.items() if k in _USAGE_KEYS}
                elif event == "done":
                    elapsed_ms = int(data.get("elapsed_ms", 0))
                    # `done` repeats every progress field, so the final snapshot is
                    # the run's whole cost.
                    usage = {k: v for k, v in data.items() if k in _USAGE_KEYS} or usage
                    # "finalized" means the local model judged the evidence
                    # sufficient; anything else means the report was written on
                    # what it had when the loop was stopped. Absent on servers
                    # older than the reason taxonomy — treat that as unknown, not
                    # as complete.
                    done_reason = str(data.get("reason", "")) or None
                    # The stored run this became. Read explicitly rather than through
                    # `_USAGE_KEYS`: these are not cost, they are how to ask for the
                    # report again — and how to hand it to a LATER question as context
                    # instead of re-investigating from cold, which is the whole
                    # token-economy argument this server exists for. Null when the
                    # server's best-effort journal write failed, and absent on servers
                    # older than the field; both mean "cannot be referenced".
                    run_id = data.get("run_id") or None
                    run_seq = data.get("seq")
                elif event == "citations":
                    # The server's own verdict on the report's `path:start-end`
                    # references, checked against what its tools actually
                    # returned. Passed through so the caller can discount the
                    # unsupported ones instead of re-reading everything.
                    # `v is not None` drops the `draft_*` keys on the common path,
                    # where the report needed no repair: their presence is the
                    # signal, so carrying three nulls on every clean run would
                    # make it one.
                    citations = {
                        k: v
                        for k, v in data.items()
                        if k in _CITATION_KEYS and v is not None
                    }
                elif event == "excerpts":
                    # The indexed code at the verified citations, read from the
                    # index by the server. Collected always, RETURNED only on
                    # request: two dozen chunks is ~100 KB into the caller's
                    # context, which is the exact cost this server exists to
                    # prevent. What always goes back is the count, so the caller
                    # knows the text is one cheap re-ask away.
                    excerpts = [
                        {k: v for k, v in item.items() if k in _EXCERPT_KEYS}
                        for item in data.get("excerpts", [])
                        if isinstance(item, dict)
                    ]
                    excerpts_truncated = bool(data.get("truncated", False))
                elif event == "error":
                    failure = f"{data.get('code', 'error')}: {data.get('detail', '')}"
                # `thinking` deltas are deliberately dropped: they are the local
                # model's reasoning, worth thousands of tokens and of no value
                # to the caller.
    except httpx.TimeoutException as e:
        # The client gave up, not the server. If the report already streamed, it is
        # a real report and throwing it away is the worst of the outcomes — so keep
        # it and say plainly that this end stopped listening. Only a timeout with
        # nothing in hand is a failure.
        if not report_parts:
            raise RuntimeError(
                f"mindex research {url} timed out after {TOTAL_TIMEOUT:.0f}s with "
                f"nothing streamed ({e}) — raise RESEARCH_TOTAL_TIMEOUT above the "
                f"server's effort.high.max_seconds + report_timeout_ms, and check "
                f"the local Ollama is up."
            ) from e
        truncated_by_client = True
        done_reason = done_reason or "client_timeout"
    except httpx.RequestError as e:
        raise RuntimeError(
            f"mindex research {url} failed ({e}) — is mindex reachable, and is its "
            f"local Ollama up?"
        ) from e
    else:
        truncated_by_client = False

    if failure is not None:
        hint = _ERROR_HINTS.get(failure.split(":", 1)[0].strip(), "")
        raise RuntimeError(
            f"research failed mid-stream — {failure}" + (f" {hint}" if hint else "")
        )

    report = "".join(report_parts).strip()
    if not report:
        raise RuntimeError(
            "research produced no report (the stream ended early) — retry, and if it "
            "repeats, check the mindex logs and the local model."
        )
    out: dict[str, Any] = {
        "report": report,
        "steps": steps,
        "elapsed_ms": elapsed_ms,
        "done_reason": done_reason,
        "usage": usage,
    }
    if run_id is not None:
        out["run_id"] = run_id
        if run_seq is not None:
            out["seq"] = run_seq
    # The count always; the bytes only when asked for. This asymmetry is the whole
    # design: the caller learns the literal text exists and is one cheap re-ask away,
    # without ~100 KB of it arriving unbidden in a layer whose entire purpose is to
    # keep that out.
    if excerpts:
        out["excerpts_available"] = len(excerpts)
        if include_excerpts:
            out["excerpts"] = excerpts
            if excerpts_truncated:
                out["excerpts_truncated"] = True
        else:
            out["excerpts_hint"] = (
                f"The server holds the verbatim indexed code at "
                f"{len(excerpts)} verified citation(s). Ask for it with "
                f"include_excerpts=True rather than reading the files or "
                f"calling mindex.search."
            )
    # Echoed back so a report read later knows what it was allowed to see. A scoped
    # report and an unscoped one are otherwise the same document, and the scoped one
    # can only speak about its scope.
    if include or exclude:
        out["scope"] = {
            k: v for k, v in (("include", include), ("exclude", exclude)) if v
        }
    if truncated_by_client:
        out["truncated_by_client"] = True
        out["incomplete"] = (
            f"This end stopped listening after {TOTAL_TIMEOUT:.0f}s, before the "
            f"server said it was done — the report may be cut off mid-sentence and "
            f"its citation check never arrived."
        )
    if citations:
        out["citations"] = citations
        if citations.get("unverified"):
            # Stated in the result, not left for the caller to infer from a count:
            # the instructions say to trust the report, so the exception has to be
            # as loud as the rule.
            out["citations_warning"] = (
                f"{citations['unverified']} citation(s) name paths no tool returned "
                f"during this run — the local model invented them. Discount the "
                f"claims resting on "
                f"{', '.join(citations.get('unverified_paths', [])) or 'those paths'}; "
                f"the rest of the report is backed by locations it actually saw."
            )
        if citations.get("stale"):
            # Same reasoning as the warning above: the instructions say to trust the
            # report, so anything that qualifies that has to be stated, not implied.
            out["freshness_warning"] = (
                f"{citations['stale']} citation(s) point into files the index changed "
                f"while this run was reading them: "
                f"{', '.join(citations.get('stale_paths', [])) or 'those paths'}. The "
                f"claims are likely still true, but the line ranges may have moved — "
                f"re-read those ranges before editing them."
            )
    if done_reason is not None and done_reason != "finalized":
        out["incomplete"] = _INCOMPLETE_HINTS.get(
            done_reason,
            "The local model stopped before it was satisfied with the evidence.",
        )
    return out


def main() -> None:
    mcp.run()  # stdio transport (default)


if __name__ == "__main__":
    main()
