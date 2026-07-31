# /research — full design record

Companion to `.claude/CLAUDE.md`, which keeps only the condensed invariant
list. This file holds the full rationale, the rejected alternatives and the
measurement history. Read it before modifying `research.rs`,
`models/ollama.rs`, the budget machinery or the SSE contract.


`POST /v0/{guid}/research` — long-lived one-way SSE: a local Ollama model
(`[research]` config, TOML-only) loops search/symbols **via internal cores**
(`search_core`/`symbols_core` in `handlers.rs`; never HTTP-to-self), then
streams a Markdown report. Non-obvious invariants:

- **Cancellation = disconnect.** No cancel endpoint. `ResearchEventStream`'s
  `Drop` cancels the job token (axum drops the SSE body on disconnect); a
  closed mpsc channel is the same signal from the other side. The semaphore
  permit rides **in the spawned job**, not the stream — the job is detached, so
  releasing on stream-drop would over-admit past `max_concurrent` while the old
  job still spends GPU/DB time.
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
  `format_citation_complaint`, `REPORT_SYSTEM_PROMPT`, either report turn's
  user message, the budget nudges or `tool_specs`** — nothing else on the
  stream says which instructions were in force. The run-state note is in scope
  for its *labels* only.
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
  `REPORT_SYSTEM_PROMPT` (a writer role). Two backstops for a model that
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

