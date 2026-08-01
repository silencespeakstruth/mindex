# VS Code extension — full design record

Companion to `.claude/CLAUDE.md` (condensed rules there). Read before
modifying `tools/vscode`.

- VS Code (`tools/vscode`): the **Ask** sidebar WebviewView (`askView.ts`) is
  the one entry point for both query modes — a Search/Research segmented
  toggle over a shared box. An *input surface only*: search results stay in
  the QuickPick (live editor preview + Esc restore), research streams into
  its WebviewPanel tab (steps + live thinking + `marked`-rendered report).
  The SSE client is hand-rolled in `api.ts` (no reconnects — a drop is a
  cancel, by contract). Force reindex lives in the Drift view's overflow
  menu.
  **Research History** (`researchRunsPanel.ts` + `webview/runs.ts`) is an
  editor-area panel, not a third sidebar view (the `icons.test.ts` argument).
  Two panes, a debounced search (`shared/debounce.ts`, vscode-free so
  `node --test` reaches it; trailing — first-keystroke results would be wrong
  on arrival), keyset paging by `seq`, and a multi-select that arrives in the
  Ask form as removable chips. **One `AbortController`, aborted on every
  keystroke**, and the caller must swallow `AbortError` itself: `api.request`
  *rejects* on abort while `research()` resolves — "fixing" the asymmetry
  would break every caller's ability to tell a cancelled request from an
  empty answer.
  **Research is popup-first; the panel is the deep end.** Picking context is
  a `QuickPick` (`researchContextPick.ts`) opened from an `Add…` button
  visible in Research mode **whether or not anything is picked**
  (hidden-until-populated made the feature undiscoverable);
  `browseResearchRuns` is the single-select twin for reading. The picker
  offers **valid runs only** (the server 400s an invalid context id; listing
  one defers the refusal to submit time), tracks selection in
  `onDidChangeSelection` rather than `onDidAccept`'s *visible* selection (a
  pick made under an earlier query is not in `items`), and keeps picked rows
  in `items` for that reason. Cancelling returns `undefined`, which is
  **not** an empty array: one leaves the form alone, the other clears it.
  A stored report opens as a **read-only Markdown document**
  (`researchDocs.ts`, scheme `mindex-research`, `markdown.showPreview`), not
  a fourth webview — outline, find and the user's theme come free, and the
  provider serves from the URI alone so a tab survives a window reload. It
  prepends a provenance block (the stored Markdown says what the run
  concluded, nothing about what it was entitled to claim).
  In the panel: selection means *rows*, not *context* — the checkbox stays
  enabled on invalid rows and `Use as context` is what refuses. The delete
  confirmation names `referenced_by_count` and states it rather than netting
  it against the selection (a summary carries ancestors, never dependants;
  under-reporting in a delete dialog is the wrong way to be wrong). The
  invalid badge shows the **reason**, not the verdict. `removed` carries an
  id *list* so one path serves both deletes, and it must clear the preview
  when the open report is the one going (it used to leave `activeId`
  pointing at a dead id). The header's refresh button is in-panel, not a
  `webview/title` menu — three contribution surfaces for one button that
  would then sit in the tab bar, away from the filters it re-runs; it
  supersedes any pending keystroke (`search.cancel()`) and re-fetches the
  open preview, whose staleness and trust are exactly the numbers that move
  under the panel. The `kind` filter select is backed by the server-side
  `kind` query param — filtered inside the cursor-bounded subquery like the
  rest, so a full page still means "there may be more".
  **Challenge flow** (`challengeFlow.ts` + `startChallenge` in
  `extension.ts`): launching a challenge is a **QuickPick chain** (effort →
  optional model → optional max-seconds), not a form and not an Ask mode —
  the server refuses a question, scope and context on the challenge body
  (`deny_unknown_fields`; all three come from the subject), so there is
  nothing to compose, and popup-first is the codified shape for that. Three
  entry points, one command (`mindex.challengeResearchRun`, three argument
  shapes): the history panel's preview button (passes the summary), the
  streaming panel's post-done button (passes the new `run_id`; suppressed on
  challenge panels — a challenge of a challenge is a server 400), and an
  `editor/title` button on the report tab's *source* view
  (`resourceScheme == mindex-research`; the Markdown preview tab is a webview
  and offers no per-resource title menu — the palette covers it). The
  command re-fetches the detail before the pre-check: kind/valid/trust must
  not be judged on whatever stale shape the caller held. The client
  pre-check (`challengeGuard` in `shared/runsFormat.ts`) mirrors the
  server's two refusals so the button can explain itself; the server stays
  the authority. The stream reuses `ResearchPanel` verbatim (`isChallenge`
  page flag + a tab title naming the subject) and the same single-flight
  handles as `startResearch` — which is what makes degradation-abort and
  Cancel cover challenges for free; the callback block is built by one
  shared `researchCallbacks()` so the two entrances cannot drift. A 429
  names `MINDex: Active Research Runs`.
  **Trust wording lives in `shared/runsFormat.ts`** (vscode-free, tested):
  the wire types keep `kind`/`trust`/`challenge_verdict` as bare `string`
  (`done_reason` precedent — an unknown future value must not become a type
  lie); the unions and narrowing guards live behind that seam, and every
  consumer of the *meaning* goes through them. Inconclusive is never an
  acquittal; unchallenged is silent (a badge on every row is a badge on
  none); a challenge row links its subject (resolved from the loaded page,
  else a link the host resolves by id). The challenges-against list in the
  preview is a client query (`kind=challenge` + filter on
  `challenged_run_id`), fired only when `trust !== "unchallenged"` — derived
  trust already proves the absence of valid challenges, and the stale ones
  it then misses no longer count.
  **Verification renders two halves separately** (`verificationView`):
  provenance is immutable — `provenance_matches: false` impeaches the
  journal, never the code — while staleness is computed against the index
  now and is the number that moves; the Verify button re-fetches on every
  click for exactly that reason. Pre-v1.3.0 rows get the staleness half
  only, with the why spelled out.
  **Active runs** (`activeRunsPick.ts`) are a palette command + QuickPick,
  fetched on open — not a status-bar item and not a `StatusMonitor` hook: an
  occupied slot is a rare state consulted deliberately (usually right after
  a 429 named the command), and permanent chrome or a per-tick poll is the
  wrong price. Cancel re-fetches rather than splicing (the 204 is idempotent
  and says nothing; a run may linger while its job unwinds, which is the
  honest state).
  **The form offers only what the server confirmed exists**: language
  pickers = the project's `chunks_active > 0` languages, model field = a
  `<select>` over `research.models`, both via `StatusMonitor.refresh()` —
  the one place that already runs at activation, on `.mindex` change, and
  after every reindex/delete (it re-reads `/config` every pass: the model
  list is no longer static). Three rules: `undefined` inventory means
  *unknown* (server down, no project, older server) and falls back to
  `ALL_LANGUAGES`, as does an *empty* one (an empty picker is a dead form; a
  superset merely lets a filter match nothing); the `readScope`/submit
  whitelists stay `ALL_LANGUAGES`, **not** narrowed to the inventory
  (offering is an availability hint, validating is a contract); everything
  is pushed by `postMessage` and rebuilt in the webview, never by
  reassigning `webview.html` (a re-render would discard the half-typed
  question, the restored `getState()` and a live run's Cancel state).
  **The form is also gated on what the server can currently do, and that
  gate needs a clock.** `fetchStatus` publishes one `Availability {ask,
  research, reason}`, split because a *required* dependency takes everything
  down (server reports `degraded`) while Ollama takes only Research and
  leaves health `"ok"` deliberately — one flag would either kill Search
  whenever no local model runs, or keep offering Research against a server
  that cannot serve it. The reason names the *required* checks, never
  Ollama. `!research` disables the Research **tab**; `!ask` disables every
  control (Stop excepted — a live run still has a connection to drop),
  leaving the half-typed question visible and inert; the mode is never
  switched out from under the user. A degradation also aborts what is
  running (via `RunRegistry`), resetting handles **before** reporting (a
  notification's thenable resolves only on dismissal — the trap that once
  left Research disabled behind an un-clicked toast), and reports it as a
  failure, not a cancellation (which would read as the user's own Stop).
  None of this is observable without `[mindex.statusPollSeconds]` (default
  30, `0` = off): every other refresh is event-driven.
  **Language marks are vendored, two-toned and tested.** `esbuild.mjs`
  generates `src/shared/langGlyphs.ts` from devicon's *monochrome* SVGs
  (fills stripped so CSS `color` drives them), committed; `sql` alone falls
  back to a codicon. Each language declares **two** colours in
  `media/lang.css` (13 of 21 brand colours fail 3:1 against one of VS Code's
  default backgrounds; the pair is derived by mixing toward white/black in
  5% steps until it clears — `langIcons.test.ts` recomputes the derivation
  and asserts no mark kept a hard-coded fill). Devicon's *font* was rejected
  on size (1.5 MB vs a 181 KB extension).
  Drift's `Sync all` is a synthetic first tree row present **only** while
  there is actionable drift; it reindexes before deleting, so a failure or
  declined confirm still leaves the index better off. Its explanatory prose
  lives in `viewsWelcome`, not `TreeView.message` (VS Code renders the
  message *instead of* the welcome view when the tree is empty — set it only
  once a check has produced rows).
  **The generated `.mindex` reads the project's `.gitignore` files rather
  than guessing** (`gitignore.ts`, pure, no `vscode` import). The rest of the
  template is guesswork and says so — a `dist/` may be checked-in output —
  but a `.gitignore` is the project's own statement of what is generated, so
  those excludes go in live, blocked per source file. Four things make that
  safe, each of which fails *silently* if dropped. **The Rust parser rejects
  what this one accepted**: `mindexfile::build_globset` bails on a leading
  `/` or any `\`, so gitignore's anchoring and escape syntax must be resolved
  at translation time, never carried. **A `!` must never reach the file** —
  picomatch reads it as negation, globset as a literal, and `.mindex` has no
  negation to express it with; since `include_paths` cannot re-admit what an
  exclude dropped, a negation instead *disarms* (comments out) the positive
  rules it overlaps, decided by materializing it into sample paths. Dropping
  them silently is the jemalloc `.gitignore` case — `/test/unit/[A-Za-z]*`
  plus `!…*.*` would delete every unit test from the index with no error.
  **Both glob forms are emitted** for a pattern that could name a file or a
  directory (`target` → `**/target` *and* `**/target/**`): globs are matched
  against file paths only, so the second is what actually excludes a build
  tree. And **the walk prunes as it goes** — a directory the rules so far
  already exclude is not entered, which is what keeps forty `.gitignore`
  files inside `perf/corpus/.clones/` and `.ruff_cache/.gitignore` (content:
  `*`) out of the result. Anything untranslatable is written as a comment
  naming the pattern and the reason; the empty case renders byte-for-byte
  what it always did, which is both the no-git path and the revert switch.
  Nothing re-reads `.gitignore` afterwards — mindex's scope is the `.mindex`,
  and this happens once.
  **A reindex must show the server's claims, not just its own upload.**
  `post_index` swallows the claim conflict (`Err(ApiError::FileInFlight) =>
  {}`) and still 200s with the claimed file *absent from the response* —
  byte-for-byte a hash-skipped file, so the extension once reported a
  refused reindex as `unchanged`. It is now read from two places, neither
  that response: `/status`'s `indexing_claims` drives a live Drift-view row
  and *refuses* to start an upload that would be swallowed, and the
  follow-up `/drift`'s `indexing` bucket is what the summary subtracts to
  say "still indexing" instead of "unchanged" — so the drift check must run
  **before** the summary. The status poll drops to 3 s while claims are
  outstanding (the configured interval stays the ceiling). Every entry point
  funnels through the one `reindex()` helper — what makes its re-entry guard
  total (two concurrent runs raced their own drift checks and could settle
  showing just-indexed files as stale).
  **The run reports itself as a feed, not a percentage, because indexing is
  batched** (a file-granular bar moves in two bursts with the long stretch
  frozen between them — the `▰▱` row is gone and `increment` is no longer
  reported). What is live is `IndexFeed` (`shared/indexFeed.ts`,
  vscode-free): the last five paths, the counters, and a `RateWindow` over
  the server's **cumulative** `chunks_done` rather than a local sum of
  `batch_chunks` (a retry or batch boundary cannot make the two disagree).
  One snapshot feeds two surfaces, and the split is forced: a `withProgress`
  message is structurally **single-line** (`\n` collapses; no multi-line
  API), so the paths live in a `StatusBarItem`'s `MarkdownString` tooltip
  and the toast keeps the one line and the Cancel button only it can hold.
  The Drift view keeps the **claims** row (other clients' work) and nothing
  else — the re-entry guard moved to a `reindexRunning` flag in `activate()`
  (`isBusy` was derived from the deleted progress state).
- MCP `scout` (`tools/mcp/scout/`): token-economy layer, one tool —
- VS Code (`tools/vscode`): `npm run check` = prettier + eslint + `tsc` + the
  `node --test` suite (`src/*.test.ts`, compiled to `dist/`).
- Shell: `shellcheck scripts/entrypoint.sh`, `shellcheck --shell=bash
  tools/search/mindex-search.sh`; format `shfmt -i 4 -ci` (bare shfmt
  defaults to tabs).
- Python (`tests/`): `ruff check`, `ruff format --check` **and**
  `black --check` (kept compatible), `mypy` (`fastapi` is `# type: ignore` —
  stubs only in the mock's image). Run mypy **per directory** — `mypy tests/`
  fails with `Duplicate module named "main"`:
  `for d in tests/integration tests/mock_embedder tests/mock_ollama; do mypy $d; done`.
- Python (MCP servers): the same four, per server —
  `(cd tools/mcp/scout && ruff check . && ruff format --check . && black --check . && mypy src)`,
  likewise for `tools/mcp/mindex`. Easy to forget: neither is under `tests/`.
- SQL: `sqlfluff lint src/db/migrations/` (dialect/layout from repo-root
  `.sqlfluff`; schema is intentionally column-aligned).
- Prefer a scoped `#[allow(...)]`/config exclusion **with a reason** over
  contorting code; never project-wide suppression.

## When modifying code

1. New loops touching Qdrant/SQLite/embedder must respect the
   `CancellationToken`.
2. Multi-row DB writes go inside a `transaction`.
3. New endpoints: register in `backend::http3::run`, use `RouterState`,
   `{param}` routes, `#[debug_handler]`, the `ApiJson`/`ApiPath`/`ApiQuery`
   extractors, return `Result<_, ApiError>`, validate at the top via
   `backend::v0::validate` (new check = new `ApiError` variant + arms +
   `codes_are_stable` + a unit test). Add a `#[utoipa::path]` annotation
   (existing tag, every error `body = ProblemDetails`, a `**Concurrency:**`
   note) **and** an entry in `openapi.rs` `paths(...)` (+ new types in
   `schemas(...)`) — a handler missing there is silently absent from Swagger;
   `openapi_spec_is_complete_and_versioned` guards the count. Swagger UI at
   `/swagger-ui` (assets vendored, no network).
4. Reach Qdrant only via `VectorStore`; collection names via
   `collection_for`.
5. Any search-path SQLite query must include `AND c.status = 'active'`.
6. Status writes use `set_file_status` and must be a legal transition
   (triggers enforce it). New status-changing paths need a transition test.
7. Adding a language → the full checklist under **Languages**.
8. Schema change → new migration in the `MIGRATIONS` slice with the next
   sequential version; startup applies those above `PRAGMA user_version`,
   then stamps it. All SQL `IF NOT EXISTS` (cold re-run = no-op, enforced by
   `every_migration_sql_is_idempotent`). SQLite can't `ALTER` a `CHECK` onto
   an existing table — add new constraints as `BEFORE INSERT/UPDATE` triggers
   (the status-machine pattern, additive). New *columns* are equally blocked:
   `ADD COLUMN` has no `IF NOT EXISTS` form, so it fails the idempotency
   test. **v1.0.0 is frozen** — an in-place edit is skipped in silence on any
   database stamped at 1; first symptom is a 500 with `no such
   table`/`no such column`. New *tables* are the easy case
   (`v1.1.0_git_history.sql`). **Widening a constraint, and adding a column,
   are both answered by the table rebuild** — `v1.1.0_toml_yaml_languages.sql`
   is the precedent; copy its shape: create the replacement under a temporary
   name, copy rows with columns **named** (`SELECT *` binds by position),
   `DROP` the original, rename, recreate its triggers (the `DROP` took them).
   It runs under `SQLite3Pool::migration_transaction` because both halves
   need foreign keys suspended (rename-first makes the children follow the
   discarded table; `ON DELETE RESTRICT` refuses the `DROP`);
   `apply_pending_migrations` pays that back with one
   `PRAGMA foreign_key_check` before stamping. Idempotency comes from the
   leading `DROP TABLE IF EXISTS <tmp>`. Rehearse on a copy of a real
   database and compare row counts per table. **A 1:1 side table is not the
   answer for a new field** (the three that existed each cost a hot-path
   JOIN); `v1.2.0_research_context.sql` is the precedent for rebuilding to
   add columns. One rebuild consequence: the FK suspension also suspends
   `ON DELETE CASCADE`, so dropping the old table does **not** take child
   rows with it — `id` surviving the copy is what makes them still resolve;
   load-bearing, pinned by
   `rebuilding_research_runs_keeps_the_baselines_that_reference_it` (through
   an ordinary transaction the same migration silently erases every child
   row).
9. Changing how chunks or symbols are derived → bump the matching const under
   **Derivation versions** — that is what makes the change reach files
   already indexed; skipping it leaves them stale behind a matching hash,
   silently.
10. Changing which files a project contains, how a path is spelled, what
    bytes are hashed, or which files a client refuses to post → **the full
    list under Four clients, one working-tree view**, in the same commit. One
    client changed alone is not a smaller version of the change; it is
