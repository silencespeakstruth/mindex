# /research — full design record

Companion to `.claude/CLAUDE.md`, which keeps only the condensed invariant
list. This file holds the full rationale, the rejected alternatives and the
measurement history. Read it before modifying `research.rs`,
`models/ollama.rs`, the budget machinery or the SSE contract.


`POST /v0/{guid}/research` — long-lived one-way SSE: a local Ollama model
(`[research]` config, TOML-only) loops search/symbols **via internal cores**
(`search_core`/`symbols_core` in `handlers.rs`; never HTTP-to-self), then
streams a Markdown report. Non-obvious invariants:

- **Cancellation = cancelling the job token.** `SseEventStream`'s `Drop` does it
  on disconnect (axum drops the SSE body); a closed mpsc channel is the same
  signal from the other side. Since 2026-08-01 `DELETE
  /research/active/{run_id}` is a second hand on the same lever, for the case
  disconnect cannot reach — see *A run became something you can see and stop*
  below. The semaphore permit rides **in the spawned job**, not the stream — the
  job is detached, so releasing on stream-drop would over-admit past
  `max_concurrent` while the old job still spends GPU/DB time.
- **Dedicated runtime.** Jobs run on a small separate multi-thread runtime
  (`[research].worker_threads`, leaked in `main.rs` — dropping a runtime from
  async context panics). Admission via `Arc<Semaphore>`
  (`[research].max_concurrent`) → 429 `research.busy` up front.
- **Two seams** keep the loop testable without Ollama/Qdrant/embedder:
  `OllamaModel` (`models/ollama.rs`, streamed NDJSON chat, thinking+content
  deltas) and `ResearchTools` (`research.rs`; prod impl = the cores). Mocks:
  `tests/mock_ollama` (scripted via `POST /script`, one action per tool turn;
  finds its place by counting `role: "tool"` messages; emits **native**
  `tool_calls`, recognises the report turn by the *absence* of `tools`;
  `force_text_calls` covers the `research.model_lacks_tools` path), fakes in
  `research.rs` tests.
- **Model protocol = Ollama's native tool calling.** The twelve tools
  (search/grep/symbols/outline/callers/list_files/read_chunks/file_history/
  list_research/read_research/note/revise_plan, plus `finalize`) go out as
  `tools` JSON Schemas (`tool_specs`) and come back in `message.tool_calls` — a
  field distinct from `content`/`thinking`, so a model cannot put its decision
  in the wrong channel. A call becomes an `Action` by injecting the function
  name into its arguments object (`Action` is `#[serde(tag = "action")]`) — one
  deserializer, one set of error messages.
- **No text fallback, deliberately.** A model whose template lacks tool support
  writes the call as prose; the loop detects that
  (`looks_like_tool_call_attempt`) and fails with `research.model_lacks_tools`,
  naming the model — parsing prose was measured a bad trade (a second protocol
  for the worst model in the bake-off, with unvalidated guessed arguments). A
  `tools` capability in `ollama show` is not proof; the template is what
  matters.
- **A turn may ask for several tools**, all execute, each as its own `step`.
  Hard invariant: **every call gets exactly one `role: "tool"` reply**, in
  order — including calls rejected as duplicates or skipped for budget. The
  `NativeOllama` test fake asserts the pairing every turn.
- **Prose with no tool call means "done"** (`Finalized`). `Unparseable` is only
  an empty reply or a call to a nonexistent tool. Duplicates are rejected, not
  re-executed; for `search` "duplicate" is **near**-duplicate: normalized
  queries compared by token-set Jaccard (`NEAR_DUPLICATE_JACCARD` 0.5, ≥
  `NEAR_DUPLICATE_MIN_TOKENS` tokens) — deliberately also rejecting a mild
  *refinement* (the measured trapped loop), with the rejection naming the
  earlier query. Only *executed* searches enter `seen_queries`, so a rejection
  cannot poison the set.
- **`read_chunks(path, start_line, end_line)` reads the index, never the
  file.** Pure SQL over `project_file_chunks` (`status='active'`, span overlap,
  `READ_CHUNKS_LIMIT` 8). Exists because the model was observed *searching*
  for a line range `symbols` had just handed it. Like `outline` it must report
  a gap honestly ("the file IS indexed; lines 53-60 have no chunk") — a silent
  empty answer reads as "the file is empty there".
- **`path_prefix` on `search` is a post-filter, not an extra `include`.**
  Over-fetch `top_k * PREFIX_OVERFETCH` with *unchanged* `include`/`exclude`,
  drop non-matching, truncate. Appending to `include` would widen (`include`
  is a union) — a scoped run could search its way out of its own scope.
- **Sampling is configurable; `PROMPT_VERSION` is stamped on every run.**
  `[research].temperature/top_p/seed` are `Option` (absent = the model's own
  Modelfile default — right for production, wrong for comparing models); a
  request's `seed` overrides config. `PROMPT_VERSION` (`research.rs`) rides on
  `done` and into the journal; **bump it on any edit to `system_prompt`,
  `PLAN_REQUEST`, `SUFFICIENCY_REQUEST`, `REVALIDATION_SYSTEM_PROMPT`,
  `format_citation_complaint`, `REPORT_ROLE`/`report_system_prompt`, either
  report turn's user message, the budget nudges or `tool_specs`** — nothing
  else on the stream says which instructions were in force. The run-state note
  is in scope for its *labels* only.
- **Citations are provenance-checked server-side** (`parse_citations` →
  `Evidence` → `CitationReport`, the `citations` event between `summary` and
  `done`). Every `path:start-end` is bucketed against what the run's own tools
  returned: `verified` (path shown **and** range overlaps a shown span),
  `path_only`, `unverified` (a path no tool returned). Range *existence* is
  deliberately unchecked (the schema holds no line counts). The parser requires
  a file extension **and** a relative path (no leading `/`, no `//`) so
  `http://host:8080-8090` is not a citation. Ships **with** its consumers by
  rule: scout's reader silently drops unknown events, and its `_INSTRUCTIONS`
  point at `citations.unverified_paths` as the one thing worth checking.
- **A failed citation check sends the report back.** The report turn writes a
  **draft** with the content gate closed (`stream_content: false`) — nothing
  reaches the client before `check_citations` runs. Clean draft → shipped as-is
  in one `summary` event. Otherwise the offending *locations* (not counts) are
  named back via `format_citation_complaint`; `REVALIDATION_SYSTEM_PROMPT`
  swaps in and the tools re-open for `MAX_REVALIDATION_STEPS` (4) executed
  calls over `MAX_REVALIDATION_TURNS` (3) turns **only when `reason ==
  Finalized`** (a budget-stopped run has nothing left to spend — its complaint
  says correct or drop the claim); then a rewrite turn streams the real
  `summary`. Revalidation steps emit `Step` events numbered on from `steps` but
  do **not** increment it (the budget-facing count stays inside what was
  granted). A rewrite that fails ships the draft (a mis-cited report beats
  none; its `citations` say what to distrust). The draft's counts ride on
  `citations` as `draft_unverified`/`draft_path_only`/`draft_stale`/
  `revalidation_steps`, null when no repair happened — otherwise "does this
  pass pay for itself?" is unanswerable from the corpus.
- **A report that cites *nothing* is the third defect in that gate** —
  `citations: {total: 0}` is byte-for-byte what a clean report emits, so
  ungrounded reports shipped looking perfect (measured 2026-07-30: 5 of 24
  runs; only 1 of 5 was catchable by a wider parser, so the fix is the missing
  route into the gate, not the parser). Two load-bearing exemptions: a run **no
  tool showed a single file** (`evidence.paths()` empty) cannot cite anything —
  its "not reachable from this scope" report is the *correct* outcome; and a
  report under `MIN_GROUNDED_REPORT_CHARS` (800) is the short honest version of
  the same answer. `format_citation_complaint` dispatches to
  `format_ungrounded_complaint` (no failing location to name; the model needs
  the *form* `path:START-END` and the citable file list). No wire field added:
  an ungrounded draft is `revalidation` present with all three draft counts
  **zero**.
- **Indexing is never blocked by research; the run reports what moved
  instead.** Nothing serializes the two — research takes no `IndexClaim`,
  `post_index` never looks at `research_semaphore` (the writer is an external
  process; mutual exclusion could only be a 409/429 refusing the change the
  user just made). Safe because per-file **consistency holds** (the prepare tx
  replaces a file's chunks and symbols atomically — a reader sees an older
  whole file, never half) and **currency is reported**: `Evidence` keeps a
  `baseline_sha` per shown path; `probe_freshness` (via
  `ResearchTools::file_versions` → `file_versions_core`, one chunked indexed
  SELECT — no HTTP, no step, no budget) re-reads them before every turn and
  once more before the report; the state note names what CHANGED, what LEFT the
  index, and what is reindexing *right now*. `changed`/`removed` are sticky
  (the transcript is the run's only memory); `in_flight` is not — it covers the
  window between `post_index` phase 1 and 2 where a chunk sits in the `has_id`
  set with no vector yet (search under-retrieves it silently; the pure-SQL
  tools still work). Staleness is **orthogonal to provenance**
  (`citations.stale`/`stale_paths`) and joins the revalidation gate.
  `apply_versions` takes the *asked* path list, not just results (inferring
  removal from an unasked path would invent staleness); a failed probe leaves
  previous verdicts standing. A snapshot (`as_of`) read was rejected: a
  hot-path chunk-deletion side table + `as_of` through five cores + a GC lease,
  still *partial*, buying internal consistency at the cost of currency — the
  wrong trade for code research.
- **Every finished run is journalled** as one `research_runs` row: question,
  report, granted budget vs cost, citation verdict, index movement, tool usage
  (notes written/rejected, plan revisions, grep calls/hits, out-of-scope
  refusals and hidden rows, scope, server-written flag, report window granted
  vs taken). One row, one INSERT — a run *is* one flat measurement record.
  Per-tool call counts are *not* journalled (`research_tool_calls{tool}` has
  them). All through the `ResearchJournal` seam (`db/research.rs`; prod
  `SqliteResearchJournal`). Best-effort: insert failure = `warn!`, never a
  failed run. **No FK to `project_files`** — a run must never surface in
  `/drift`. Unset sampling stores NULL, not 0. `NoJournal` is
  `#[cfg(test)]`-gated: production is never offered a trace-less journal.
- **A stored run is reusable as context; staleness is per-path, not a global
  counter.** `context_run_ids` names earlier runs of the *same* project; their
  reports are injected as one `user` message before the plan turn
  (`format_prior_reports`). A global `project_version` was rejected: with
  `mindex-watch` running, one save would stale every run at once — correct and
  useless. Instead each run's baselines (`Evidence.baseline_sha` per shown
  path) persist into `research_run_files`; staleness is the same
  `changed || removed` comparison, asked later against `project_files`. Three
  easy breaks: the join needs **`model_id`** (bind from `RouterState`;
  `project_files` is keyed `(project_guid, model_id, path)` — a path-only join
  matches across embedding models); `research_run_files.path` carries **no FK**
  (`RESTRICT` would make `prune_deleted_files` refuse to drop any file a past
  run read; `CASCADE` would erase the baseline and make a run whose file is
  gone read as fresh); and the freshness/validity filters on the list must
  apply **inside** the cursor-bounded subquery, before `LIMIT`, or a short page
  stops meaning "no more".
- **Validity is the transitive verdict, derived — never stored.**
  `context_run_ids_json` is the edge set of a knowledge graph (A → B = "B was
  in A's context at launch"); `research_validity_ctes` (`handlers.rs`) computes
  `valid = own files unmoved AND every context parent exists AND is itself
  valid` as one recursive CTE over `json_each` at read time. A stored flag was
  rejected: staleness can *heal* (a file reindexed back to the same bytes) and
  its onset is an ordinary indexing write with no research-side event to hook.
  Deletion needs no cascade: a hard `DELETE` (or the GC sweep) leaves a
  dangling id, and the CTE reads a dangling reference as invalid, transitively,
  with no write. Cycles impossible by construction (context ids validated at
  launch; the run's own row doesn't exist yet — edges point strictly
  backwards); the recursive `UNION` deduplicates. `freshness` keeps its
  self-staleness meaning; `valid` is the orthogonal filter; each summary
  carries `valid`/`invalid_reason` (`stale`/`context_deleted`/
  `context_invalid`) plus `context` — the flat transitive ancestry with each
  ancestor's state. A request naming an invalid run is refused up front (400
  `validation.research_context_invalid`, offenders in `meta.runs`) — the client
  showed `valid` before the pick, so the refusal only fires when the index
  moved in between.
- **The model can browse the stored corpus** — `list_research` (seq, title,
  question of *valid* runs only, minus those already injected, capped at
  `LIST_RESEARCH_LIMIT`) and `read_research(seq)` (one valid report, truncated
  out loud at `max_context_chars`). Both deliberately **unscoped** (reports are
  not files; `ToolScope` does not apply — the tool descriptions and
  `system_prompt` say so) and both return `shown: Vec::new()` unconditionally
  (`read_research_never_seeds_the_evidence`). An invalid/missing seq is an
  explicit refusal, not an empty answer. Self-scan needs no check: the live run
  has no row yet.
- **Prior reports are hearsay; nothing in them may be cited.** They are the
  fastest way to learn real names (the measured cold-run bottleneck) — and not
  evidence. Their paths are **never** seeded into `Evidence`, so a copied
  `path:start-end` lands `unverified` and trips the gate like an invented one
  (`a_prior_report_never_seeds_the_evidence`). The `system_prompt` paragraph
  saying so is conditional, like `scope_rule`, shipping with the corpus half or
  not at all. Each injected section states its own staleness in words (a stale
  report is useful for names, misleading about specifics). Over-cap reports are
  truncated **with a marker** (`[research].max_context_chars`) — an injected
  block is prompt tokens on *every* turn, so the cap is a budget axis.
- **`title` is the report's own heading, `seq` an ordinal, `id` identity.**
  `extract_report_title` stores the first ATX heading at journalling time (NULL
  when absent or trivially repeating the question); the wire `title` falls back
  to `research_title`, still derived at read time (a stored *truncation* goes
  stale when the rule changes; a stored copy of model output cannot). The
  list's `q` searches title, question **and** report. `seq` is per-project,
  monotonic, and the keyset cursor — never `OFFSET`, over a table GC prunes. It
  is **not** identity (a total wipe restarts it at 1), so every mutating
  endpoint keys on the uuid `id`.
- **A structurally broken report is sent back; if still broken, never
  journalled.** `validate_report_markdown` is four shape checks (empty, JSON
  start, no leading `# heading`, unclosed fence) — tree-sitter-md accepts
  anything, so parsing would be a validator that cannot fail. A failing draft
  joins the citation complaint (`format_markdown_complaint`, appended when both
  fire); a markdown-only defect re-opens **no** tools (nothing to look up, only
  rewrite). If the final text still fails it is streamed (a watched broken
  report beats a vanished one) but `journal.record` is skipped and `done`
  carries null `run_id`/`seq` — the existing failed-journal wire shape.
  `forced_synthesis` is exempt by flag
  (`forced_synthesis_passes_the_markdown_gate`); the skipped run is invisible
  to `MeteredJournal`'s counters, accepted with a `warn!`.
- **The missing heading is the one defect the server repairs itself**
  (`repair_missing_heading`). Measured 2026-07-31: of three runs that reached a
  finished report, one was discarded for a missing `#` and nothing else — a
  whole local investigation lost to a line of syntax the server can write as
  well as the model can, while the analysis, structure and citations below it
  were untouched. So the gate keeps its four checks, and the repair fires only
  when the heading is the **sole** problem: a report that also opens with JSON,
  or leaves a fence open, is broken in a way no substitution fixes — prepending
  a heading there would produce something that *passes* the gate and is still
  unusable as prose, which is worse than the refusal. The heading is
  `# {research_title(question)}`, the same derivation the readers already fall
  back to, so a repaired report is titled exactly as an untitled one is
  displayed. Three consequences worth keeping: the repair runs **after**
  `check_citations` at both sites (the question can itself contain a
  `path.rs:1-2`, and a server-written line must never enter the provenance
  report as a claim the model did not make); at the draft site it also spares
  the run a rewrite turn, since a heading-only complaint was one whole report
  window spent on formatting; and at the final site the rewrite has *already
  been streamed*, so there — uniquely — the stored report carries a line the
  live view did not, which is one derived line weighed against losing the run,
  resolved towards the corpus because the corpus is what a later run reads.
  `title` is extracted **before** the repair, so a server-written heading is
  never stored as the model's own. Not recorded as a flag on the row: it is
  derivable (`title IS NULL` plus a report opening with that derivation) and a
  column costs a table rebuild. Pinned by
  `a_report_missing_only_its_heading_is_journalled_after_repair` and
  `only_the_missing_heading_is_ever_repaired`.
- **`expires_at IS NULL` means pinned — the whole retention mechanism.** The
  deadline is stamped at insert from `[research].retention_days`, so a setting
  change moves future runs only; `prune_expired_research` takes no retention
  argument (comparing against *current* config would make pinning
  inexpressible and re-date the corpus on every edit). Unpinning restores
  `created_at + retention` — a run older than the window becomes eligible at
  the next sweep; `now + retention` would turn a checkbox toggled twice into a
  silent renewal.
- **`effort` selects a budget; the request may override it**
  (`[research.effort.{low,medium,high}]` → `EffortBudget` → `research::Budget`
  via `Budget::resolve`, axis by axis; an absent axis keeps the preset). The
  run stops at whichever axis is reached first; `done.reason` says which. The
  levels are **config keys, not `match` arms** (the numbers are hardware- and
  model-dependent). Four axes with different jobs:
  - **`max_seconds` is the budget and a HARD deadline** (300/900/3600) — what
    the caller waits and what holds a `max_concurrent` slot. Polling between
    turns was not enough (a real bug: one `chat_stream` can retry internally up
    to `6 × turn_timeout_ms`, and the ~18 post-loop turns ran uncheckd —
    measured ~1.5 h overrun holding a slot). Now *also* enforced by a
    `DeadlineToken` child of the job token firing at `started + max_seconds`,
    reaching `chat_stream`'s two `select!`s (dropping the reqwest body makes
    Ollama abort generation) and every `*_core`'s child token. **Both
    mechanisms stay**: the poll is the graceful stop leaving a well-formed
    transcript; the token is the backstop for a turn that never returns. A
    deadline stop is told from a client disconnect by `stopped_by` (job token
    tested *first* — a disconnect cancels the whole tree) and is not a failure:
    the run reports what it found. Two traps: a deadline firing mid-batch must
    still answer every announced call (the pairing invariant) before breaking,
    and must not charge a step for a lookup that returned nothing.
  - **The report phase has its own window** (`[research].report_timeout_ms`,
    default 120 s) — `max_seconds + report_timeout_ms` is the true worst case.
    Its token is a child of the **job** token, never the budget one (parented
    to the deadline that just fired, it would be dead before it opened): a
    deadline-stopped run still gets to synthesise. The window bounds the
    empty-report retries, the revalidation loop and the rewrite; expiring with
    a draft ships the draft; expiring with nothing, `forced_synthesis` writes
    an honest account (question, plan, notes, shown paths) rather than closing
    a 200 stream with no `summary`. Salvaging a half-written draft was
    rejected: `chat_stream` discards accumulated content on cancel, and a
    mid-sentence report reads as authoritative.
  - **A truncated run says so in its own report** — `report_request` prepends a
    paragraph naming the limit and the open sub-questions (`done.reason` is a
    wire field, but the report is what gets quoted months later). The
    sufficiency turn is *skipped* on a truncated run (both outcomes pointless;
    it used to run unbudgeted).
  - **`turn_timeout_ms` must sit ABOVE every budget; startup enforces it**
    (`validate` refuses `turn_timeout_ms <= max_request_seconds`). Measured
    twice from the same wrong intuition: tightened, glm's cold opening turn
    (model load + ~98k-token KV alloc) or a thinking-loop turn crossed it and
    the run died at step 0 with `ollama.unavailable` — while the deadline would
    have cancelled the turn and still shipped a report. It is a dead-socket
    guard, not a bound.
  - **The silence guard bounds a turn's mute prefix**
    (`[research].first_token_timeout_ms`, default **120 s**, `0` = off). The
    hole the two guards above leave between them: `turn_timeout_ms` sees a
    socket that died, `max_turn_thinking_chars` sees a model that will not stop,
    and neither sees a connection that is **alive and produces nothing** —
    which is what an Ollama loading (or repeatedly evicting and reloading) a
    model looks like from the client. Measured 2026-08-01: a run spent 300 s of
    its 300 s budget at `steps: 0, turns: 0, prompt_tokens: 0` while Ollama
    logged five `Load failed … timed out waiting for llama-server to start`,
    thrashing between `-c 32768` (another client) and `-c 98304` (mindex); the
    caller watched an empty stream for seven minutes and mindex said nothing,
    because from its side the request had merely not answered yet. The guard is
    armed once across **`post_chat` and the wait for the first delta** — the
    stall lands in either half, since Ollama holds the connection open while it
    loads, so the response headers themselves may be what never arrives — and
    is spent by the first delta of any channel, tool calls included (a turn
    that emits one call and nothing else is answering, not silent). Two minutes
    because what it must not preempt is a legitimately slow *first* token:
    prompt evaluation of a long transcript is minutes of silence by nature, and
    this fires only when even that has not begun. It surfaces as
    `OllamaError::Silent` — its own variant, because the diagnosis is specific
    and the `warn!` can then name the model and point at `journalctl -u ollama`
    — which reaches the client as `ollama.unavailable` in seconds instead of a
    budget spent waiting. Startup keeps it strictly below `turn_timeout_ms`
    (above it, the transport always wins and the setting reads as on while
    never firing) and at or above 5 s. Pinned by
    `a_turn_that_never_answers_is_abandoned_long_before_the_turn_timeout` and
    `the_silence_guard_is_spent_by_the_first_token` (real time in small
    increments — `start_paused` would auto-advance past the window and prove
    only that `timeout_at` exists).
  - **The runaway-thinking guard counts volume, not time**
    (`[research].max_turn_thinking_chars`, default **8192**, `0` = off) — for
    the turn that never leaves the thinking channel (socket healthy, deltas
    steady, only the deadline would stop it). Two pathologies sized the
    number: a wedged investigation turn runs ~18 chars/s, a wedged report turn
    ~310; the initial 20000 caught only the fast one. 8192 drops the slow
    wedge at ~445 s of a 900 s deadline, at a 3.1× margin over the busiest
    healthy turn measured (2642 chars) — a margin over averages, so a false
    positive is possible and the `warn!` names model and count. Not per-model,
    not request-overridable (nothing good lies on either side of the default).
    An abandoned turn returns an **empty** `ChatOutcome` — every phase already
    recovers from one (plan → plan-less, tool loop → bounded parse retry,
    sufficiency → drop, report → re-ask at shifted seed) — so it is invisible
    in the return value and instrumented in place
    (`research_runaway_thinking_turns` + `warn!`); `TokenTally::record` must
    not let its zero `num_ctx` overwrite a known window. Its GPU cost is
    invisible to `max_tokens` (Ollama's `done` line never arrives on cancel →
    counts `None` → `turns_unreported`). A clock armed on the first thinking
    delta (immune to the cold-start trap) is the instrument that would catch
    the slow wedge early — a later change.
  - **`max_tokens` is the *cost*** (400k/1.2M/6M): `prompt_eval + eval` summed
    over turns. The whole transcript is resent every turn, so cost grows
    super-linearly with turns while steps count linearly — this is the axis
    `max_steps` was pretending to be. Sized from measurement (a medium 8-step
    run = 52149 prompt + 3431 eval); time normally binds first, this catches
    the pathological long-transcript run.
  - **`context_fraction`** (0.5/0.7/0.85) is a *guard* for small-window models,
    where Ollama trims the transcript in silence. Checked against
    `tally.peak_prompt_tokens` *before* the next turn — one turn short of the
    window, never after a trim. **The one axis a request cannot override**:
    raising it buys truncation, nothing else.
  - **`max_steps`** (8/20/64) is the coarse backstop. A step is a poor unit:
    `outline` is one SELECT while `search` is a GPU embed + vector query, one
    turn may ask for several, and a measured run had 20 turns against 16
    executed steps.

  A fifth key, **`search_top_k`**, is not a budget axis: the evidence width of
  one `search` call, 5 at every level (runs that missed an answer already got
  five hits and lost on query *formulation* — raising it buys transcript, not
  coverage); a knob so a harness can sweep it. Trap: research builds a
  `SearchRequest` directly and `search_core` leaves validation to callers — so
  config validation refuses `search_top_k > [search].max_top_k` at **startup**.

  Each stop has a loop-level test (`the_time_budget_ends_a_run_that_still_has_
  steps_left`, `the_token_budget_ends_a_run_the_clock_and_the_step_cap_would_
  not`, `the_context_budget_ends_a_run_before_ollama_would_trim_it`). The time
  one uses **real** time in small increments — the budget is measured with
  `std::time::Instant`, which `tokio::test(start_paused)` does not move.
- **Per-request `budget` is capped by
  `[research].max_request_{seconds,tokens,steps}`** (TOML-only), checked at the
  edge by `validate::research_budget` → 400
  `validation.research_budget_out_of_range` with `field` naming the axis.
  Config validation additionally rejects a ceiling below
  `[research.effort.high]` (which would make `effort = "high"` unreachable via
  `budget`). `GET /config` publishes the whole ladder + ceilings — clients
  render that (three independent hardcoded copies had each drifted).
- **`progress` makes a live run steerable** — `RunProgress`
  (steps/time/tokens/context spent vs granted, plus `turns` and `binding`, the
  axis closest to exhaustion) is emitted once before the first turn, then after
  every executed step and completed turn; `done` carries the same struct plus
  `reason`, so the run's whole cost is on the stream. **No ticker**: a timer
  task would race the cancellation token for a number the client can
  interpolate, and would make the loop's tests clock-dependent.
- **The identifier rule governs code; documentation inverts it, and shipping
  the exception is half the docs feature.** `*.md` files are indexed (language
  `markdown`) and answer "why" questions about as well as source — but indexing
  them alone changed *nothing*: the model never opened a document, because
  `system_prompt`'s loudest paragraph says only identifiers work. That
  paragraph carries its own exception (*documentation is written in English;
  ask it in English*), and the two must never ship apart. Any future prose
  channel (git history is the named one) inherits this rule.
- **`outline`/`list_files` exist because search matches text and code is
  written in identifiers** (measured: an NL query retrieves the *test* (~9),
  the identifier the implementation (~13) — a model that doesn't know a name
  burns budget rephrasing). The intended path is `list_files → outline →
  symbols/search/callers → read_chunks`, and the system prompt says so — that
  instruction is half the feature. Both are pure SQL over
  `project_files`/`project_file_symbols` (`outline_core`/`list_files_core`,
  covered by `idx_project_file_symbols_file`). `outline` reports `indexed`
  separately from an empty symbol list (a wrong path guess and a symbol-less
  file must read differently). `list_files`' glob is **SQLite `GLOB`** (same
  operator as `/search` and `/files`; note `*` crosses `/` there, unlike the
  `.mindex` contract). Errors after stream start are `error` *events* (HTTP is
  already 200); `NoMatch` is a tool result, not a failure.
- **The run's scope is enforced on every tool, and `ToolScope` is why it
  cannot stop being.** (It once reached only `search`/`list_files`, so a run
  scoped to `docs/**` could read any file by naming it.) `ResearchTools` takes
  a `ToolScope` (`research.rs`) as a required argument on every model-facing
  method — a tool added later cannot quietly be the next exception. Evaluated
  in **SQLite** by `build_file_filter` (`src/` has no glob matcher; `globset`
  lives in `tools/mindexfile`), appended as a `file_path IN (SELECT …)`
  subquery, **not** a join (it emits unqualified column names, ambiguous
  against the chunk/symbol tables). Two shapes: path-keyed (`outline`,
  `read_chunks`) get an **explicit refusal** — a third read plus an `in_scope`
  flag mirroring `indexed` (a refusal reading as empty tells the model the
  file is empty); name/text-keyed (`symbols`, `callers`, `grep`) drop rows
  **and count them** against one extra unscoped `COUNT(*)` ("not here" ≠ "not
  anywhere", and `/symbols` calls the second definitive). `callers`' `defined`
  probe stays unscoped for the same reason. All gated on `is_scoped()`, so an
  unscoped run builds byte-for-byte the SQL it always did — which is what
  makes the public `/symbols` sharing these cores provably unaffected.
  `SymbolsRequest` gained optional `include`/`exclude` (the MCP `symbols` tool
  got it free); its binds must be appended **last** (`symbols_core` rewrites
  the role bind by Vec index). `file_versions` is deliberately *not* filtered
  (it only asks about paths already shown; a file leaving the scope must still
  be reported as changed). The scope is also *told* to the model — a
  `system_prompt` paragraph and a `Scope:` line in the state note, both from
  the one `ToolScope::describe` — a wall it has forgotten is a wall it spends
  calls rediscovering.
- **`note` is the run's only durable memory; `grep` is what `search` cannot
  do.** `note(text)` and `revise_plan(plan)` mutate the run, not the index —
  they bypass `ResearchTools` (`apply_local`); both cost a step (pricing stops
  note-churn). Notes are pinned into the state note every turn *and* pushed as
  their own message before the report turn (where the state note is not
  rebuilt) — they are the conclusions the report is written from. Caps are
  announced, never silent (`MAX_NOTES` 24, double `STATE_NOTE_MAX_ITEMS`;
  `MAX_NOTE_CHARS` 500; at the cap the oldest drops *out loud*). `grep` is a
  case-insensitive `LIKE` over `project_file_chunks.code` (`grep_core`);
  **`like_escape` is mandatory** (`_` is a wildcard; unescaped `read_chunks`
  also matches `readXchunks`). It reports the matching line *and* the chunk
  span (the chunk is what a citation verifies against). Cost bounded: narrowed
  by the scope subquery, stopped by `GREP_LIMIT`, refused below
  `GREP_MIN_PATTERN_CHARS`. FTS5 is the real answer, deferred — a table plus an
  invalidation surface is a project, not a tool.
- **`callers` is deliberately an *approximate* call graph.**
  `project_file_symbols` has no target column — a `role='reference'` row
  records a token in call position, not which definition it binds to — but
  carries `parent_name` (the enclosing definition, by byte-span containment),
  so "who calls X" is one indexed SELECT, grouped per (file, definition)
  because raw rows are resent every later turn (`callers_core` +
  `build_callers_query`, testable like `build_symbols_query`).
  `direction: "out"` reads the table the other way (`WHERE parent_name = ?`,
  hence `idx_project_file_symbols_parent`). Edges are exact only up to name
  collision and an aliased import breaks them — stated in the tool description
  **and repeated on every result** (by result-reading time the description is
  thousands of tokens back). An empty answer distinguishes "defined, never
  referenced" from "no such name" (two reads); a top-level reference with no
  parent is reported, not dropped (or totals disagree with the list).
  LSP/SCIP resolution was **rejected** for the product case (lifecycle can't
  be plug-and-play; readiness differs per server and early queries return
  *wrong empty answers*; degradation is per-language and invisible) — this
  tool makes the ambiguity **measurable** first. The property it rests on — a
  tags query tags the enclosing definition with a span covering the call — is
  not guaranteed, so `symbols_cross_language_tests.rs` pins it across five
  non-Rust languages and its allow-list forces a decision when a language with
  a tags query is added. Measured: the model calls it when the question is
  shaped as reach, so the lever is the prompt's wording, not edge precision.
- **The loop terminates on counters, not on a clock** (regression guard).
  Every tool-loop iteration either breaks or increments exactly one of `steps`
  (≤ `max_steps`), `parse_retries` (≤ `MAX_PARSE_RETRIES`) or
  `duplicate_calls` (≤ `MAX_DUPLICATE_CALLS`). A rejected duplicate executes
  nothing, so it must *not* cost tool budget — hence its own cap (without one
  a model repeating one call spins forever: each turn gets a fresh
  `turn_timeout_ms` and there is no cancel endpoint; two such jobs wedge both
  `max_concurrent` slots). The counters stay primary even with the hard
  deadline: a run spinning inside its budget should report `repeated_calls`,
  not a timeout. Adding a rejection path: a new `continue` needs a new bounded
  counter — or, better, price the refusal as a **step**, as every refusal
  added since does (note over cap, grep pattern too short, out-of-scope path).
  The rule binds one level up too: `'phases`' sufficiency re-entry is bounded
  by `reopens ≤ MAX_REOPENS`; revalidation by
  `MAX_REVALIDATION_STEPS`/`MAX_REVALIDATION_TURNS`.
- **A plan turn opens the run, a sufficiency turn closes it** — both toolless
  (`NO_TOOLS`), both answering the same measured problem: **the thinking
  channel is discarded from the transcript** (`ChatMessage` has no `thinking`
  field; `chat_stream` forwards it straight to SSE), so a thinking model plans
  in the one channel erased every turn and re-derives its plan from raw tool
  output — which is what "looping" looks like from outside. `PLAN_REQUEST`
  moves that thought into the replayed channel (the reply is pushed back as an
  **assistant** message); degrades to a plan-less run rather than failing. The
  plan is also the run's only sufficiency criterion — `SUFFICIENCY_REQUEST`
  asks the model to mark each sub-question ANSWERED/UNANSWERED, which either
  re-opens the loop (only if the model *chose* to stop, an axis is unspent,
  and `declares_unanswered` — a substring test on server-dictated vocabulary)
  or rides into the report so "the evidence was insufficient" is a list.
  (Measured: 26 of glm's 36 medium runs were cap-stopped, gemma4 finalized at
  a median of 4 steps with 34% coverage — the same missing criterion at both
  ends; raising `max_steps` 20→48 moved nothing.) The re-open nudge is the
  **one** place `revise_plan` is offered by name — the tool went uncalled in
  28 runs, likely because nothing asked for it where it fits; removing it was
  rejected (without it a run with a wrong plan is re-opened against the wrong
  plan).
- **The run-state note is pinned, not appended** (`RunState` →
  `format_state_note`). One `user` message rebuilt from what the loop already
  tracks (executed queries, symbols, outlines, globs, ranges read, paths
  shown, the plan, the budget position), re-pushed before every turn so the
  model sees exactly one, adjacent to where it generates. Exists because the
  transcript is the run's only memory and by step 19 it is ~165k tokens in
  which "I already asked that" is written nowhere. A `user` message on purpose
  (attributing invented history to the assistant is worse than useless),
  placed after the previous turn's `role: "tool"` replies so the pairing is
  untouched.
- **`num_ctx` is the model's own limit, capped — not a configured target.**
  `OllamaHttpClient` asks `/api/show` once per model (cached; key found by
  `.context_length` suffix, namespaced per architecture) and requests
  `min(model_limit, [research].max_num_ctx_tokens)`. Asking a 32k model for
  65k buys nothing — llama.cpp allocates it and the model degrades past its
  training length in silence. The config key is a **VRAM ceiling** (default
  131072), not a window: `num_ctx` allocates KV up front (~30 KiB/token at
  `OLLAMA_KV_CACHE_TYPE=q8_0`, ~54 at f16). An unreachable `/api/show`
  degrades to the ceiling, never to zero.
- **The model catalog is what makes `GET /config` no longer static.**
  `worker::ollama_catalog` reads `/api/tags` every
  `[research].models_refresh_interval_seconds` (default 300) into an
  `Arc<RwLock<ModelCatalog>>` on `RouterState`; `get_config` publishes it as
  `research.models` — a closed model list instead of a free-text field whose
  typo comes back as `ollama.unavailable` mid-run. Easy breaks: a **failed
  tick keeps the previous list** (`refreshed_at` is *not* re-stamped — the
  only thing separating "Ollama has no models" from "never reached", both an
  empty array); the worker is gated on **nothing** (an Ollama up an hour later
  must be picked up); nothing primes the snapshot before serving (startup
  never blocks on an optional dependency; a `/config` inside the first-tick
  window is the designed degradation). `health()` is a *provided* method over
  `list_models` — one URL, one timeout (`health_timeout_ms`), so ping and
  catalog cannot drift. The `/api/tags` reader is `#[serde(default)]`
  throughout (a shape change degrades to an empty list). No metric:
  `dependency_up{dependency="ollama"}` answers up/down, and a catalog gauge
  could not join `StateMetrics` (cleared-and-repopulated, written by
  `worker/metrics.rs` alone).
- **A non-2xx from Ollama carries its reason in the body, and one class of 500
  is retried in silence.** `chat_stream` reads the error body instead of
  `error_for_status` (which drops it). A 500 whose body contains
  `error parsing tool call` is resent **with the same transcript at the next
  seed**, up to `MAX_TOOL_CALL_PARSE_RETRIES` (`gpt-oss:20b` sometimes mixes
  analysis prose into the call's JSON; 11 of its 36 bake-off runs died this
  way). The fault is one sampled reply, so only `sampling.seed` moves (a
  verbatim resend at a pinned seed rescued only 2 of 4 turns). Deliberately
  **not** a nudge to emit only JSON (that would edit the transcript every turn
  resends, bind the fix to `PROMPT_VERSION`, and coach a model that never
  misunderstood). Safe *because* the 500 arrives before the stream opens. Any
  other status/reason fails at once with Ollama's own words.
- **Token accounting is the run's only trace.** `ChatOutcome`
  (`models/ollama.rs`) carries `prompt_eval_count`/`eval_count` from Ollama's
  `done` line; `TokenTally` folds them per run; `run_research` logs one record
  (steps, elapsed, turns, tokens). Counts are `Option` — an unreported turn
  lands in `turns_unreported`, never as zero. The client WARNs when
  `prompt_tokens` reaches `num_ctx_tokens`: Ollama trims an over-long prompt
  and streams on silently — that log line is the *only* symptom of a truncated
  transcript.
- **`Step` carries a typed `StepCall`** — the wire gives each action its own
  key (`query`/`name`/`path`/`glob`); choosing by string match kept the same
  list in two places with a silent `"query"` fallback when they drifted.
- SSE event contract lives in **four** places that move together:
  `post_research`'s doc comment, its `#[utoipa::path]` 200 description, the VS
  Code client (`tools/vscode/src/api.ts` + `researchView.ts`) and scout's
  reader (`tools/mcp/scout/.../server.py`) — whose `if/elif` chain and field
  whitelists (`_STEP_KEYS`, `_USAGE_KEYS`, `_CITATION_KEYS`) **silently drop**
  anything unknown, so a change that skips them fails by going quiet. Both
  consumers read SSE *per line*, safe only because the payload is
  `serde_json::to_string` (newlines escaped → one `data:` line per frame) —
  keep it that way. Shapes pinned by `progress_wire_fields_are_stable`,
  `done_event_carries_the_reason_and_the_run_cost_on_the_wire`,
  `done_names_no_run_when_the_journal_write_failed`,
  `each_action_names_its_argument_on_the_wire`. `done` carries `run_id`/`seq`
  (how a client offers a watched run back as context), both **null** when the
  journal write failed (a fabricated id would name a run nothing can fetch);
  nullable, not absent — scout reads them explicitly, not through
  `_USAGE_KEYS`. A null `run_id` is *rendered* — the VS Code panel says the
  report was not saved.
- **A stream that ends without a terminal event is a failure, and
  `SseEventStream` says so.** The job is detached, so a panic drops the sender
  and closes the channel — byte-for-byte a completed stream to every consumer
  (a run that panicked in `parse_citations` once read as "finished, no
  report"). The stream tracks whether a `done`/`error` went through and
  synthesises one `error` (`internal.error`) when the channel closes without
  one — an existing event name and `ApiError` code, so the four-place contract
  and `codes_are_stable` are untouched. `SseWireEvent` is generic, so
  streaming `/index` gets the same guarantee free;
  `a_stream_that_ended_properly_gets_no_synthetic_terminal` keeps it from
  appending an error to every healthy run.
- **A report is arbitrary UTF-8; nothing may index it by byte.**
  `parse_citations` once walked backwards over `report[k - 1..k]` — a byte
  slice into a `&str`, which panics mid-multi-byte char (measured in
  production: `gpt-oss:20b`'s `【…】` brackets; Russian prose does it too — the
  run was lost outright). The walk is over bytes now, exactly equivalent
  because `is_path_char` accepts only ASCII (a byte < 0x80 is always a char
  boundary). Guard: `a_report_is_arbitrary_utf8_and_must_never_panic_the_
  parser`. General rule: the only safe indexes into model output are
  `char_indices` or positions a scanner produced from ASCII.
- **The report turn passes no tools at all** — the field is *omitted*, not
  sent empty, so there is structurally nothing to call (on the old protocol ~1
  run in 5 answered "write the report" with one more tool call). It swaps in
  `report_system_prompt` (a writer role). Two backstops for a model that
  writes JSON anyway: the **content gate** in `chat_turn` withholds a reply
  whose first non-whitespace char is `{` (`is_withheld` tells the caller
  nothing reached the client, making a re-ask safe), and a second such reply
  fails the run with `research.no_report`. A withheld reply that is *not* a
  call attempt still gets streamed, in one event. Both passes (draft and
  rewrite) go through the one `write_report`, returning `ReportOutcome`
  (`Written`/`Empty`/`ToolCall` — kept apart because `research.no_report`'s
  detail names which defect to re-ask about). With the gate closed for the
  draft, "nothing streamed" is unconditional, so that re-ask needs no
  `is_withheld` guard.
- **`done` carries a `reason`** (`DoneReason`): `finalized`, else
  `time_exhausted` / `tokens_exhausted` / `budget_exhausted` (steps) /
  `context_exhausted` / `unparseable` / `repeated_calls` — one per `break` in
  the tool loop; the four budget reasons are distinct so a log query can say
  which limit binds. Wire contract (`done_reason_wire_values_are_stable`).
  Scout surfaces `done_reason` + an `incomplete` hint. Adding a `break` means
  adding a variant.

## Output volume is where runs fail (2026-08-01)

A field report from a real investigation on another project, run through
mindex + scout, is the source for everything in this section. Its finding is
sharp and one-sided: **retrieval never failed, writing always did.** The loop
found the right files every time — the step traces confirm it even in the
failed runs, and the author never once had to supply a path. Every failure
landed in the report phase, and the shape of the request predicted it:

- a broad question (five sub-questions, "list everything") → failed;
- "reproduce these three JSON files verbatim" → failed 5 times running, across
  efforts and across `context_run_ids`;
- one file + numbered short questions + a word limit → succeeded, in minutes.

Symptoms were `done_reason: repeated_calls`, `unparseable`, and empty final
reports; one 15-minute run returned nothing at all. The author's own summary:
*"the bottleneck is the writing model, not the index. Ollama collapsed under
output volume, not read volume. Raising `num_ctx` does not help — only
shrinking the required answer does."*

Reading the code against that report corrected two natural assumptions and
turned up one defect nobody had named.

**Nothing had ever bounded the output.** `REPORT_SYSTEM_PROMPT` and
`report_request` carried no length, section or word instruction, and `Sampling`
had three fields — `num_predict` was never sent on any turn of any run. The
de-facto size of a report was `PLAN_REQUEST`'s "3-6 sub-questions" times
whatever the model felt like writing. So `[research.effort.*].max_report_words`
(400/900/1800, `0` = off) now rides in the prompt as a **ceiling** — "at most",
never "about", because a target makes a model write to the number — and arms
`num_predict` at `REPORT_WORDS_TO_TOKENS` (4) × the grant.

That multiplier is ~3× the honest prose ratio on purpose. `num_predict` is a
runaway backstop, not the ceiling: Ollama cuts at a token, so a tight value
severs a code fence, fails `validate_report_markdown`, and buys a full-volume
rewrite of the document that just failed — a long run turned into a lost one.
`research_report_length_caps` firing at all therefore means the multiplier or
the model is wrong. Having the server close a dangling fence was rejected: the
heading is the one defect the server repairs, and a closed fence over a
sentence that stops mid-word passes the gate and is still unusable.

**`verified: 0 / unverified: 0` was `forced_synthesis` on the wire.** This is
the defect the report surfaced without naming. The `citations` event is sent
unconditionally, and for a server-written report `check_citations` runs over a
notice that by construction contains no `path:start-end` — so it scores
`total: 0, verified: 0, unverified: 0`, byte-for-byte what a flawless report
scores, in the exact field scout tells the caller to trust. Every "verified 0
even though it read the files" observation is that collision. The fix is a wire
field, **not** a gate change: `citations.server_written`, sourced from the flag
the journal has always recorded. The fact existed; it just never reached the
wire.

**The ungrounded gate's length exemption was too wide.** `< 800 chars` stays
for a run the *budget* stopped — it had no chance to gather more, and demanding
citations from it demands a fabrication — but a run that `Finalized` declared
its own evidence sufficient, so a short uncited report from it is a
self-contradiction rather than the honest short version.

**`grep` could not say how much it searched.** It already distinguished
out-of-scope, but "the literal is absent" and "nothing here was searchable"
shared one sentence — so a glob matching no file read as proof of absence. That
is exactly how the same literal is honestly reported 0 times by one run and 5
times by the next, which the field report saw and could not resolve.
`GrepResponse` now carries `searched_chunks`/`searched_files`, read with one
extra `COUNT` **only on a miss** (the `out_of_scope` probe's rule: pay for the
second scan where it changes the answer), and `format_grep`'s empty branch is
three-way — the `file_history` three-flag honesty, expressed as counts.

**The report turn's prompt was measured by nothing.** `context_fraction` guards
*between* turns and against the *previous* turn's size; the report turn runs
after the loop breaks, adds the notes block, and in the repair path carries the
whole draft plus a complaint — the largest prompt of the run. It now gets an
`estimate_prompt_tokens` check and `shed_for_report`, which drops the
prior-reports block first (hearsay, never citable, its value already spent),
then oldest tool replies, each **replaced by a naming stub, never removed** —
the pairing invariant is absolute, and a turn announcing three calls followed
by two replies is a malformed transcript. The instructions, question, plan,
notes, verdict, digest and request are never shed.

Two honest caveats on that guard. `CHARS_PER_TOKEN_ESTIMATE` (4) comes from
prose and code tokenizes denser; log the estimate against the turn's real
`prompt_tokens` before trusting it. And measured on this host a run's peak
prompt was ~12k against a 65k window, so `research_report_context_sheds` may
stay at zero forever — in which case this is insurance, and its correctness has
to be *tested* (fabricated transcript, tiny window), not measured. Rebuilding
the report turn from `Evidence` alone was rejected for the same reason: it is
the cleaner architecture, but it changes what the model sees on **every** run,
including the ~100% that already fit.

An **evidence digest** is pushed unconditionally before the report request —
paths and merged spans, no code. It is what the system prompt has always
claimed sits below it, it is what `check_citations` actually scores against,
and it is what makes shedding safe: a tool result can be dropped without
dropping the citability of what it showed.

**The excerpt channel is the answer to "reproduce these three files".** That
request has no good form: it asks the model to retype pages of exact text, and
transcription volume is precisely what breaks. But the server already holds the
bytes — `Evidence` has the paths and spans, SQLite has the code — so the new
`excerpts` event (between `citations` and `done`) ships the indexed code at
every **verified** citation for one SQL read and no GPU. `path_only` and
`unverified` citations are excluded: they name no location worth reading, and
attaching real bytes would dress up a claim the check just refused. Read
through `read_chunks_core`, so the run's scope is enforced — this must never
become how a scoped run hands over bytes its scope refused. Caps drop **whole
chunks**, never cut one.

That channel is what makes the prompt's new "do NOT reproduce code you were
shown" sentence honest rather than a demand the caller pays for; the two ship
together. Scout returns `excerpts_available` always and the bytes only under
`include_excerpts=True` — two dozen chunks is ~100 KB into the caller's
context, the exact cost scout exists to prevent, and defaulting it on would
invert that layer's economics overnight.

`PROMPT_VERSION` 1.3 → **1.4**, MINOR: the job — read this evidence, write one
cited report — is unchanged; only how it is asked for is.

**What is unmeasured, and must not be reasoned about.** 400/900/1800 are
guesses: the field report establishes the direction (shrinking the answer is
what worked) and not the cliff, which is certainly per-model — sweep
`max_report_words` at fixed effort/seed/model and score completion rate.
Whether announcing a budget changes anything at all is the open question
`research_report_words{model}` exists to answer; if granted and actual turn out
uncorrelated, the prompt half of the knob is dead weight and only `num_predict`
earns its place.

## The report is written in sections (2026-08-01)

Bounding the output is necessary and not sufficient: it makes each report
smaller, but the report is still **one turn**, so a model that cannot produce
it still produces nothing. That is the shape of every zero-return run in the
field report. Sectioning removes the all-or-nothing property itself.

A plan of `MIN_SECTIONED_PLAN_ITEMS` (3, what `PLAN_REQUEST` asks for) or more
is written one sub-question per turn. Below that — or with no plan — the run
takes the single-turn path this file has always had, byte-for-byte. That
fallback is not an edge case; it is the safety valve *and* the revert switch,
and it is why the plan turn's documented degradation (a plan-less run rather
than a failed one) stays harmless.

**What each section turn sees.** Its sub-question, the run's own sufficiency
verdict on that item — machinery that already existed and was being thrown
away — the word allowance, and the other sections' **headings only**. Feeding
back their prose would grow the prompt to compensate for shrinking the output,
which is the same bug wearing a different hat. A banked checkpoint draft of the
same section is included when there is one, which is what makes six turns cheap
rather than six cold starts.

**Bounds, because this is a new turn-producing path.** `MAX_REPORT_SECTIONS` 6
(since 2026-08-01 the run's `budget.max_report_sections` — see the
request-tunable section below);
`MAX_SECTION_ATTEMPTS` 2 — deliberately not `MAX_EMPTY_REPORT_RETRIES` (5),
which was sized for the single turn a whole report used to take, and 5 × 6 is
thirty report turns; `MAX_SECTION_REWRITES` 3. Two further checks **stub rather
than stop**: `MIN_SECTION_MS` (10 s) of window left, and
`REPORT_TOKEN_OVERDRAFT` (1.5) of the token budget — the report phase has
always been outside that budget's reach, which was immaterial at one turn and
is not at six. None of these is a `break` in the tool loop, so **no new
`DoneReason` variant**: a failed section is a degradation, not a stop. That is
the first thing to check when reviewing this.

**The repair became proportionate.** The whole-document rewrite instruction
says "it replaces the draft entirely, so repeat everything that should
survive" — a second full-volume generation of the document that just failed, at
the moment the run has least budget left. `defective_sections` maps each failing
citation back to its section and only those are regenerated.
`parse_citations_at` reports byte offsets, converted to char offsets at that one
seam: a report is arbitrary UTF-8, section ranges are counted in chars, and
mixing the two would rewrite the wrong section the moment a report contains one
Cyrillic word.

**Checkpoints are the other half.** `[research].checkpoint_every_steps` (6,
`0` = off) interrupts the loop to bank the sections already answerable into
`RunState::draft_sections` — keyed by plan item, **replaced never merged**,
since a later checkpoint saw more evidence. A checkpoint **costs a step** *and*
is capped by `MAX_CHECKPOINTS` (8), both on purpose: charging it puts its cost
where the operator set the budget, and the cap stops a mis-set interval from
turning a run into a writing exercise. It emits **no `step` event** — there is
no tool call, and a `step` frame with no argument key would break
`each_action_names_its_argument_on_the_wire`. A step invisible on the wire is a
real asymmetry, but a sanctioned one: a rejected duplicate already is exactly
that, and a test pins it so it reads as a decision.

The payoff is the fifteen-minutes-for-nothing case. A section that cannot be
written at the end ships its banked version — the model wrote it, its citations
are real, and its only defect is that it saw less evidence — and
`forced_synthesis` assembles banked findings under a derived title instead of
"**No report was produced.**"

**Consequences worth stating rather than discovering.** `report_timeout_ms`
rises 120 s → 300 s: the window's meaning is unchanged (the tail a caller
waits) but what has to fit inside it is not, and scout's `TOTAL_TIMEOUT` moves
with it — that comment exists because this exact drift has bitten once already.
`validate_report_markdown` splits into `validate_markdown_body` (empty / JSON /
unclosed fence, which every fragment owes) plus the heading check (which only a
whole document owes), because a section legitimately opens with `##`, or with
prose when the server supplies its heading. And **every sectioned run stores
`title = NULL`**: the document's heading is the server's, so
`extract_report_title` finds no model-written one and readers fall back to the
question — exactly what a repaired-heading run already does. Section headings
themselves derive from the *plan*, which is model-authored, so a `path.rs:1-2`
inside a plan item can reach the provenance report; that is the model's own
text and is scored honestly, but the neighbouring `repair_missing_heading` rule
points the other way and the difference is subtle.

`PROMPT_VERSION` 1.4 → **2.0**, MAJOR: the run is asked to do a different job —
write section *i* of *n* blind to the others' prose, and bank partial sections
mid-investigation. No corpus comparison across this boundary is valid.

**What must be measured.** Two things, and neither can be reasoned about.
Sectioning may cost report *quality*: six independently written sections lose
the cross-cutting synthesis a single pass produces and will repeat each other
at the seams. Hand-score coverage as well as completion rate — a jump in the
first paid for by a drop in the second is not a win, and
`MIN_SECTIONED_PLAN_ITEMS` is the switch that turns it off. And checkpoints
spend investigation budget on writing: at 6 against `medium`'s 20 steps, ~15%
of the run's lookups become writing turns, a pure loss for a run that would
have finished anyway. Measure coverage with them on and off at the same seeds,
and be prepared for the answer to be "default 0, enable for `low` only".

### What was measured (2026-07-28)

108 runs — 4 models × 12 questions about this repo × 3 seeds, effort medium,
temperature 0.2 — plus follow-up arms. Harness and corpus revision are gone;
this is the record: evidence for the design choices above, not a benchmark to
re-run.

- **Names before search**: a model that doesn't know a name burns budget
  rephrasing; the discarded thinking channel makes it re-derive its plan every
  turn. `list_files`/`outline`/`read_chunks`, the plan turn, the pinned state
  note and the sufficiency turn each close one measured failure.
- **Depth is not the knob**: `max_steps` 20→48 for glm moved nothing — median
  depth 16→16, `finalized` 3/12 both arms, citations 60→32 (deep reports drift
  to bare `(lines N-M)` forms that cite nothing).
- **Why citations are checked server-side**: `qwen2.5-coder:32b` made zero
  tool calls in 36 runs, answered from its weights in 16 s, declared
  sufficiency every time — all 18 citations unverified. Without provenance
  checking it would have read as the fastest, most competent model.
- **The two winners**: `glm-4.7-flash` — most thorough (48% hand-scored
  coverage, 192/193 citations verified, none invented) at 120 s median / 19
  steps / 160k prompt tokens; stays `[research].default_model`. `gpt-oss:20b`
  — roughly twice as fast (40-45 s, 9-10 steps), best individual answers, but
  cites more loosely (5/151 unverified), never once called `symbols`, and
  needed the tool-call-parse retry to exist. `gemma4:12b`: cheapest, most
  reliable, shallowest (34%, 4 steps, 86% finalized). `qwen2.5-coder:32b`:
  disqualified on integrity.
- **Caveats**: coverage was hand-scored by the author, seed 1 only; trust the
  mechanical columns (all 108 runs). Duplicate rejections are invisible on the
  wire (no `step`), so a model burning turns on rephrasing shows only as turns
  without steps plus an early non-`finalized` stop.

### What was measured (2026-07-30)

28 runs after the hard deadline, new tools and scope enforcement landed: 12
questions × glm and × gpt-oss, effort medium, seed 1, temperature 0.2, plus 3
scope probes and 1 seed-2 control. Mechanical columns only; the corpus of
record is the `research_runs` table (predates the 1.0.0 schema).

- **Scope enforcement holds, nearly free**: a run scoped to `*.md` kept every
  lookup (search, grep, read_chunks) inside markdown and said it could not
  reach the source; a rust-scoped run asked about the Python servers reported
  the question unanswerable instead of inventing — the property that matters,
  and why the ungrounded gate must exempt a run shown nothing. 511 rows hidden
  at a cost of one out-of-scope refusal: the model learns the walls from the
  prompt and state note, not by hitting them.
- **The new tools split by model, oppositely**: `grep` is glm's (13 calls, 11
  with hits; gpt-oss never called it), `note` is gpt-oss's (12 calls; glm
  never wrote one). `revise_plan`: zero calls by either — hence its wiring
  into the re-open nudge. The note cap never bit.
- **Provenance**: 85/85 citations verified, 0 invented/stale/path-only — and 5
  of 24 reports cited nothing parseable, the hole the gate now closes.
- **`max_steps: 20` is the axis under pressure**: time never bound (max 441 s
  of 900), tokens never bound (max 390k of 1.2M — the raise from 400k was
  necessary), context never close; 3 of 24 runs stopped at exactly 20.
  `done.binding` is **not** evidence here — it names the largest *fraction*
  spent, and steps/20 beats time/900 in nearly any run.
- **The deadline works**: glm reproducibly wedges in its thinking channel on
  one question at seed 1 — before, the run died at 600 s with
  `ollama.unavailable` and zero output; after, it stopped at the 900 s
  deadline and shipped a report (seed 2 finished in 120 s: the wedge is
  sampling). 2 of 12 glm runs had a turn over 600 s — a 17% hard failure rate
  at the old `turn_timeout_ms`, which is why that key must sit above every
  budget.

## The report shape became request-tunable (2026-08-01)

The section mechanism shipped with its numbers hardcoded, and the first field
use found the wrong one first: six sections is not always the right plan
shape, and nothing let the caller say so. Four knobs moved into the request —
as new keys **inside `budget`**, deliberately: `ResearchBudgetOverride` is
`deny_unknown_fields`, so an old server refuses them loudly with a 400 instead
of silently running a different run than the caller recorded
(`ResearchRequest` itself has no such guard — a top-level field would be
dropped in silence). Scout needed no schema change: its `budget` dict passes
through verbatim.

| `budget` key | preset | request range | ceiling (TOML, default) |
|---|---|---|---|
| `max_report_sections` | 6/6/6 (`[research.effort.*]`) | 3..=cap | `max_request_report_sections` (12) |
| `max_report_words` | 400/900/1800 (existing) | 0 (=off) or 150..=cap | `max_request_report_words` (4000) |
| `checkpoint_every_steps` | `[research]` scalar, 6 | 0 (=off) or 2..=`max_request_steps` | reuses `max_request_steps` |
| `evidence_width` | 1/1/1 (`[research.effort.*]`) | 1..=cap | `max_evidence_width` (3) |

Decisions of record:

- **`max_report_words` reversed its "never from the request" stance.** The
  original objection was that every value a caller picks is "bigger". What
  answers it is not trust in callers but the ceiling being held to the same
  startup window check as the presets (`words × REPORT_WORDS_TO_TOKENS <
  max_num_ctx_tokens / 2`), so no override can arm a `num_predict` the window
  cannot hold — there is no per-request window check, and none is needed.
- **The plan prompt is templated, not duplicated.** `PLAN_REQUEST` became
  `plan_request(max_sections)` ("3-N sub-questions"), so the plan the model is
  asked for and the sections the server will write can never disagree.
  `PROMPT_VERSION` 2.0 → 2.1 (MINOR: byte-identical at the default 6). The
  test fakes recognise the plan turn by `PLAN_REQUEST_PREFIX`, not equality.
- **The floor of `max_report_sections` is the sectioning threshold.**
  `MIN_SECTIONED_PLAN_ITEMS` (3) doubles as `config::MIN_REPORT_SECTIONS`; a
  grant below it would name a shape the mechanism cannot produce. A caller
  wanting a short report has `max_report_words`.
- **`evidence_width` is one integer multiplier**, not per-tool fields. It
  scales `READ_CHUNKS_LIMIT` 8, `GREP_LIMIT` 20, `CALLERS_LIMIT` 50,
  `FILE_HISTORY_LIMIT` 20, `SYMBOLS_LIMIT` 10 — the per-call evidence widths —
  and deliberately not `outline`/`list_files` (navigation; when 300 rows bind,
  the fix is a narrower glob), not `search` (`search_top_k` stays TOML-only:
  widening it was measured not to be the fix), not `MAX_EXCERPT_*` (response
  caps). Threaded as a `limit` parameter into the research-only core fns and
  stored on `StateResearchTools`; the `ResearchTools` trait is untouched.
  Width is resent every turn, so it compounds into `max_tokens` — the scout
  docstring says "width costs tokens" for that reason.
- **Section mechanics stayed consts.** `MAX_SECTION_ATTEMPTS`,
  `MAX_SECTION_REWRITES`, `MIN_SECTION_MS`, `REPORT_TOKEN_OVERDRAFT`,
  `MAX_CHECKPOINTS` and every loop-termination counter are stability
  guardrails, not tuning — "only genuine tuning knobs are configurable".
- **Validation split by shape.** The spend axes and `evidence_width` keep
  `validation.research_budget_out_of_range` (`1..=cap`); the three axes with
  floors above 1 — two of which accept `0` as the off switch — got their own
  code, `validation.research_shape_out_of_range`, whose detail says "or 0 to
  disable it" only where that is true.
- **Not journalled, no migration.** Shape grants were never journalled
  (`max_report_words` set the precedent), and a `research_runs` rebuild is not
  bought speculatively. The resolved values ride on the "Starting a research
  job." log line — the only record of what a run was granted. Revisit with a
  migration when a measurement harness needs them SQL-queryable.
- **Ceiling risk, stated**: 12 sections × `MAX_SECTION_ATTEMPTS` 2 inside one
  `report_timeout_ms` (300 s) means a caller asking for the ceiling should
  expect `MIN_SECTION_MS` stubs, not a wider window. The 12 is deliberately
  conservative for exactly that reason.

Clients: VS Code adds four budget sliders (`bsections`/`bwords`/`bwidth`/
`bcheckpoint`) on the existing `ConfigBound`/`PresetAxis` pattern — ceilings
from `GET /config`, presets from the ladder (checkpoint's preset is the
`[research]` scalar published beside it), empty = omitted. The one host-side
subtlety: `readBudget`'s `num()` drops 0 (`n > 0`), so `bcheckpoint` has its
own zero-preserving reader, and it rounds a slider's 1 up to 2 rather than
letting the server's floor turn a slider position into a 400.

## A run became something you can see and stop (2026-08-01)

Field report, after four clean runs (39 citations, all verified) followed by an
outage: `/research` stopped answering, and **every instrument said it was fine**.
`GET /health` returned `{"status":"ok"}` with all four dependencies green;
`GET /projects/{guid}/research?status=running` returned `[]`; there was no cancel
endpoint; and `max_concurrent` was published nowhere, so the one number that
would have explained the 429s was unavailable. The only honest advice the session
could produce was "restart the service".

What the code actually said, once read:

- **A run had no name until it ended.** `run_id` was minted inside
  `db::research::insert_run`, i.e. by the journal write at the very end. So
  nothing could name a *running* run — and a cancelled run, which is never
  journalled at all, could never be named.
- **The stored-run list only ever showed finished runs**, by construction: it is
  keyset-paged by `seq`, which a live run does not have. The `status=running`
  filter the operator reached for had never existed.
- **The only live signal was `mindex_research_active`** in `/metrics` — a count
  with no identities. Worse, the scrape handler refreshed
  `research_permits_available` and *not* `research_active`, so a single response
  could report a free permit and an active run at once.

And the likely cause was not a wedge at all. scout holds its SSE connection for
`RESEARCH_TOTAL_TIMEOUT` (4200 s), and an abandoned MCP tool call does not close
the socket. From the server's side a client was still waiting, every deadline was
ticking correctly, and the slot was legitimately spoken for — for up to seventy
minutes. **Externally that is indistinguishable from a hung run**, which is the
real defect: not the holding, the not-being-able-to-tell.

Decisions of record:

- **The id moves to admission.** `post_research` mints it, hands it to
  `RunContext`, streams it as a new first frame (`started`, with the grants and
  the derived `worst_case_ms`), and registers it. One uuid from second zero to the
  stored row.
- **The registry guard rides in the same future as the permit**
  (`backend::inflight`). Anywhere else and the two drift, and a list that
  describes a free slot is worse than no list. Same reasoning as the SQLite
  connection returning itself from inside its blocking task.
- **Cancelling does not remove the registry entry.** The job removes it as it
  unwinds. A cancelled-but-unwinding run still holds its permit, and hiding it
  would report a slot that is not free.
- **`DELETE /research/active/{run_id}` is idempotent (204 either way).** "Already
  finished" and "never existed" are the same state a moment later and the caller
  cannot act on the difference. It takes no lock and is deliberately callable
  mid-turn.
- **The endpoints are global, not per project.** The semaphore is. A caller
  planning a queue needs to know the slots are gone, not that none of *its* runs
  hold them. Folding live runs into `/projects/{guid}/research` was rejected: two
  data sources under one keyset cursor, one of which has no cursor value.
- **A busy slot is not a degradation.** With `max_concurrent = 1` that rule would
  make `degraded` the steady state. `/health` moves its verdict only for a run
  past `max_seconds + report_timeout_ms + WEDGE_GRACE` — which is also exactly the
  watchdog's cancel rule, sharing one const so the two cannot disagree.
- **The watchdog is unconditional**, unlike the metrics collector it would
  otherwise have lived in: gating a recovery mechanism on `[metrics].enabled`
  makes an observability switch decide whether the service can recover. It exists
  for the three awaits not under a token — `effective_num_ctx`'s `/api/show`, the
  error-body read in `post_chat`, and the deliberately uncancellable journal write
  — and for the fourth nobody has added yet. `research_watchdog_cancels_total` is
  expected to stay at **zero**; a non-zero value names the day one of them wedged.

### The instruments that were lying quietly

- **`binding` never meant what it reads as.** It names the axis with the largest
  *share spent*, seeded at `Time`, so a run 12.5% into its clock and less into
  everything else reports `binding: "time"` — read in the field as "this run is
  running out of time". `shares` (the four percentages) now ships beside it, and
  scout promotes both to the top of its result. The field itself is unchanged:
  it is a wire contract, and renaming it would break the consumers that read it
  correctly.
- **`step` reported the request, never the result.** `{action: "read_chunks",
  path: …, hits: 3}` on a 4000-line file names no lines at all, so the trace could
  not answer the one question it exists for. The locations were already collected
  for citation provenance (`Executed::shown`); `spans` is that same list, capped
  with `spans_truncated` rather than cut silently.
- **`effort` had no price.** The ladder publishes what a level *grants* (`high`:
  3600 s); nothing said what it *takes* (measured here: ~400 s). So `effort: high`
  lands on questions that read one dictionary literal, and a queue of two
  investigations is planned as if it were an hour when it is fifteen minutes — or
  the reverse. `research.observed` publishes p50/p90 per `(model, effort)` from
  the journal, on the model catalog's tick and with its keep-the-last-snapshot
  rule. Below `MIN_RUNS_FOR_ESTIMATE` (3) a pair is absent rather than noisy: one
  cold start must not become "what high effort costs".
- **`worst_case_seconds` is derived and published** because `max_seconds` and
  `report_timeout_ms` were both already on the wire and *nobody adds them*. They
  bound different phases; the sum is the only number that answers "how long might
  I wait", and reading the first alone understates `high` by five minutes.
- **A new effort level was considered and rejected.** The complaint that produced
  it ("no fast tier") was really that the existing levels had no published cost.
  `observed` answers that without changing a contract three clients render.

### scout

- **Small excerpt sets now come back unasked** (`AUTO_EXCERPT_BYTES`, 32 KiB).
  `include_excerpts=False` was right about the ~100 KB case and wrong about every
  small one: it charged a second round trip, and it asked the caller to decide
  from a hint whether the literal text was worth reading — the judgement it cannot
  make without seeing it. Field evidence: the two corrections that mattered in a
  real session (a `list(set(...))`, a default argument value) were visible only in
  the verbatim code.
- **`citations_verified`/`citations_total`/`binding` are promoted to the top
  level.** Every *exception* to "trust the report" was already flat
  (`citations_warning`, `freshness_warning`) while the grounds for trusting it sat
  one level down — and `binding`, which `_INSTRUCTIONS` explicitly tells the
  caller to read, was buried in `usage`.
- **429 is handled apart from every other non-200.** It is a queue, not a bad
  request, and the caller's next move differs; the message names the slot count
  and `/research/active` instead of "retry later".
- **A client timeout keeps its own note and gains `live_run_id`.** The generic
  `done_reason` fallback used to overwrite the more specific message, and the
  result never said the obvious thing: the run is still going, still holding a
  slot, and here is the id that ends it.
- **The README was wrong in the way that matters**: it documented
  `RESEARCH_TOTAL_TIMEOUT` as 1800 (code: 4200) and `MCP_TOOL_TIMEOUT=1800000`,
  the exact setting whose old value killed every high-effort run — and its
  signature omitted `budget`, `context_run_ids` and `include_excerpts` entirely.
  A stale README is also, on inspection, the likeliest source of the "docs mention
  effort levels beyond low/medium/high" report: the repo has exactly three
  everywhere, and the budget shape-knob table reads like a fourth if skimmed.
