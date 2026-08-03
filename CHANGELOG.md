# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Every component ships under one version: the server, `mindex-index`, `mindex-watch`,
the `.mindex` parser, both MCP servers and the VS Code extension. A component with no
changes of its own is still released, so "which version am I running" has one answer.

## [1.1.0] — 2026-08-03

Gives research reports an **opponent** and an **offline re-verification**, writes them
**section by section** so a run that runs out of time still returns findings, makes
`GET /health` **tri-state** so a client stops guessing which dependency matters,
publishes the whole workflow at **`/llms.txt`**, and halves the on-disk size of a
collection. Withdraws the `callers` tool and the reference half of the symbol table,
which is the one removal here. A broad stability pass gave every wait a number.

### Upgrading — REQUIRED. Until you do this, search returns nothing and says nothing

`COLLECTION_SCHEMA_VERSION` moved `v1` → `v2`, and it is **not self-healing**: the new
name names no collection, an empty one is created, SQLite still reports every file
`indexed`, and every search comes back empty with no error anywhere.

1. Stop the server.
2. Start it once. Migrations 5 and 6 apply in place.
3. **Reindex every project**: `mindex-index --root <repo> --force`. This also rebuilds
   the symbol table, which migration 6 needs (`SYMBOLS_DERIVATION_VERSION` is `1.1`).
4. **Drop the leftover `*_v1` Qdrant collections by hand.** They still hold the whole
   pre-upgrade index and nothing reaches them.

Step 4 is deliberately not automated: leaving the old collections in place is what makes
a rollback possible. New in this release, `worker::stale` runs at startup and hourly and
publishes `mindex_stale_collections` and `mindex_orphaned_collections`, so the state
between steps 2 and 4 is visible rather than silent. Both gauges are seeded at `-1`,
never `0` — `0` is the healthy reading and an unreachable Qdrant must not be able to
spell it. The runbook is in `docs/claude/qdrant.md`.

Two smaller notes: **`role:` on `POST /v0/{guid}/symbols` is now a `400`**, not an
ignored field (see below); and `PROMPT_VERSION` is `2.7`, so reports written before this
release are not directly comparable — partition a stored corpus on it.

### Added

- **Reports have an opponent.** `POST /v0/{guid}/research/{run_id}/challenge` is a
  research run whose subject is a stored report: same loop, same semaphore, same
  budgets, its own citation gate. The subject is injected as hearsay under examination
  and **never seeds the evidence** — re-deriving every location through the tools *is*
  the refutation. A closing verdict turn scores each claim `CONFIRMED` / `DISPUTED` /
  `REFUTED`, and the stream carries one new event, `verdict`, between `excerpts` and
  `done`; an ordinary run's stream is byte-for-byte unchanged.
  - **The grounding cap is what makes this safe with a weak model, and it is
    symmetric.** A challenge is `grounded` when it verified at least one citation and
    its unverified citations do not outnumber them. An **ungrounded `refuted` caps at
    `disputed`** — an accusation that showed no code refutes nothing — and an ungrounded
    `confirmed` resolves to NULL, because an unshown *acquittal* is not one either. One
    surviving citation out of nine was enough to launder a challenge that had checked
    nothing; that is measured, not hypothetical.
  - Zero parseable verdict lines is NULL: **challenged, inconclusive**, which no reader
    may render as an acquittal.
  - **Trust is derived at read time, never stored** — over *valid* challenges only, so a
    challenge whose own evidence goes stale stops counting by itself. Severity wins
    (`refuted` > `disputed` > `confirmed` > `unchallenged`). One challenge stands per
    report: a newer one **with a parseable verdict** evicts the older, inside the same
    transaction as its own insert. An inconclusive run evicts nothing — it produced no
    finding, and letting it erase a `refuted` would spend the mechanism's most valuable
    output on its least informative outcome.
  - Refused when the subject is invalid (`400 research.challenge_subject_invalid` —
    staleness must not be spendable as refutation) or is itself a challenge
    (`research.challenge_subject_is_challenge`; trust aggregation is single-level).
- **Offline re-verification.** `GET /projects/{guid}/research/{run_id}/verification`
  re-runs the citation check as a pure function over journal rows — no model, no GPU.
  It answers two questions and keeps them **separate**: *provenance* is immutable and
  must match the recorded counters, so a mismatch is a journal bug and never news about
  the code; *staleness* is computed against the index as it stands now and is the number
  that actually moves. Nothing is stamped, so it can never disagree with a
  recomputation.
- **Migration 5** (`v1.3.0_research_verification.sql`) makes the run journal structured:
  `research_run_evidence` (the shown spans — the one `check_citations` input that
  otherwise died with the run, and what makes the offline check possible),
  `research_run_citations` (per-occurrence verdicts in report order) and
  `research_run_steps` (calls, arguments and landing spans; no result bodies — the code
  is in the index). Trace rows are built at the same sites as the `step` SSE frames from
  the same locals, so wire and journal cannot drift.
- **`GET /llms.txt`** — the whole workflow as one document, so pointing an agent at a
  URL is enough to get it started. Static narrative plus a live section rendered from
  the same snapshot `GET /config` serves: available models, the effort ladder, and the
  **measured** p50/p90 per model and effort. Absent data is stated as absent rather than
  papered over with an invented value. Deliberately outside the OpenAPI spec, and a test
  asserts the absence so the omission reads as a decision.
- **A report of three or more plan items is written one section at a time.** The report
  used to be a single turn, so a model that could not produce it produced *nothing* — a
  fifteen-minute run returning zero. Each numbered sub-question now gets its own turn,
  the server assembles the document, and a section that fails costs that section rather
  than the document. Below three plan items the run takes the old single-turn path
  byte-for-byte, which is both the safety valve and the revert switch.
- **Checkpoints make a stopped run return findings.** `[research].checkpoint_every_steps`
  (6; `0` disables) interrupts the tool loop to bank the sections already answerable.
  A section that cannot be written later ships its banked version, and a forced synthesis
  assembles real findings instead of "No report was produced." It costs a step, so it is
  visible in the operator's budget, and is capped so a mis-set interval cannot eat a run.
- **A request can shape the report.** `max_report_sections`, `max_report_words`,
  `checkpoint_every_steps` and `evidence_width` join the per-axis budget overrides, each
  capped by a new `[research]` ceiling that config validation refuses to set below what
  `effort.high` already grants. `evidence_width` is one integer multiplier on how much a
  single lookup returns; it deliberately does not scale navigation tools or `search`.
- **A run is named from admission and listed while it runs.** `run_id` is minted before
  the work starts, streamed as the first frame, and registered — so `GET /research/active`
  lists live runs and `DELETE /research/active/{run_id}` cancels one whose caller
  abandoned it without closing the socket. Before this a run had no id until it ended:
  with `max_concurrent = 1` an occupied slot was an unattributable total outage whose
  only remedy was a restart. A `429` now names the endpoint that explains it.
- **`GET /health` is tri-state, and the server owns the verdict.** `ok`; `degraded`,
  meaning only the **optional** Ollama is failing — exactly the state where a client
  should keep offering search and stop offering research; `unhealthy`, meaning a
  required check failed or a research run is wedged. Severity wins, so Ollama down *and*
  Qdrant down is `unhealthy`. Two words rather than three was the defect: every client
  then needed its own copy of which check is required, and the VS Code extension's did
  not match the server's. `checks.*` is now exactly `"ok"` or `"error"` — the reason a
  probe failed goes to a log with a sysadmin hint, because this response is readable by
  anything that can reach the port and a driver's error chain carries paths, URLs and
  versions. Clients must test `== "ok"`, never a prefix.
- **`[research].allowed_models`**, a glob whitelist compiled once at startup (empty =
  any). Checked before the semaphore, so a refused model costs no slot; `GET /config`
  publishes the model list already filtered by it, plus the raw patterns.
- **A toolless model is refused before it costs anything.** `/api/show`'s `capabilities`
  is now read and cached alongside the context length, and a model that does not declare
  `tools` is a `400` at admission instead of a slot, a model load and a wasted turn. It
  is three-valued on purpose: only an explicit "no" refuses, since a pre-flight that
  cannot be performed is not a refusal.
- **Three contention guards, all shipping armed.** `[research].max_turn_seconds` (300)
  abandons a turn that is still producing but has consumed a whole run's wall clock —
  measured twice on one host, 985 s at ~1.5 tok/s and 912 s for 702 tokens, each time a
  single plan turn eating the run. `slow_turn_tokens_per_second` and
  `slow_turn_unaccounted_ms` warn without stopping anything; they are **independent**,
  and neither may gate the other, because the second exists precisely for the case the
  first cannot see (time spent queueing behind another client falls outside Ollama's own
  accounting entirely). Ollama's `load_duration` / `eval_duration` / `total_duration`
  were previously parsed by nobody; they now ride on `progress` and `done` as
  `generation_ms` / `model_load_ms` / `unaccounted_ms` / `eval_tokens_per_second`.
- **`GET /config` publishes what a run costs, not only what it grants.** `observed` is
  measured p50/p90 per `(model, effort)` from the journal, and a pair with too few runs
  is absent rather than noisy. Also `worst_case_seconds` per level, since `max_seconds`
  and the report window bound *different phases* and reading the first as the whole wait
  understated `high` by five minutes.
- **A step reports where it landed** (`spans`, `path:start-end`), from the same locations
  citation provenance is scored against. `hits: 3` on a 4000-line file named no lines,
  which made the trace unusable for the only thing it is for.
- **Collection metrics.** `mindex_stale_collections` and `mindex_orphaned_collections`
  (see Upgrading), `mindex_project_vectors` (Qdrant's own point count per project — the
  only detector for a lost vector volume, and a project the store cannot answer for is
  *absent*, not zero), `mindex_search_orphaned_winners`, `mindex_search_unscorable_winners`,
  `mindex_worker_running` / `mindex_worker_exits_total`, and
  `mindex_research_unjournalled_runs` — the denominator every research rate lacked, since
  the three endings that write no row were absent from every per-run metric at once.
- **VS Code — Research History is now the one reading surface**, an editor-area panel
  with a challenge launcher, kind/trust badges linking challenge to subject, a Verify
  action for the offline re-check, an active-runs picker, and a garbage-collection review
  that proposes invalid, stale, partial and inconclusive runs (pinned exempt) with the
  reasons visible. `mindex.browseResearch` is gone; `ctrl+alt+,` opens the panel.
- **VS Code — `.gitignore` writes the excludes it already knows.** Creating a `.mindex`
  translates every `.gitignore` in the project, nested ones included, naming the file
  each block came from and commenting out what it cannot express rather than guessing.
- **VS Code — three new settings**: `mindex.requestTimeoutSeconds`,
  `mindex.streamIdleTimeoutSeconds` (an **idle** clock, never a total one — a `high` run
  may legitimately live 70 minutes) and `mindex.indexingPanel`.
- **Prebuilt binaries.** `mindex-index` and `mindex-watch` for Linux, Windows and macOS
  (Intel and Apple silicon), the server for Linux x86-64, and the `.vsix`, all built on
  native runners by a new release workflow. `mindex-watch`'s filesystem watching has only
  ever been exercised on Linux inotify.

### Changed

- **The ColBERT vector is stored as fp16, with no HNSW graph.** Measured on this repo's
  own index, ColBERT was **99.6% of the collection's bytes** — 838 MB per segment against
  2.6 MB dense and 0.5 MB sparse, roughly 1.85 MB per chunk. `datatype: Float16` halves
  that and is not a quality trade the way quantization would be: this vector only
  *orders* a pool dense and sparse already agreed on. `hnsw_config.m = 0` builds no graph
  at all, which is correct only because ColBERT is always the outer query over a prefetch
  pool and never an entry point. This is what `COLLECTION_SCHEMA_VERSION` `v2` is.
- **`callers` is withdrawn, along with the reference half of the symbol table and the
  repo map that ranked by it.** The reference rows were measured rather than assumed:
  23 810 of them against 3 397 definitions — **87.5% of the table** — serving one
  model-facing tool called **twice** across twenty-five recorded research runs at a 50%
  miss rate. The edges were lexical, so the most-referenced names here were `assert_eq`
  (1084), `clone`, `Ok`, `unwrap`, `map`, several with exactly one definition in the
  tree. Separating a core abstraction from a name shared with a language builtin is name
  resolution, which is the wall this project declines to climb. `grep` answers "who uses
  this name" lexically **and says so**, which is the honest version of what `callers`
  implied. `parent_name`/`parent_kind` survive — for a definition, the enclosing
  definition is what makes `Gc::collect` readable.
  - **Migration 6** (`v1.4.0_symbol_definitions.sql`) drops `project_file_symbols.role`,
    now that every value is `'definition'`. It does not delete the reference rows:
    symbol rows are wholly derived, so `SYMBOLS_DERIVATION_VERSION` `1.0` → `1.1` removes
    them on the next indexing run, keeping the rule in one place.
  - **`POST /v0/{guid}/symbols` now rejects `role:` with a `400`.** Accepting and
    ignoring it would answer a `role: "reference"` query with the *definitions* — the one
    wrong answer that costs nothing to detect and looks exactly like a right one.
- **A score that cannot be compared ranks last, not first.** `total_cmp` orders `+NaN`
  above every finite value, so the plain descending sort handed the **top result slot** to
  a chunk the reranker could not score. NaN results are ranked last rather than dropped —
  the chunk really did match the filters — and counted, because the symptom otherwise
  reads as a ranking-quality complaint rather than the misconfigured embedder it is.
- **Every wait has a number, and it is the server's, not the library's.**
  `[qdrant].timeout_ms` / `connect_timeout_ms` exist because the client's own default was
  5 s and no knob reached it: a project whose rerank ran past that failed **every** search,
  untunably. `[model].encode_timeout_ms` now bounds the whole call rather than each
  attempt — per attempt the worst case was forty minutes at the defaults. `GET /health`
  runs its probes concurrently under a fixed 3 s ceiling; they were sequential, and the
  SQLite one was the file's only transaction without a cancellation token, so a wedged
  pool hung the one endpoint that must always answer. Shutdown now drains for 8 s instead
  of logging "Shutdown complete." while in-flight batches were torn out mid-flight.
- **HTTP/3 now streams frame by frame.** The response body was buffered whole before the
  first send, so `/index?stream=yes` and `/research` did not stream over h3 at all — the
  client saw nothing until the run ended, up to seventy minutes, while the server
  accumulated every event in memory. Both endpoints exist *because* their output is worth
  watching arrive.
- **A prose tool call is detected in two notations.** A model that calls tools natively
  all run long can still write markup on the report turn, where no tools are passed;
  JSON-only detection let that through every gate meant to catch it, and a run shipped and
  journalled a "report" whose whole body was three fake calls.
- **A cited path is resolved before it is scored.** A cited path may be the unambiguous
  tail of exactly one shown path; two candidates resolve to none. The failure this fixes
  is not a parser gap but `unverified` — the verdict for a path no tool returned — being
  handed to a report about a file it had just read, five times in one run. `citations`
  gained `path_resolved` as the honesty counter, plus `shown_paths` (how many files the
  run saw the *inside* of) and `server_written`, because a forced-synthesis report cites
  nothing by construction and scored byte-for-byte what a clean report scores.
- **A run that looked at nothing while holding hearsay is refused.** The ungrounded gate
  exempts a run with nothing to say; a run handed prior reports or a challenge subject has
  somebody else's answer to hand, and the same uncited prose is then that answer restated
  as findings, in the field callers are told to trust.
- **Research metrics are charted with `increase()`, and the report phase is charted at
  all.** The Grafana dashboard gained the report-phase row, which is where runs actually
  fail; rare histograms are drawn as points with gaps kept.
- **VS Code — a degradation freezes the form and never the tabs.** Both mode buttons stay
  live in every state: a disabled tab is a dead end whose explanation lives behind it.
  Every server-touching button single-flights, supersedable for reads and refused for
  writes. No raw error reaches a user — one funnel, machine codes to the log.
- `mindex-watch` is described as a filesystem watcher rather than an inotify daemon, since
  it is now released for three platforms.

### Fixed

- **Recovery ran under the request's own token, so it was a no-op in the one case it
  exists for.** The pool short-circuits on a cancelled token *before* touching the
  database, so a cancelled or disconnected request left every prepared file `indexing`
  until the 30-minute stuck-grace sweep. The unit test passed a fresh token and so agreed
  with the bug.
- **A failed status write read as a success.** `set_file_status` returns a `bool` covering
  a DB error, a trigger rejection and a 0-row update, and it was discarded: the retry
  worker reported `"indexed"` on the strength of a write it never checked, so a database
  that had stopped accepting writes kept a clean success rate while every file stayed
  stuck. It is `#[must_use]` now, and the legitimate discards are written as such with
  their reason.
- **The retry worker inferred "no chunks" from a failed read.** Behind an
  `unwrap_or_default()`, a `PoolEmpty` or a locked database silently promoted files to
  permanently-indexed-and-empty, at `info!`. A read that fails now leaves the file for the
  next sweep.
- **A crash read as the client hanging up.** A panicked pool task became `Cancelled`,
  which told the client it had closed a connection it never closed, told the dashboard a
  disconnect, and silenced every call site's log — while permanently costing a pool
  connection. It is its own variant now, counted as `outcome="panic"`; after
  `db_pool_size` of them every request failed with `database.busy` and nothing said why.
  The reachable instance was `grep` slicing a string with an offset found in its
  lowercased copy (`İ` grows by a byte), so any indexed file containing one made `grep` a
  way to dismantle the pool four requests at a time.
- **A dead background worker was invisible.** Workers were bare spawns with the handle
  dropped, so a panic stopped GC or the retry sweep permanently and in silence —
  indistinguishable from a healthy idle system. They are supervised now, publishing
  `worker_running` *before* the task starts, because a series that never existed cannot be
  alerted on.
- **`PoolEmpty` was an unretryable-looking 500 that produced no log line at all** — the
  likeliest production failure, invisible. It is `503 database.busy` with a hint.
- **GC reported success for a pass that could not run.** Each phase returned a bare count
  with every error mapped to `0`, and the run counted as `ok` whenever the token was live,
  so a GC failing for days looked idle and `POST /gc` answered 200 with zeros either way.
  Phases now report whether they finished, and `failed_phases` names them on the wire.
- **A missing Qdrant collection was treated as a GC failure, which made the backlog
  permanent.** "Keep the row until the vector is confirmed gone" then meant *never*: chunk
  rows were unsweepable, their file rows unprunable behind the RESTRICT FK, and the
  backlog grew for the life of the deployment — in exactly the state a lost Qdrant volume
  leaves behind, where GC needs to work most. A missing collection is a confirmation now,
  checked only after a failure, and only a definitive answer converts.
- **`cancel_overdue` re-reported what it had already cancelled.** A run wedged in an await
  its token cannot reach stays registered, so the sweep re-cancelled, re-warned and
  re-counted it every 30 s, turning a counter documented to stay at zero into an unbounded
  number describing one event.
- **`show_facts` cached failures**, making one blip permanent for the process and silently
  running every later run of that model at the configured ceiling instead of its own
  window. Successes only, now.
- **Ollama's two failure classes are two codes.** `ollama.unavailable` is unreachable or
  reachable-and-mute; `ollama.error` is Ollama answering *with* an error, nearly always a
  model that is not pulled. Collapsed into one, a client could not word the message or
  decide whether re-reading `/health` would say anything — and for a typo, health is green
  every time.
- **A turn against a live but mute socket spent the whole budget waiting.**
  `[research].first_token_timeout_ms` (120 s) bounds the silent prefix only, armed across
  the request *and* the wait for the first delta, since Ollama holds the connection open
  while it loads a model.
- **A scope that admits no file is refused at admission.** Without it such a run refuses
  every lookup and then reports the question unanswerable, which reads as a finding about
  the code: the commonest spelling (`"src/"`, where SQLite `GLOB` wanted `"src/**"`) cost a
  measured 302-second run with zero citations and no error anywhere.
- **A report missing its heading is repaired rather than refused**, when that is the sole
  problem — and always *after* the citation check, since the derived heading comes from the
  question and a server-written line must never enter the provenance report.
- **An empty `grep` result had three meanings and one spelling.** Out-of-scope, nothing
  searchable, and genuinely absent are now distinguished, and a miss whose pattern carries
  regex punctuation says the match was literal: `\.bwp` → 0 against `.bwp` → 7 is a false
  negative the reader was handed as proof of absence.
- **`embed_and_upsert` trusted the row counts off the wire.** `zip` silently truncated a
  short response, leaving a file marked `indexed` with vectors missing and no error
  anywhere; a long one indexed out of bounds. Relatedly, a `Vec::with_capacity` from an
  unvalidated `u32` asked for ~100 GB on a corrupt body and aborted the process.
- **A section turn could ship the checkpoint's sections again, or somebody else's.** A
  reply whose numbered headings are all *other* items is not this section written badly; it
  is a second document, and one was measured shipping sections 1-2 twice.
- **Search returned `200` with an empty list when every winner was orphaned** — the
  reassuring spelling for the case that means the two stores disagree, while an over-narrow
  filter gets a `404`. It is a `404` uniformly now, and the orphans are counted.
- **VS Code: a reindex read its result from the `/index` response**, which swallows claim
  conflicts and answers `200`, so a refused reindex read as `unchanged`. It reads the
  server's claims and the follow-up drift check instead.

## [1.0.1] — 2026-07-31

Adds a second channel for research — **git history** — turns its stored reports into a
validity-tracked corpus the model and the reader can both browse, streams indexing
progress over SSE, and indexes TOML and YAML. The VS Code extension got the rest of the
attention. Also fixes an indexing failure that could take a whole pass down with it,
and a research run that could vanish without a trace.

### Added

- **Stored research is a validity-tracked knowledge graph.** Every finished run is
  browsable (`GET /projects/{guid}/research[/{run_id}]`), keeps or drops its place in
  the retention sweep (`POST …/{run_id}/pin`), and can be fed to a later question as
  prior context (`context_run_ids`). Validity is **derived at read time**, never
  stored: a run is valid when its own files are unmoved *and* every run in its
  transitive context chain still exists and is itself valid. Staleness can heal, and
  a deleted parent leaves a dangling id that the recursive CTE reads as invalid
  immediately — so there is no cascade to write and nothing to keep in step.
  Migration 4 (`v1.2.0_research_context.sql`) rebuilds `research_runs` for `seq`,
  `expires_at` and `context_run_ids_json`, and adds `research_run_files`.
- **Batch delete for stored research** — `DELETE /projects/{guid}/research` with
  `{"ids": […]}`. A corpus is pruned in handfuls, and one request per pick is one
  chance per pick to fail halfway; this is a single transaction. Unknown ids are
  ignored, so it is idempotent like the single-run delete. An **empty** list is a 400
  (`selector.empty`) rather than a whole-corpus wipe, and the batch is capped by the
  new `[limits].max_research_delete_ids` (TOML-only, default 500) — over it,
  `validation.research_delete_too_many`.
- **Two significance counters on every run summary**: `references_count` (how many
  reports this one was built on) and `referenced_by_count` (how many were built on
  it, counted across the whole corpus rather than the page). The first is
  deliberately *not* `context.length`, which is the transitive ancestry — a run built
  on one report that itself rests on three reads `1` and lists four. The second is
  what makes a delete confirmation honest: removing a run invalidates every
  descendant, and the caller is owed that number before agreeing.
- **VS Code — research is a popup-first surface.** An `Add…` button beside the
  context chips opens a QuickPick over the stored corpus, and it is visible in
  Research mode whether or not anything is picked; while the block was hidden until
  it had contents, the feature could only be found by already knowing about the
  History panel. `Browse Stored Research` is the single-select twin for reading.
  Stored reports open as read-only Markdown documents in their own tab (scheme
  `mindex-research`, rendered by VS Code's own preview) with a provenance header, so
  a report can sit beside the code it describes. A live run's header lists the
  reports it was built on as clickable chips. Research History gained batch delete,
  an `Ask again` action, visible retention (`pinned` / `3d left`), and both counters.

- **`POST /v0/{guid}/index?stream=yes` streams indexing progress as SSE.** The
  default (`stream=no` or absent) keeps the one-shot JSON summary byte-for-byte, and
  both modes run the identical pipeline — the query parameter only decides how the
  result travels. The stream reports `started`, per-file `prepared`/`skipped`
  (`unchanged`/`in_flight`/`cancelled`), one `embedded` per GPU embed batch with
  cumulative `chunks_done`/`chunks_total` and the server's own `elapsed_ms`, per-file
  `indexed`, then exactly one terminal `done` (whose `files` is the JSON mode's
  response body) or `error` (the stable `ApiError` code, since the HTTP status is
  already 200 by then). Closing the connection cancels the request and recovers the
  batch exactly as a dropped JSON request would. A mistyped `?stream=` value or key
  is a 400, never a silent fall-through to the mode the caller did not ask for.
- **`mindex-index` progress is now a measurement, not an estimate.** The bar consumes
  the SSE events: the file counter advances as the server settles each file instead
  of once per 100-file batch, the status line names the file being worked on, and
  chunks-per-second is computed over a 20-second sliding window fed by the per-batch
  `embedded` events — the old figure was a cumulative average over a counter that
  jumped once per batch response. Against an older server the client detects the
  plain-JSON answer and degrades to exactly the previous behaviour.
- **VS Code — live reindex progress.** The reindex notification and the Drift view's
  progress row now show `settled/total files · N chunks/s · <path>` from the same
  stream, updating per file and per embed batch (throttled) instead of once per
  batch. The Drift row's tooltip no longer has to disclaim that a posted batch may
  still be on the GPU — a file now counts only once it is settled.

- **TOML and YAML are indexed** (`tree-sitter-toml-ng`, `tree-sitter-yaml`), sliced by
  the ordinary AST walk like JSON. Neither grammar ships a tags query, so they
  contribute no symbols. `Cargo.toml`, `config.example.toml`, `docker-compose*.yml`
  and every `pyproject.toml` were previously dropped in silence: the three extension
  maps carried no entry for them, which is the "silently skipped file" failure the
  Languages checklist exists to catch.
  - **Migration 3** (`v1.1.0_toml_yaml_languages.sql`) widens the
    `programming_language` CHECK on `project_files`. It is the first migration that is
    not purely additive — SQLite cannot alter a CHECK, so the table is rebuilt — and
    the first to need `SQLite3Pool::migration_transaction`, which suspends foreign-key
    enforcement for the rebuild and verifies the result with `PRAGMA foreign_key_check`
    before stamping `user_version`. No manual step: an existing database upgrades on
    the next start, and the `.toml`/`.yml` files appear on the next indexing run.
  - The VS Code extension labels both: YAML gets devicon's mark, TOML a codicon —
    devicon draws none, the second language after `sql` for which that is true.
  - CI and deployment YAML (`.github/**`, `deploy/**/*.yml`) are excluded from this
    repository's own `.mindex`.
- **Git history channel.** `project_commits` and `project_commit_paths` (migration 2,
  `v1.1.0_git_history.sql`) record what each commit touched and why. Opt-in and
  metadata-only: no embeddings, no Qdrant points, no chunks, no derivation version.
  - `POST /v0/{guid}/history` reconciles a commit set; `DELETE /v0/{guid}/history`
    prunes it by `keep_last` and `older_than`, intersected.
  - `mindex-index --history` walks the refs named by `git_refs` in `.mindex`;
    `--history-only` runs that phase alone, `--git-ref` scopes it.
  - `file_history` is the research loop's tenth tool. Historical claims must still be
    anchored to a `path:start-end` with the sha named in prose — a sha is
    content-addressed and needs no server-side citation grammar.
- **`git_refs:`** as a `.mindex` key.
- **VS Code — language marks.** Every language is drawn with its official mark
  (devicon, vendored at build time) in the Ask view's filters and the status panel's
  inventory and failed lists.
- **VS Code — Sync all.** One action in the Drift view that reindexes every stale and
  missing file and drops every orphan from the index. Present only while there is
  drift to clear.
- **VS Code — inline reindex progress**, in the Drift view itself rather than only in
  a corner notification, and it reports the server's own in-flight work as well as
  this window's uploads.
- **VS Code — `mindex.statusPollSeconds`** (default 30, `0` disables): a background
  health poll, which is what lets the Ask form stop offering work the server cannot
  currently do.

### Changed

- **The slicer cuts on token boundaries where a line boundary does not exist**, and
  both slicers clamp their configured window below what Qdrant can store.
- **VS Code — the Ask form is gated on server health.** Research disables itself when
  the server's Ollama goes away; the whole form disables when a *required* dependency
  does, and a run in flight is aborted rather than left to time out.
- **VS Code — the status panel reads as a dashboard**: health checks carry colour and
  an `optional` badge, the SQLite pool is one inline meter, and the failed-files card
  is hidden entirely when nothing has failed.
- `PROMPT_VERSION` is `1.3`. Research reports written under 1.0.0 and 1.0.1 were
  written under different instructions and are not directly comparable; if you are
  keeping a corpus, partition on it.

### Fixed

- **A research run whose report contained a non-ASCII character next to a citation
  was lost outright.** `parse_citations` walks backwards from `:<digits>-<digits>` to
  collect the path and did it by slicing the report at a **byte** index, which panics
  when that byte falls inside a multi-byte character. `gpt-oss:20b` writes
  OpenAI-style `【…】` citation brackets; one landing before a line range killed the
  job thread, so no `done` event reached the client, `journal.record` was never
  called, and the run left no row and no error — the report streamed to the screen and
  then simply did not exist. Russian or any other non-ASCII prose in the report does
  the same. The walk is over bytes now, which is exactly equivalent because the path
  character class is ASCII-only. As a side effect the bracketed form parses correctly
  instead of crashing.
- **An SSE stream that ended without a terminal event looked like success.** Both
  `/research` and `/index?stream=yes` spawn their job detached, so a panic aborts the
  task, drops the sender and closes the channel — which is byte-for-byte what a
  completed stream looks like to every consumer. The stream now tracks whether a
  `done`/`error` went through and synthesises one `error` (`internal.error`) when the
  channel closes without one. No new event name and no new code, so the SSE contract
  and its consumers are unchanged.
- **A run the server did not save now says so.** `done.run_id` has always been null
  when the best-effort journal write failed or the report failed the Markdown gate,
  and no surface rendered it — so a report that would never appear in Research
  History was indistinguishable from one that would. The VS Code panel now warns
  above the report, while the text is still there to copy.
- **VS Code: deleting the open report left it rendered.** The Research History
  preview kept showing a run that no longer existed, with the selection pointing at a
  dead id. The pane resets, and it now remembers which report was open across a
  reload the way the query and the selection already did.
- **`selector.empty` named a field the request did not have.** The batch research
  delete reuses the rule, but its selector is `ids`, not `include`/`exclude` — so the
  error pointed a client at a field that does not exist in that body. The code is
  unchanged (the rule is one rule); the `field` pointer and the detail now name
  whichever selector the endpoint actually takes.
- **The Grafana dashboard's whole Research row read as empty while the runs were
  there.** A labelled metric family is created by the first event carrying its label
  set, so its first scraped sample is already `1` — there is no preceding zero for
  `rate()` to subtract from, and `research_runs{model, done_reason}` normally sees
  exactly one run per label set per process lifetime, so the series sat flat at 1
  until restart and every panel built on `rate()` drew zero. The metrics themselves
  were correct throughout, and high-traffic families hid the defect because their
  second event arrives seconds after their first. Research counters and per-run
  histograms are now charted with `increase()`, which counts a new series' first
  sample, and drawn as bars with a `sum` legend rather than as a per-second line;
  quantiles over a rare histogram are drawn as points with the gaps kept. The
  "Unverified citation share" stat gained `or vector(0)` — the healthy case is that
  no `unverified` series exists, and "No data" is indistinguishable from a broken
  query.
- **An oversized chunk could fail a whole indexing batch.** A file with no line
  boundaries — a minified JSON, one unwrapped paragraph of prose — produced a single
  chunk of unbounded size, which Qdrant refuses above 1 048 576 multivector elements
  and which exhausts the embedder's GPU memory before that. Several hundred innocent
  files in the same pass went unindexed, and reruns failed in the same place.
- **VS Code: a reindex against a busy server silently did nothing.** The server
  answers `200` with claimed files simply absent from the response, which is
  indistinguishable from a hash-skip — so the upload finished instantly and reported
  the files unchanged at the moment it had in fact been refused. The view now shows
  the server's claims, refuses to start against them, and the summary tells the two
  apart.
- **VS Code: concurrent reindex runs.** A second press started a second run over the
  same paths; their drift checks then raced, and the view could settle showing
  just-indexed files as still stale.
- **VS Code: `mindex.noVerify` could not be reached.** The client read `mindex.caCert`
  with an unguarded `readFileSync` in its constructor, so a path naming a file absent
  from this machine threw at activation — every command dead, and the one setting that
  would have connected anyway unusable, because the read that failed came first. The
  CA is now read outside the constructor, a failure is a warning naming the path, and
  `noVerify` overrides `caCert` rather than queueing behind it.
- **VS Code: TLS settings now ride on each request, not only on the shared agent.**
  With `http.proxySupport` at its default `"override"` the extension host may
  substitute its own proxy agent and discard ours — taking `rejectUnauthorized` with
  it. That is what made both settings look inert behind a corporate proxy.
- **A Windows clone reported permanent drift.** Git's default `core.autocrlf=true`
  rewrites the working tree to CRLF on checkout, and mindex hashes `code.as_bytes()` —
  so every client saw every file as changed, reindexed the whole tree on every check,
  and nothing errored anywhere. `.gitattributes` now declares `* text=auto eol=lf`
  repo-wide, which overrides the setting rather than leaving it to each contributor,
  and a minimal `.editorconfig` keeps an editor from putting CRLF back.

### Upgrading

- **The database migrates in place**, on the next start. Migration 2 is additive — two
  new tables — and was verified non-destructive against a copy of a real 1.0.0
  database. Migrations 3 and 4 rebuild a table each (`project_files` for the widened
  language CHECK, `research_runs` for `seq`/`expires_at`/`context_run_ids_json`),
  which SQLite cannot express as an `ALTER`; both run under
  `SQLite3Pool::migration_transaction`, which suspends foreign-key enforcement for the
  rebuild and verifies the result with `PRAGMA foreign_key_check` before stamping
  `user_version`. Nothing needs reindexing — `.toml` and `.yml` files simply appear on
  the next indexing run.
- **`.mindex` files using `git_refs:` require 1.0.1 tooling.** The parser rejects
  unknown keys by design, so a 1.0.0 `mindex-index` or `mindex-watch` will fail on
  one. Files that do not use the key are unaffected in both directions.
- The git history channel stays off until `--history` is passed.
- **An existing Windows clone must run `git add --renormalize .` once.**
  `.gitattributes` applies at checkout, so a tree already written out as CRLF stays
  that way until it is renormalised — and until then it keeps reporting drift.

## [1.0.0] — 2026-07-30

First release. An HTTPS API server that indexes repositories locally and answers
semantic search, exact symbol lookup and research questions over them, with vectors in
a local Qdrant, metadata in a local SQLite file and embeddings from a local BGE-M3
server.

### Added

- **The server**: `tree-sitter` AST chunking, BGE-M3 multi-vector embeddings
  (dense/sparse/ColBERT), RRF fusion with a ColBERT rerank. 21 programming languages
  plus Markdown.
- **Wire contracts**: RFC 7807 errors with namespaced machine codes, OpenAPI 3.1 at
  `/api-docs/openapi.json` with Swagger UI, OpenMetrics at `/metrics`. All three are
  snapshot-tested.
- **`POST /v0/{guid}/research`**: a local Ollama model runs an investigation over the
  index and streams back a cited Markdown report. Citations are provenance-checked
  server-side before the report is shipped.
- **`POST /drift`**: read-only comparison of a posted `path → sha256` manifest against
  the index, classifying every file `stale` / `missing` / `orphaned` / `indexing`.
- **Tools**: `mindex-index`, `mindex-watch`, `mindex-search.sh`, the `mindex` and
  `scout` MCP servers, and a VS Code extension.

[Unreleased]: https://github.com/silencespeakstruth/mindex/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/silencespeakstruth/mindex/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/silencespeakstruth/mindex/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/silencespeakstruth/mindex/releases/tag/v1.0.0
