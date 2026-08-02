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
# `report_timeout_ms` — 3600 + 300 s at the shipped defaults. This is a CLIENT
# ceiling and it has no idea what the server is configured for, so it is set above
# the default ladder with room to spare: too low and every high-effort run dies in
# flight, which is exactly what happened when the ladder was raised and this was
# left at 1800. It moved again when the report window went 120 s -> 300 s to cover
# a report written one section at a time; this comment is the thing that has to be
# re-read whenever either server number changes.
TOTAL_TIMEOUT = float(os.environ.get("RESEARCH_TOTAL_TIMEOUT", "4200"))

# Effort used when the caller doesn't say. It selects a server-configured budget
# (wall-clock, local tokens and tool calls), so it is also the main latency lever.
# The numbers live in the server's [research.effort.*] and are deliberately not
# repeated here: three separate copies of them had drifted before this comment.
DEFAULT_EFFORT = os.environ.get("RESEARCH_DEFAULT_EFFORT", "medium")

# Excerpt bytes below which the verbatim code comes back without being asked for.
#
# `include_excerpts` defaults to False because two dozen chunks is ~100 KB into the
# caller's context, which is the cost this layer exists to prevent. But the same
# default made every *small* excerpt set cost a second full round trip — and worse,
# it made the caller decide from a hint whether the code was worth asking for, which
# is precisely the judgement it cannot make without seeing it. Field experience says
# the corrections that matter (a `list(set(...))`, a default argument) are visible
# only in the literal text and never in the summary.
#
# So: cheap sets come back, expensive ones stay behind the flag. 32 KiB is roughly a
# handful of chunks — noticeable but not a context event.
AUTO_EXCERPT_BYTES = int(os.environ.get("RESEARCH_EXCERPT_AUTO_BYTES", str(32 * 1024)))

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
    # Where the call actually landed, as `path:start-end`. `hits: 3` says three rows
    # came back and nothing about where they were, which made the trace unusable for
    # the one thing it is for — judging what the run actually looked at.
    "spans",
    "spans_truncated",
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
    # The four percentages `binding` is the maximum of. Without them `binding` is
    # routinely read as "this run is running out of X" when it means "X is the
    # largest of four shares, and it is 12%".
    "shares",
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
# `shown_paths` is the denominator the counts never had: how many files the run's
# tools actually returned. It is what makes admissibility machine-checkable —
# `verified: 0` over `shown_paths: 12` is a report that cited none of the dozen files
# it read, while over `shown_paths: 0` it is the honest "nothing in this scope was
# shown to me". The server exempts the second from its own grounding gate, so that
# one arrives looking exactly like a clean run.
_CITATION_KEYS = (
    "server_written",
    "shown_paths",
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

# Fields kept from the `verdict` event, which only a challenge stream carries:
# the opponent's conclusion about the subject report. `overall` is null when the
# verdict turn parsed to nothing — "challenged, inconclusive", which must never
# be rendered as an acquittal. `grounded: false` means the challenge's own report
# verified no citations, which the server already used to cap `overall` at
# "disputed" — an unshown accusation can dispute a report but never refute it.
_VERDICT_KEYS = ("challenged_run_id", "overall", "grounded", "claims")

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

The result's `usage` says what the run spent against what it was granted. `binding`
(also flat, beside `done_reason`) names the axis with the LARGEST SHARE SPENT — a
maximum, not a warning. Read it together with `usage.shares`, the four percentages
it was chosen from: `binding: "time"` at `shares.time: 12` means the run used an
eighth of its clock and less of everything else, which is a comfortable run, not one
about to expire. It is worth reading before a *broader* follow-up: a "finalized"
report whose winning share is near 100 means the next question needs a higher
effort to finish at all. What actually stopped a run is `done_reason`, never this.

`citations_verified` and `citations_total` are flat too. They are the grounds for
the instruction above — the count of the report's claims the server checked against
locations the run really saw. High `verified` with zero `unverified` is why you do
not re-read anything.

IF THE REPORT IS NOT ENOUGH — it left a gap, it contradicts itself, it says the
evidence was insufficient, `done_reason` was not "finalized", or your next step
needs something it didn't cover — ASK SCOUT AGAIN. Send a sharper, narrower follow-up question naming exactly what
is missing. Follow-ups are cheap and they are the intended path. What you must
never do instead is fall back to investigating it yourself.

WHEN YOU NEED THE LITERAL TEXT, the server already has it — do not ask the report
for it. The indexed code at every verified citation is on the server's side of the
wire. A small set comes back on its own, in `excerpts` (path, line range, verbatim
code), with `excerpts_note` saying it was sent unasked; READ IT rather than
trusting the report's paraphrase, because the corrections that matter — a
`list(set(...))`, a default argument value — are visible only in the literal text.
A large set is withheld and `excerpts_hint` tells you its size; ask for it by
calling `research` again with `include_excerpts=True`. Either way it costs one SQL
read and no model tokens. This is the right answer to "reproduce this config file",
"show me the exact rule text", "what does that function actually say". Large sets
are not sent by default because two dozen chunks is ~100 KB of your context and
this server exists to keep that out.

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

THE ADMISSION CHECK, AND IT IS ARITHMETIC, NOT JUDGEMENT: before you act on a
report, require `steps > 0` and `citations.verified > 0`. A report that passes
neither was written without the index being consulted, however confident it reads.
The one legitimate exception is `citations.shown_paths == 0` — no tool returned a
single file, so there was nothing to cite and the report can only be saying the
question is unanswerable in this scope; treat that as a scoping problem and re-ask
with a wider one, never as a finding about the code. `verified: 0` with
`shown_paths` above zero is the case to refuse outright: the run read files and
grounded nothing in them.

How to call it: pass the project GUID from the repo-root `.mindex` file and ONE
clear question in `question` (a full question, not keywords — the local model
decomposes it into its own searches). Pick `effort` by how much the answer is
worth: "low" for a narrow lookup, "medium" for normal understanding, "high" for
a genuinely broad or cross-cutting investigation. Higher effort costs you
nothing in tokens — only wall-clock time, and that cost is real: on a 30B-class
local model a "high" run is typically several minutes to a quarter of an hour.
Spend it on questions that earn it. A mechanical extraction — "list the keys in
that dict literal" — is a "low" question, and asking it at "high" buys nothing but
the wait. The server publishes what each level has actually cost lately in
`GET /config` under `research.observed`, if you need the real numbers rather than
this rule of thumb.

ONE RUN AT A TIME is the normal configuration (`research.max_concurrent` in
`GET /config`, usually 1 on a single-GPU host). A second call while one is running
is refused outright, so plan investigations as a queue rather than firing several
and hoping. If a call of yours is interrupted or times out, the run keeps going on
the server and keeps its slot: the result carries `live_run_id` and `still_running`
for exactly that case, and `DELETE /research/active/{run_id}` frees it.

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

REPORTS CARRY A TRUST STATUS. Every stored run's listing shows `trust`:
`unchallenged` (nobody has attacked it), `confirmed`, `disputed` or `refuted` —
the aggregated verdict of *valid* challenge runs aimed at it. Weigh a `refuted`
report as likely wrong and a `disputed` one with care; `unchallenged` merely
means untested. When a report's correctness is load-bearing — you are about to
build on it, hand it to a human, or chain serious work onto it — order a
refutation pass with the `challenge` tool: a local model re-derives the report's
claims against the index and files a verdict, for wall-clock only. An
inconclusive challenge (null `overall`) is NOT an acquittal, and an ungrounded
one is capped at `disputed` — the server enforces both, so read `verdict` as
given.

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
            of these numbers have drifted from the server before (``GET /config``
            publishes the live ladder and ceilings).

            Four report-shape keys ride in the same dict:
            ``max_report_sections`` (min 3 — how many sub-questions the plan asks
            for and the report may write; sections share one fixed report window,
            so past its capacity extras ship as stubs), ``max_report_words``
            (0 = announce no length, else min 150), ``checkpoint_every_steps``
            (0 = no draft-banking turns this run, else min 2 — each costs a step),
            and ``evidence_width`` (min 1 — multiplies how many rows read_chunks/
            grep/callers/file_history/symbols return; width is resent on every
            later turn, so it is paid for in the token budget, not once). An older
            server rejects the new keys with a 400 rather than ignoring them.
        context_run_ids: Optional ``run_id`` values of earlier runs on this project
            whose reports should be handed to this one as background — normally the
            ``run_id`` a previous call returned. They save the new run the work of
            rediscovering names, and they are NOT evidence: the local model is told it
            may not cite them, and anything it copies from one is reported back as
            ``unverified``. The server caps how many may be given.
        include_excerpts: Force the verbatim indexed code at every verified citation
            into the result. Not needed for a SMALL excerpt set — that comes back on
            its own — so set it when the result said the code was withheld
            (``excerpts_hint``) and you actually need the literal text: about to edit
            that code, or reproducing a config/schema file. A large set is not sent
            by default because two dozen chunks is ~100 KB landing in YOUR context,
            which is the cost this server exists to prevent.

    Returns ``{"report", "steps": [{n, action, query|name|path|glob, hits, spans}],
    "elapsed_ms", "done_reason", "binding", "usage"}``, plus
    ``citations``/``citations_verified``/``citations_total`` and ``excerpts`` when
    the run produced them. ``steps`` is the trace of what the local model looked at,
    and ``spans`` on each step are the ``path:start-end`` locations that call
    actually returned — read them if you need to judge how well the question was
    covered.

    ``done_reason`` is "finalized" when the local model judged the evidence
    sufficient. Any other value means it was stopped
    ("time_exhausted" / "tokens_exhausted" / "budget_exhausted" /
    "context_exhausted" / "unparseable" / "repeated_calls"), an ``"incomplete"``
    key explains which, and the report rests on partial evidence — the one case
    where a follow-up question (or the same question at a higher effort) is worth
    the wall-clock rather than a waste of it.

    ``usage`` is what the run spent against what it was granted (time, local
    tokens, steps, turns) plus ``shares``, the four percentages spent. ``binding``
    names the largest of those shares — a maximum, not a warning: read it with
    ``usage.shares`` beside it, because "time" at 12% is a comfortable run. Only on
    a share near 100 does the next, broader question need a higher effort to finish
    at all.

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
    return await _run(url, body, include_excerpts)


async def _run(url: str, body: dict[str, Any], include_excerpts: bool) -> dict:
    """Drive one research/challenge SSE stream and shape the result.

    One consumer for both tools, so the reader whitelists, the timeout story and
    the warning wording cannot drift between them."""
    timeout = httpx.Timeout(TOTAL_TIMEOUT, connect=CONNECT_TIMEOUT, read=READ_TIMEOUT)

    report_parts: list[str] = []
    steps: list[dict[str, Any]] = []
    usage: dict[str, Any] = {}
    citations: dict[str, Any] = {}
    excerpts: list[dict[str, Any]] = []
    verdict: dict[str, Any] = {}
    excerpts_truncated = False
    excerpts_total = 0
    elapsed_ms = 0
    done_reason: str | None = None
    run_id: str | None = None
    live_run_id: str | None = None
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
                if resp.status_code == 429:
                    # Told apart from every other refusal because the caller's next
                    # move is different: this is not a bad request, it is a queue.
                    # The server publishes how many slots exist
                    # (GET /config → research.max_concurrent) and what is holding
                    # them (GET /research/active), so say where to look instead of
                    # leaving "retry later" as the whole advice.
                    raise RuntimeError(
                        f"mindex is already running its maximum number of research "
                        f"runs — {_problem(resp.text, resp.status_code)} Wait and "
                        f"re-ask; do NOT retry in a loop. One run at a time is the "
                        f"normal setting on a single-GPU host, so treat research as "
                        f"a queue of one: finish this question before starting the "
                        f"next."
                    )
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
                if event == "started":
                    # The run's name, before any work — kept apart from the stored
                    # id `done` reports. The interesting case is the one where `done`
                    # never arrives: a run this end stops listening to is still
                    # running on the server, holding a slot, and this is the id that
                    # lets the caller cancel it (DELETE /research/active/{run_id})
                    # instead of waiting it out.
                    live_run_id = data.get("run_id") or None
                elif event == "summary":
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
                    # Deliberately NOT falling back to the id `started` gave: a null
                    # here means the journal write failed, so the run exists but
                    # nothing can fetch it. Substituting the live id would hand the
                    # caller a name for a report it cannot read.
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
                    # The server's own count of verified citations, BEFORE its caps.
                    # `len(excerpts)` is what survived them, so reporting only that
                    # would quietly under-report how much evidence the report rests
                    # on.
                    excerpts_total = int(data.get("total", len(excerpts)))
                elif event == "verdict":
                    # Challenge streams only: the opponent's conclusion.
                    verdict = {k: v for k, v in data.items() if k in _VERDICT_KEYS}
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
    # Flat, beside `done_reason`, because the instructions name it as something to
    # read: buried in `usage` it was a field the caller was told to consult and had
    # to go looking for. It names the axis with the largest share spent — a maximum,
    # not a warning — which is why `usage.shares` beside it is what makes it legible.
    if "binding" in usage:
        out["binding"] = usage["binding"]
    if run_id is not None:
        out["run_id"] = run_id
        if run_seq is not None:
            out["seq"] = run_seq
    # The count always; the bytes when asked for, or when they are cheap enough that
    # asking would cost more than sending. The asymmetry is the design: the caller
    # must never receive ~100 KB unbidden in a layer whose whole purpose is to keep
    # that out — but a few KB withheld behind a hint buys a second round trip and a
    # judgement the caller cannot make without seeing the code.
    if excerpts:
        out["excerpts_available"] = len(excerpts)
        if excerpts_total > len(excerpts):
            # The server capped before we did; say so, or the count reads as the
            # whole evidence base.
            out["excerpts_verified_total"] = excerpts_total
        excerpt_bytes = sum(len(e.get("code", "")) for e in excerpts)
        if include_excerpts or excerpt_bytes <= AUTO_EXCERPT_BYTES:
            out["excerpts"] = excerpts
            if excerpts_truncated:
                out["excerpts_truncated"] = True
            if not include_excerpts:
                out["excerpts_note"] = (
                    f"Included unasked: {excerpt_bytes} bytes is under the "
                    f"{AUTO_EXCERPT_BYTES}-byte threshold, so this is cheaper than "
                    f"the re-ask it saves. This is the indexed code itself — read it "
                    f"rather than trusting the report's paraphrase of it."
                )
        else:
            out["excerpts_hint"] = (
                f"The server holds the verbatim indexed code at "
                f"{len(excerpts)} verified citation(s), about {excerpt_bytes} bytes — "
                f"over the {AUTO_EXCERPT_BYTES}-byte threshold, so it was not sent "
                f"unasked. Ask for it with include_excerpts=True rather than reading "
                f"the files or calling mindex.search."
            )
    # Echoed back so a report read later knows what it was allowed to see. A scoped
    # report and an unscoped one are otherwise the same document, and the scoped one
    # can only speak about its scope. (A challenge sends no scope of its own — it
    # inherits the subject's on the server — so nothing is echoed for it.)
    if body.get("include") or body.get("exclude"):
        out["scope"] = {
            k: v
            for k, v in (
                ("include", body.get("include")),
                ("exclude", body.get("exclude")),
            )
            if v
        }
    if truncated_by_client:
        out["truncated_by_client"] = True
        out["incomplete"] = (
            f"This end stopped listening after {TOTAL_TIMEOUT:.0f}s, before the "
            f"server said it was done — the report may be cut off mid-sentence and "
            f"its citation check never arrived."
        )
        if live_run_id is not None:
            # The run did not stop when we stopped reading: it is still on the
            # server, still holding one of `max_concurrent` slots, and it will keep
            # it until its own deadlines expire. Handing back the id is what makes
            # that recoverable instead of a wait.
            out["live_run_id"] = live_run_id
            out["still_running"] = (
                f"The server was not told to stop and is probably still running this "
                f"job, holding a research slot. Cancel it with "
                f"DELETE /research/active/{live_run_id}, or check "
                f"GET /research/active, before starting another research run."
            )
    if verdict:
        out["verdict"] = verdict
        overall = verdict.get("overall")
        if overall is None:
            out["verdict_warning"] = (
                "The challenge ran but its verdict turn produced nothing parseable "
                "— the subject is CHALLENGED, INCONCLUSIVE. Do not read this as the "
                "report being confirmed; if the verdict matters, run the challenge "
                "again."
            )
        elif not verdict.get("grounded", True):
            out["verdict_warning"] = (
                "The challenge's own report verified no citations, so its verdict "
                f"was capped at '{overall}': an accusation that showed no code can "
                "dispute a report but never refute it."
            )
    if citations:
        out["citations"] = citations
        # Promoted out of the nested object. The instructions tell the caller to
        # trust the report *because* the server checked its provenance — and the
        # number backing that instruction sat one level down while every exception
        # to it (`citations_warning`, `freshness_warning`) was already flat. The
        # grounds for trusting were harder to find than the grounds for doubting.
        if "verified" in citations:
            out["citations_verified"] = citations["verified"]
        if "total" in citations:
            out["citations_total"] = citations["total"]
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
    # Only when the *server* named a stop reason. `client_timeout` is this end's own
    # verdict and already carries a more specific note above; overwriting it with the
    # generic fallback traded a precise message ("we stopped listening, the run may
    # still be going") for a vague one about the model.
    if (
        done_reason is not None
        and done_reason != "finalized"
        and not truncated_by_client
    ):
        out["incomplete"] = _INCOMPLETE_HINTS.get(
            done_reason,
            "The local model stopped before it was satisfied with the evidence.",
        )
    return out


@mcp.tool()
async def challenge(
    project_guid: str,
    run_id: str,
    effort: str = DEFAULT_EFFORT,
    model: str | None = None,
    budget: dict[str, Any] | None = None,
    include_excerpts: bool = False,
) -> dict:
    """Set an opponent on a stored research report — refutation as a service.

    A local model receives the stored report as the SUBJECT UNDER EXAMINATION,
    extracts its principal claims, and spends a whole research budget trying to
    refute each one against the live index. Nothing from the subject counts as
    evidence: every location must be re-derived through the opponent's own
    tools, and that re-derivation IS the check. The result is a challenge report
    (same shape as `research`) plus a `verdict`:

        {"challenged_run_id", "overall", "grounded", "claims": [{claim, verdict}]}

    `overall` is "confirmed" / "disputed" / "refuted" — or null, meaning the
    verdict turn parsed to nothing: CHALLENGED, INCONCLUSIVE, never an
    acquittal. `grounded: false` means the challenge itself verified no
    citations, and the server capped its verdict at "disputed": an accusation
    that showed no code can dispute but never refute.

    The verdict also lands on the SUBJECT as a derived trust status
    (`unchallenged`/`confirmed`/`disputed`/`refuted`) that every research
    listing shows from now on — this is the durable half. A challenge whose own
    evidence goes stale stops counting automatically.

    WHEN TO USE IT: before building further work on a report whose correctness
    matters — a report you are about to hand to a human, cite in a decision, or
    chain many follow-ups onto. It costs a full research run of wall-clock, so
    challenge reports that have earned the scrutiny, not every answer.

    Refused (400) when the subject is no longer valid — its files moved, so
    "the code changed" cannot be spent as "the report was wrong"; re-run the
    research first. Also refused when the subject is itself a challenge:
    contest a bad challenge by challenging the original again, or delete it.

    Args:
        project_guid: The project's mindex GUID (from the repo-root .mindex file).
        run_id: The stored run to challenge — `run_id` from a `research` result
            or from GET /projects/{guid}/research.
        effort: Same ladder as `research`; "medium" is right for most reports.
        model: Optional Ollama model override. Challenging with a DIFFERENT
            model than wrote the subject is the interesting experiment.
        budget: Same per-axis override dict as `research`.
        include_excerpts: As on `research` — the challenge's own verified
            citations come with verbatim code the same way.
    """
    run_id = run_id.strip()
    if not run_id:
        raise ValueError("run_id must not be empty")
    if effort not in _EFFORTS:
        raise ValueError(f"effort must be one of {sorted(_EFFORTS)}, got {effort!r}")

    body: dict[str, Any] = {"effort": effort}
    if model:
        body["model"] = model
    if budget:
        body["budget"] = budget

    url = f"{SERVER}/{PROTOCOL}/{project_guid}/research/{run_id}/challenge"
    return await _run(url, body, include_excerpts)


def main() -> None:
    mcp.run()  # stdio transport (default)


if __name__ == "__main__":
    main()
