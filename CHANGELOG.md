# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Every component ships under one version: the server, `mindex-index`, `mindex-watch`,
the `.mindex` parser, both MCP servers and the VS Code extension. A component with no
changes of its own is still released, so "which version am I running" has one answer.

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

[Unreleased]: https://github.com/silencespeakstruth/mindex/compare/v1.0.1...HEAD
[1.0.1]: https://github.com/silencespeakstruth/mindex/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/silencespeakstruth/mindex/releases/tag/v1.0.0
