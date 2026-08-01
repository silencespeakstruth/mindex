# CLAUDE.md — mindex architecture & conventions

Only what is **not obvious from reading the code**: invariants, non-trivial
"why", gotchas, regression guards. No flag tables (`--help`), no per-test lists,
no language table (the `ProgrammingLanguage` enum + `Cargo.toml`), no struct/SQL
dumps. Accepted limitations are stated next to the invariant they qualify.
Detail companions live in `docs/claude/` (research, git history, VS Code) —
this file keeps the invariants; read the matching companion before modifying
that area.

## Overview

`mindex` is an async RAG indexing + search engine in Rust. HTTPS API →
`tree-sitter` AST chunking → `BGE-M3` multi-vector embeddings
(dense/sparse/ColBERT) → `Qdrant` vectors + `SQLite3` metadata. Internal
service: TLS is the only transport security, no API auth.

TLS verification is uniform across every client: **OS trust store** by default
(where mkcert/corporate roots live), extra CA via `--ca-cert` / `ca_cert` /
`MINDEX_CACERT` / `mindex.caCert`, and `--no-verify` / `MINDEX_NO_VERIFY` —
verifies *nothing*, exists only for the self-signed cert `scripts/entrypoint.sh`
generates. Two sprung traps: reqwest's `rustls-tls` trusts only bundled Mozilla
roots — a Rust client needs `rustls-tls-native-roots` to see the OS store
(without it a locally-issued cert fails every request; this silently stopped
both CLI tools once); and the MCP `drift` tool **shells out to `mindex-index`**,
so a CA setting that misses the child process breaks one tool while the rest
work.

Remote access is a *proxy's* job; the server stays unauthenticated and
loopback-bound. For such a proxy every client carries one optional header,
`X-Api-Key` (`--api-key` / `api_key` / `$MINDEX_API_KEY` / `mindex.apiKey`).
**Additive**: unset sends no header (direct path byte-for-byte unchanged;
mindex never reads it), and it travels on the *client*, not per-request, so it
reaches every endpoint — including `mindex-search.sh`'s `/config` probe, which
behind a gate would otherwise quietly fall back to its built-in language list.
Prefer the env var; a flag value is visible in `ps`.

## Configuration (TOML file + CLI flags)

`config.rs` owns it; indexer and watcher CLIs mirror the scheme in their own
`config.rs` (`mindex/indexer.toml`, `mindex/watcher.toml`; `*.example.toml`).
Precedence **CLI flag > TOML > compiled default**. Defaults live *only* in
`Default` impls — clap holds no `default_value` (every flag is `Option<T>`, so
"passed" ≠ "absent"; that's what makes layering work). `resolve()` finds the
file by XDG canon (`--config`/`$MINDEX_CONFIG` → `$XDG_CONFIG_HOME` →
`$XDG_CONFIG_DIRS`; missing file = defaults), logs every path checked, source
loaded and flag override, then validates: *all* problems collected (not
fail-fast) with what/why/how-to-fix; any error aborts (`deny_unknown_fields`
makes a typo a parse error). Keys carry unit suffixes
(`*_ms/_seconds/_minutes/_days/_chunks/_tokens/_bytes/_points/_mib`).

**Only genuine tuning knobs are configurable.** Structural invariants stay
`const` next to their code with a "why not configurable" comment (`VECTOR_DIM`
1024, `ENCODE_MAGIC`, `COLLECTION_SCHEMA_VERSION`, HTTP 499, the SQLite
PRAGMAs). Config reaches code through constructors/params, **never globals**.
New knob = key in the right `config.rs` section + `Default` + validation rule,
threaded to the consumer. Request-shape limits are knobs too: `[limits]` and
`[search].max_top_k`/`max_query_bytes` bound requests at the API edge (via
`RouterState` → the validation layer); they are **TOML-only** — tuning them in a
container means mounting a `config.toml`.

## Layout (non-obvious bits only)

- `tools/indexer` (`mindex-index`) and `tools/watcher` (`mindex-watch`) are
  **own crates with own `Cargo.lock`, not in the root workspace**, both
  path-depending on `tools/mindexfile` (the `.mindex` parser);
  `tools/mcp/{mindex,scout}` are Python/Poetry MCP stdio servers;
  `tools/search/mindex-search.sh` is the bash search frontend.
- `embedder/` is the vendored BGE-M3 server (3 heads) — **host-run + GPU, NOT
  in the Docker image** (`embedder/README.md`). On this host: a systemd
  **template** `mindex-embedder@{egpu,igpu}` (units in `embedder/systemd/`,
  symlinked into `~/.config`), two torch backends (ROCm / Intel XPU) in two
  venvs (`.venv-%i`), mutually exclusive via a symmetric `Conflicts=`+`After=`
  naming both instances (systemd drops the self-reference). `@igpu` is the
  default (leaves the discrete card to the research LLM); `@egpu` for bulk
  reindexing (~17× faster per batch; query path ~28 ms either way). The backends
  are not bit-identical and nothing checks it (the split-embedder warning under
  **Retrieval pipeline**, across time), but measured interchangeable (dense
  cosine 0.999996, sparse Jaccard 0.9968) **only with XPU off its default
  attention kernel**, which returns NaN for padded fp16 rows and still answers
  200 — `attention_backend()` in `__main__.py`; removing it silently corrupts
  every batch of more than one text.
- Migrations in `src/db/migrations/`. **Four**: `v1.0.0_schema.sql` (version 1,
  the whole 1.0.0 schema), `v1.1.0_git_history.sql` (2, adds `project_commits`
  + `project_commit_paths`), `v1.1.0_toml_yaml_languages.sql` (3, rebuilds
  `project_files` to widen its `programming_language` CHECK),
  `v1.2.0_research_context.sql` (4, rebuilds `research_runs` for
  `seq`/`expires_at`/`context_run_ids_json`, adds `research_run_files`). The
  applied set is the `MIGRATIONS` slice in `main.rs`, keyed by the integer in
  `PRAGMA user_version`; the filename version is documentation. **v1.0.0 is
  frozen** — the filter is `version > user_version`, so an in-place edit never
  reaches a database stamped at 1 and is skipped in silence. Nine tables:
  `projects`, `project_files`, `project_file_chunks`, `project_file_status_log`,
  `project_file_symbols`, `research_runs`, `research_run_files`,
  `project_commits`, `project_commit_paths`. **No 1:1 side tables** — the three
  that existed (an `ADD COLUMN` workaround) were folded back into their parents;
  each cost a hot-path JOIN. A new *field* is a table rebuild (rule 8);
  `research_run_files` is a genuine 1:N child. `.sqlfluff` raises
  `large_file_skip_byte_limit` — sqlfluff skips files over 20 kB with only a
  warning, so without it the schema is silently unlinted.
- `scripts/entrypoint.sh` generates a self-signed cert on first container start.
- `rust-toolchain.toml` pins 1.95.

## Core invariants (violating these causes bugs)

**Project isolation = collection + has_id filter.** One Qdrant collection per
project, `{guid_simple}_v1` (`COLLECTION_SCHEMA_VERSION`, `qdrant.rs`); always
derive names via `collection_for(project_guid)`. The candidate set is a `has_id`
filter built from SQLite (`qdrant_guid` for chunks matching project + filters +
**`status='active'`**) — the *sole* isolation mechanism, also excluding
soft-deleted vectors. It grows linearly with active-chunk count — fine at this
scale; a very large collection would want a stored `project_guid` payload field
+ `match` filter.

**Append-only hot path.** Indexing never deletes from Qdrant. On reindex
(sha256 mismatch): old chunks marked `deleted` in SQLite, new inserted
`active`, new vectors upserted; old vectors orphan until GC (decouples indexing
latency from Qdrant delete latency).

**Symbols parallel chunks, but hard-delete.** `project_file_symbols` (defs/refs
from the language's upstream tree-sitter tags query — one universal extractor,
`slicing/symbols.rs`, zero per-language code; vendored queries in
`slicing/queries/` where the crate exports none) has no Qdrant counterpart, so
its lifecycle is the opposite of chunks: hard `DELETE`, no soft-delete/GC.
Invariant: every tx that marks a file's chunks `deleted` (reindex-prepare,
`DELETE /files`, `/cancel`, `drop_cancelled`) deletes its symbols in the same
tx; `DELETE /projects/{guid}` drops them in its one hard-delete tx. Inserts
happen in the prepare tx alongside chunk inserts. FK RESTRICT backstops the
`prune_deleted_files` ordering. Extraction failure degrades to "no symbols"
(WARN), never fails indexing. `POST /v0/{guid}/symbols` is exact-name lookup
returning **ranked candidates + full totals** (collisions are contract, never a
single "the" answer); ranking is purely path-based (anchor file > its exact dir
> rest). Empty result = 200, not 404.

**GC hard-deletes only confirmed rows** (regression guard, `worker/gc.rs`). A
sweep deletes from SQLite *only* chunks whose Qdrant `delete_batch` succeeded;
failed collections keep their rows `deleted` for the next sweep (SQLite-first
would orphan the vector forever). If every collection in a batch fails, the
loop breaks rather than spinning. The same pass prunes
`project_file_status_log` (`[workers].status_log_retention_days`, default 30)
and runs `prune_deleted_files` — drops `deleted` `project_files` rows once
their chunks are gone (guard: `NOT EXISTS` over *any* chunk row; FK RESTRICT,
so only after the sweep); that ordering is what makes `DELETE /files`
eventually physical. `POST /gc` runs the same `gc::collect` synchronously. GC
is **global**, serialized by `GcGuard` (`Arc<AtomicBool>`): `POST /gc` during a
running pass → **409**; the hourly worker skips its tick if a manual pass holds
the flag; the guard frees on `Drop`, so a panic can't wedge GC off.

**Status state machine** (`project_files.status`), enforced by SQLite triggers
(`project_files_status_{insert,update}_guard`). Legal moves: **any →
`indexing`** (incl. `deleted → indexing`, resurrection), **any → `deleted`**,
and **`indexing` → `indexed`|`cancelled`|`failed`**; a new row may only enter
as `just_uploaded`/`indexing`. Anything else raises
`SQLITE_CONSTRAINT_TRIGGER`.

- `indexing` is committed durably *before* heavy work (crash-recoverable; the
  retry worker picks up files stuck longer than `--stuck-grace-mins`, default
  30). That grace **must exceed the longest in-flight request** — cross-file
  batching holds a whole batch in `indexing` through the embed pass.
- A stuck file with **no active chunks** (sliced to 0) is marked `indexed`, not
  `failed` (`failed→indexed` is illegal — a wrong `failed` would trap it).
- `sha256` is (re)written on entering `indexing` and confirmed at `indexed`;
  the `retry_count` reset lands only on `indexed`.
- Status writes go through `db::files::set_file_status` (stamps
  `status_updated_at`, WARNs on rejection); AFTER-triggers log every transition
  to `project_file_status_log`. A file exhausting `MAX_RETRIES` (3) stays
  `failed` forever (`warn_permanently_failed` surfaces it at startup + hourly).

**sha256 + derivation-version skip / empty 404.** Identical content is skipped
by hash — but only if the *derivation versions* also match (a hash answers "did
the file change", not "did the deriving code change"). `file_already_indexed`
requires `project_files.chunks_version` and `symbols_version` to equal the
current consts; both nullable, and NULL never matches. `post_search` returns
404 immediately when the SQLite candidate set is empty, without calling Qdrant
(avoids a 503 from a missing collection).

**Internal versions are all one notation: `MAJOR.MINOR`, as a string.** MINOR =
the *way* something is produced changed; MAJOR = its *shape* did. All compared
by plain equality, never ordered — both halves trigger the identical rebuild.
The set: `CHUNKS_DERIVATION_VERSION`, `SYMBOLS_DERIVATION_VERSION` (both
`"1.0"`), `PROMPT_VERSION` (`"2.1"`). Deliberately outside it:
`COLLECTION_SCHEMA_VERSION` (`"v1"`, a collection-*name* component) and the
migration `i32` in `PRAGMA user_version`.

`COLLECTION_SCHEMA_VERSION` is the one version with **no mismatch detection and
no self-healing**: bump it and the new name names no collection —
`ensure_collection` makes an empty one, SQLite still reports every file
`indexed`, search returns nothing, no error anywhere. A bump means reindexing
every project by hand.

**Derivation versions** (two nullable columns on `project_files`), stamped by
the same prepare-tx upsert that moves the file to `indexing` — the tx that
writes the chunks/symbols they describe, so a row cannot claim a version whose
rows were never produced:

- `CHUNKS_DERIVATION_VERSION` (`slicing/traits.rs`) — the AST walk, node
  selection, left-extension rule, tokenizer. **Bump when a change would give
  different chunk boundaries for the same source.** Expensive (re-slice,
  re-embed, re-upsert). The `[slicer]` token window is deliberately *not*
  covered — it is config; retuning is the operator's call.
- `SYMBOLS_DERIVATION_VERSION` (`slicing/symbols.rs`) — `queries_for`, the
  vendored `.scm` files, the extraction walk, the grammar crates. **Bump on any
  new/edited/vendored tags query, an `ALL` variant change, a `SymbolExtractor`
  change, or a `tree-sitter-<lang>` bump that alters tags output.** Cheap (pure
  CPU) — separate precisely so a tags fix doesn't cost a full reindex.

Bumping is the *whole* action: the next ordinary `mindex-index` run rebuilds
affected files by itself. After a symbols bump use `mindex-index
--symbols-only` (body flag `symbols_only`): replaces symbol rows in one tx per
file, no slicing/embed/Qdrant — ~20× faster (0.3 s vs 6.5 s on this repo). It
skips files whose hash no longer matches (their chunks are stale too); run an
ordinary pass for those. Not bumping is the motivating failure: the symbols
feature shipped without one, hash-skipped files never gained symbols, and
`/symbols` answered "no such symbol" (contractually *definitive*) for a third
of the tree. Caveat the consts cannot see: a grammar-crate bump in `Cargo.lock`
changes tags output with the const untouched — bump by hand.

**FK is RESTRICT.** `project_file_chunks → project_files` is `ON DELETE
RESTRICT`. Never delete a parent row while chunks exist; mark chunks deleted,
let GC clean up.

**Management endpoints** (`handlers.rs`, routed in `http3::run`, *not* under
`/v0`). Full behavior in handlers + OpenAPI; the non-obvious parts:

- `DELETE /projects/{guid}`: immediate hard delete — rows first, collection
  dropped **last** so a retry re-attempts it; idempotent 204.
- `DELETE /projects/{guid}/files`: **soft** delete; `include`/`exclude`
  selector in the **body** (globs don't fit the path); empty selector = 400;
  204 if none matched, else 200+count.
- `POST /cancel`: same body selector + empty-400, matches **only
  `status='indexing'`** (a too-late cancel is a no-op); marks chunks `deleted`,
  `indexing → cancelled`. Takes **no** `IndexClaim` (so it can interrupt a held
  one); correctness against a live `/index` rests on **two re-reads**, not a
  lock: `post_index` runs `drop_cancelled` between Phase 1 and 2, and the retry
  worker re-checks status *after* acquiring the claim (else
  `cancelled → indexing`, a legal move, would resurrect it). A cancel landing
  mid-embed lets the pass finish; `mark_indexed`'s `UPDATE` carries `AND status
  = 'indexing'`, so it matches 0 rows, the illegal transition is never
  attempted, the file stays `cancelled`, the rest of the batch succeeds, GC
  reclaims the orphans. The trigger is the backstop, not the mechanism;
  `mark_indexed` is the one status write not going through `set_file_status`.
- `POST /retry`: requeues `failed` files; **empty body = all failed**
  (non-destructive). **Metadata-only**: `retry_count = 0`, status stays
  `failed` (skips triggers, takes no claim), and `status_updated_at` untouched
  so the retry worker's failed-branch cooldown (`status_updated_at < now-60`)
  fires on the next sweep, not after a fresh grace.
- `POST /drift`: **read-only**. Posted `path → sha256` manifest (capped by
  `[limits].max_drift_files`) classified against SQLite: `stale` (hash
  differs), `missing` (not indexed; `failed` counts), `orphaned` (indexed,
  absent from manifest), `indexing` (in-flight, excluded from `stale`/`missing`
  since its stored hash is the *incoming* value). Unknown project ≠ 404 — every
  posted file is simply `missing`. Backs `mindex-index --check`, the MCP
  `drift` tool, the watcher's sweep.
- **Stored research** (`GET /projects/{guid}/research[/{run_id}]`,
  `POST …/{run_id}/pin`, `DELETE …/{run_id}`): the browse half of the corpus,
  on the management plane like `/files`; the run that *produces* a report stays
  at `POST /v0/{guid}/research`. The list is keyset by `seq`, searches
  `question` **and** `report` with `like_escape` (FTS5 is the next ladder rung;
  `LIKE` unmeasured-insufficient at this corpus size), and never selects the
  report body — which is why it is a separate endpoint from the detail. `pin`
  is the one mutation on an otherwise append-only row; its `pinned` **defaults to
  true**, so `{}` pins — required, it made the obvious call on an endpoint named
  `/pin` a 400 naming a field the caller had no reason to guess, and unpinning is
  the direction worth spelling out. `DELETE …/research` (no
  `run_id`) is the **batch** form, body `{"ids": […]}`; empty list = 400
  `selector.empty`, capped by `[limits].max_research_delete_ids`; unknown ids
  ignored (idempotent). Each summary carries `references_count` (direct edges
  out) and `referenced_by_count` (direct edges in, whole corpus), from the
  `edges` CTE validity already builds. `references_count` is **not**
  `context.len()` (that is *transitive* ancestry). The inbound count is what
  makes a delete confirmation honest — removing a run invalidates every
  descendant. Summary columns are counted by `RESEARCH_SUMMARY_COLUMNS`, and
  the detail query indexes its four columns *from* that constant (a summary
  column added without moving it once handed the caller `invalid_flag` where
  `report` belonged).
- **Live research runs** (`GET /research/active`, `DELETE
  /research/active/{run_id}`): global, not per project — the semaphore is. Kept
  off `/projects/{guid}/research` because that list is keyset-paged by `seq`,
  which a live run has not got yet. See the `/research` section for the registry
  behind them.
- The read-only set (`GET /projects[/{guid}][/files]`, `/status`, `/config`,
  `/health`, `/version`) + `POST /gc` are self-describing in OpenAPI.
  `GET /config` serves the canonical supported-language list (read by the
  search frontend); `/files?status=failed` is the dead-letter view. `/config`
  is static **except `research.models` and `research.observed`** — both worker
  -refreshed on a tick; don't cache it once.
- `GET /projects/{guid}` is the per-project **inventory**; the per-language
  *file* count is the load-bearing half. Keyed on chunks alone, a language
  whose files are all `failed` or sliced to zero chunks was absent from the map
  — indistinguishable from a language the project lacks, which is a different
  answer ("indexed, and search will still find nothing"). That distinction is
  what lets the VS Code pickers offer only `chunks_active > 0`.

## /research (SSE, Ollama-driven)

`POST /v0/{guid}/research` — long-lived one-way SSE: a local Ollama model
(`[research]` config, TOML-only) loops tools **via internal cores**
(`search_core`/`symbols_core` in `handlers.rs`; never HTTP-to-self), then
streams a Markdown report. **Full rationale, rejected alternatives and the
measurement record live in `docs/claude/research.md` — read it before
modifying `research.rs`, `models/ollama.rs`, the budgets or the SSE
contract.** (Design decisions marked "measured" point to the 2026-07-28
108-run and 2026-07-30 28-run corpora summarized there; the corpus of record
is the `research_runs` table.) The hard invariants:

- **Cancellation = cancelling the job token; two hands reach it.**
  `SseEventStream`'s `Drop` (disconnect) is the primary one and still the only one
  the loop knows about; `DELETE /research/active/{run_id}` is the second, for the
  case disconnect cannot cover — a caller that abandoned the run while its socket
  stays open (scout holds its connection up to `RESEARCH_TOTAL_TIMEOUT`, 70 min).
  The semaphore permit rides **in the spawned job**, not the stream (releasing on
  stream-drop would over-admit past `max_concurrent`).
- **A run is named from admission, and listed while it runs.** `run_id` is minted
  in `post_research` (not by `insert_run`), streamed as the first frame
  (`started`), and registered in `backend::inflight::ResearchRegistry` — whose
  guard rides in the **same** spawned future as the permit, so the list can never
  describe a slot that is free or hide one that is not. A cancelled run is still
  never journalled; the registry, not the table, is what makes it visible. Before
  this a run had no id until it ended: nothing could list, cancel or name one while
  it ran, and with `max_concurrent = 1` an occupied slot was an unattributable
  total outage whose only remedy was a restart.
- **`GET /health` reports the slots** (`research.{slots_total, slots_busy,
  oldest_inflight_age_ms}`), and this is the one place research moves the verdict:
  a **busy** slot is never a degradation (permanent at `max_concurrent = 1`), while
  a run past `max_seconds + report_timeout_ms + inflight::WEDGE_GRACE` is. That
  same predicate is `worker::research_watchdog`'s cancel rule — one const, so
  health and the watchdog cannot disagree about "wedged". The watchdog is spawned
  **unconditionally**, unlike the metrics collector: gating a recovery mechanism on
  `[metrics].enabled` would let an observability switch decide whether the service
  can recover. It exists for the awaits that are not under a token (`/api/show`,
  Ollama's error-body read, the deliberately uncancellable journal write);
  `research_watchdog_cancels_total` is expected to stay at zero.
- **Dedicated runtime**, leaked in `main.rs` (`[research].worker_threads`);
  admission via `Arc<Semaphore>` (`max_concurrent`, published by `GET /config`) →
  429 `research.busy`, whose detail names `/research/active`.
- **Two seams** keep the loop testable: `OllamaModel` (`models/ollama.rs`) and
  `ResearchTools` (`research.rs`). Mocks: `tests/mock_ollama` (scripted via
  `POST /script`; `force_text_calls` covers `research.model_lacks_tools`),
  fakes in `research.rs` tests.
- **Native tool calling only; no text fallback.** Twelve tools
  (search/grep/symbols/outline/callers/list_files/read_chunks/file_history/
  list_research/read_research/note/revise_plan, plus `finalize`) as `tools`
  JSON Schemas (`tool_specs`), back in `message.tool_calls`; a call becomes an
  `Action` (`#[serde(tag = "action")]`) — one deserializer. A prose call is
  detected (`looks_like_tool_call_attempt`) → `research.model_lacks_tools`,
  naming the model.
- **Every announced call gets exactly one `role: "tool"` reply, in order** —
  including rejected/skipped ones (the pairing invariant; the `NativeOllama`
  fake asserts it every turn). A deadline firing mid-batch must still answer
  every announced call before breaking.
- **Prose with no tool call = `Finalized`**; `Unparseable` = empty reply or
  nonexistent tool. Duplicates rejected, not re-executed; for `search`
  "duplicate" is near-duplicate (normalized, token-set Jaccard
  `NEAR_DUPLICATE_JACCARD` 0.5, ≥ `NEAR_DUPLICATE_MIN_TOKENS` tokens) —
  deliberately also rejecting a mild refinement, naming the earlier query.
  Only *executed* searches enter `seen_queries`.
- **`read_chunks` reads the index, never the file** (pure SQL,
  `status='active'`, span overlap, `READ_CHUNKS_LIMIT` 8 × the run's
  `evidence_width`); gaps reported
  honestly ("indexed; lines N-M have no chunk"). **`path_prefix` on `search`
  is a post-filter** (`top_k * PREFIX_OVERFETCH`, then truncate), never
  appended to `include` (a union — the run could search out of its scope).
- **Bump `PROMPT_VERSION`** (`research.rs`) on any edit to `system_prompt`,
  `plan_request` (templated by the run's `max_report_sections` — the test fakes
  match it by prefix, `PLAN_REQUEST_PREFIX`), `SUFFICIENCY_REQUEST`,
  `REVALIDATION_SYSTEM_PROMPT`, `format_citation_complaint`,
  `REPORT_ROLE`/`report_system_prompt`, either report turn's user message, the
  budget nudges or `tool_specs`. Sampling (`temperature/top_p/seed`) is
  `Option` — absent = model default; a request's `seed` overrides config.
- **A report of 3+ plan items is written one section at a time.** The report used
  to be a single turn, so a model that could not produce it produced *nothing* —
  a fifteen-minute run returning zero. Now each numbered sub-question gets its
  own turn (`write_sectioned_report`), the server assembles the document under a
  derived `# heading`, and a section that fails costs that section, not the
  document. Below `MIN_SECTIONED_PLAN_ITEMS` (3, what `PLAN_REQUEST` asks for) —
  or with no plan at all — the run takes the old single-turn path byte-for-byte,
  which is the safety valve *and* the revert switch. Bounded three ways because
  it is a new turn-producing path: the run's `budget.max_report_sections`
  (effort default 6, request-overridable `3..=[research].
  max_request_report_sections`, and what the templated plan prompt asks for —
  "3-N"), `MAX_SECTION_ATTEMPTS` 2 (not `MAX_EMPTY_REPORT_RETRIES` 5 — that was
  sized for one turn, and 5×6 is thirty), plus `MIN_SECTION_MS` of window and
  `REPORT_TOKEN_OVERDRAFT` (1.5) which **stub rather than stop**. No new `DoneReason`: a failed section is a
  degradation, not a `break`. Each section turn sees its sub-question, the run's
  own sufficiency verdict on it, and the other sections' **headings only** —
  feeding back their prose would grow the prompt to compensate for shrinking the
  output. Two consequences: `validate_report_markdown` splits into
  `validate_markdown_body` (sections) + the heading check (documents), and
  **every sectioned run stores `title = NULL`**, since the heading is the
  server's and `extract_report_title` finds none — exactly what a
  repaired-heading run already does.
- **Citation repair regenerates one section, not the document.** The
  whole-document rewrite says "repeat everything that should survive" — a second
  full-volume generation of what just failed, when the run has least budget left.
  `defective_sections` maps each failing citation's offset to its section
  (`parse_citations_at` gives bytes, converted to **chars** at that one seam —
  a report is arbitrary UTF-8), and only those are rewritten, capped by
  `MAX_SECTION_REWRITES` (3). `rewrite_sections` is infallible: a report already
  exists, so a failure costs the repair, never the run.
- **Checkpoints make a stopped run return findings.**
  `[research].checkpoint_every_steps` (6, `0` = off; a request overrides it via
  `budget.checkpoint_every_steps`, `0` = off for that run, capped by
  `max_request_steps` — an interval above the step budget is `0` spelled
  differently) interrupts the tool loop to
  bank the sections already answerable into `RunState::draft_sections`, keyed by
  plan item and **replaced, never merged** (a later checkpoint saw more
  evidence). It **costs a step** *and* is capped by `MAX_CHECKPOINTS` (8):
  charging it makes it visible in the operator's budget, the cap stops a mis-set
  interval eating a run. It emits **no `step` event** — no tool call, and a
  `step` frame with no argument key breaks
  `each_action_names_its_argument_on_the_wire`; a step invisible on the wire is
  sanctioned (a rejected duplicate already is one) and pinned by a test. Payoff:
  a section that cannot be written now ships its banked version, and
  `forced_synthesis` assembles real findings instead of "No report was
  produced." Cost: ~15% of `medium`'s lookups become writing turns — **measure
  coverage on and off at the same seeds** before trusting the default.
- **Output volume, not retrieval, is where runs fail** (measured in the field;
  full record in `docs/claude/research.md`). Nothing had ever bounded the
  report: no length in any prompt, and `num_predict` sent on no turn ever.
  `[research.effort.*].max_report_words` (400/900/1800, `0` = off) is announced
  as a **ceiling** — "at most", never "about" — and arms `num_predict` at
  `REPORT_WORDS_TO_TOKENS` (4) × the grant, ~3× the prose ratio so only a
  runaway meets it: a tight cut severs a fence, fails the markdown gate, and
  buys a full-volume rewrite of what just failed. Request-overridable since the
  shape knobs shipped (`budget.max_report_words`, `0` or
  `150..=[research].max_request_report_words`) — safe because the *ceiling* is
  held to the same startup window check as the presets (`words × 4 <
  max_num_ctx_tokens / 2`), so no override can arm a `num_predict` the window
  cannot hold. The numbers are **unmeasured**; `0` is what keeps that honest.
- **The report turn's prompt is guarded too.** `context_fraction` only checks
  *between* turns against the *previous* turn's size, so the run's largest
  prompt (report turn + notes, or the rewrite with the whole draft) went
  unmeasured. `estimate_prompt_tokens` + `shed_for_report`: prior reports go
  first (hearsay, never citable), then oldest `role:"tool"` replies, each
  **replaced by a naming stub, never removed** (the pairing invariant is
  absolute); instructions/question/plan/notes/verdict/digest/request are never
  shed, and the shed is announced in the prompt. `tally.num_ctx == 0` skips the
  guard rather than substituting the configured ceiling. An **evidence digest**
  (paths + merged spans, no code) is pushed unconditionally — it is what
  `check_citations` scores against, and what makes shedding safe.
  `CHARS_PER_TOKEN_ESTIMATE` (4) is prose-derived and code tokenizes denser;
  this path may never fire on this hardware, so test it, don't measure it.
- **Citations are provenance-checked server-side** (`parse_citations` →
  `Evidence` → `CitationReport`; the `citations` event): `verified` /
  `path_only` / `unverified`; range existence deliberately unchecked; parser
  requires a file extension + relative path. The report turn writes a
  **draft** with the content gate closed; a failing draft is sent back with
  the offending *locations* (revalidation re-opens tools only when
  `reason == Finalized`, `MAX_REVALIDATION_STEPS` 4 /
  `MAX_REVALIDATION_TURNS` 3; revalidation steps don't increment `steps`); a
  failed rewrite ships the draft; draft counts ride as `draft_*`, null when no
  repair. An **ungrounded** report (`total: 0`) also trips the gate
  (`format_ungrounded_complaint`), with two load-bearing exemptions:
  `evidence.paths()` empty, or under `MIN_GROUNDED_REPORT_CHARS` (800) **from a
  budget-stopped run only** — a run that `Finalized` declared its evidence
  sufficient, so a short uncited report from it is a self-contradiction.
- **`citations.server_written` is not a nicety.** A `forced_synthesis` report
  cites nothing by construction, so `check_citations` scores it
  `total: 0, verified: 0, unverified: 0` — byte-for-byte what a clean report
  scores, in the field scout tells callers to trust. Every "verified 0 even
  though it read the files" is that collision. The flag comes from the same
  `RunTools.forced_synthesis` the journal already recorded; the fact existed
  and never reached the wire.
- **The `excerpts` event ships code, so the model never has to.** Between
  `citations` and `done`: the indexed code at every **verified** citation,
  verbatim, via `read_chunks_core` (pure SQL, **scope-enforced** — this must
  not become how a scoped run leaks refused bytes). `path_only`/`unverified`
  are excluded (no location worth reading; real bytes would dress up a refused
  claim). `MAX_EXCERPT_CITATIONS` 24 / `MAX_EXCERPT_BYTES` 256 KiB, enforced by
  dropping **whole chunks** — never cutting one, since a report and its code
  are both arbitrary UTF-8. Best-effort: a failure costs the excerpt, never the
  run. This is what makes the prompt's "do NOT reproduce code you were shown"
  honest; the two ship together. Scout returns `excerpts_available` always and
  the bytes only under `include_excerpts=True` (~100 KB is the cost scout
  exists to prevent).
- **Indexing is never blocked by research; the run reports what moved.**
  Per-file consistency holds (the prepare tx is atomic); currency:
  `probe_freshness` re-reads `baseline_sha` per shown path before every turn +
  the report turn; `changed`/`removed` sticky, `in_flight` not; staleness is
  orthogonal to provenance (`citations.stale`) and joins the revalidation
  gate. `apply_versions` takes the *asked* path list; a failed probe changes
  no verdicts.
- **Every finished run is journalled** as one flat `research_runs` row via the
  `ResearchJournal` seam (`db/research.rs`); best-effort (`warn!`, never a
  failed run); **no FK to `project_files`** (must never surface in `/drift`);
  unset sampling = NULL; `NoJournal` is `#[cfg(test)]`-gated.
- **Stored runs as context**: `context_run_ids` injects prior reports before
  the plan turn. **Prior reports are hearsay** — never seeded into `Evidence`
  (`a_prior_report_never_seeds_the_evidence`); truncated with a marker at
  `[research].max_context_chars`. Staleness is per-path via
  `research_run_files` (a global counter was rejected): the join needs
  **`model_id`**; `path` carries **no FK** (RESTRICT would brake GC, CASCADE
  would fake freshness); list filters apply **inside** the cursor-bounded
  subquery, before `LIMIT`.
- **Validity is derived, never stored**: `research_validity_ctes` — one
  recursive CTE over `context_run_ids_json` (`valid = own files unmoved AND
  every parent exists AND is valid`); dangling ids read as invalid with no
  write; cycles impossible (edges point backwards). Each summary carries
  `valid`/`invalid_reason` + `context`; invalid context in a request → 400
  `validation.research_context_invalid`.
- `list_research`/`read_research` browse the corpus (valid runs only, capped
  at `LIST_RESEARCH_LIMIT`), deliberately **unscoped**, both return
  `shown: []` (`read_research_never_seeds_the_evidence`); invalid seq =
  explicit refusal.
- **`title`** = the report's first ATX heading (`extract_report_title`,
  fallback: derived `research_title`); **`seq`** = per-project keyset cursor
  (never `OFFSET`), not identity — mutating endpoints key on the uuid `id`.
  **`expires_at IS NULL` = pinned** — the whole retention mechanism; stamped
  at insert; unpinning restores `created_at + retention`.
- **Markdown gate**: `validate_report_markdown` (four shape checks — empty,
  JSON start, no leading `#` heading, unclosed fence); a failing draft joins
  the citation complaint but re-opens **no** tools; a failing final report is
  streamed but **not journalled** (`done` carries null `run_id`/`seq`);
  `forced_synthesis` is exempt
  (`forced_synthesis_passes_the_markdown_gate`). **The missing heading is
  repaired, never refused** — `repair_missing_heading` writes
  `# {research_title(question)}` when that is the *sole* problem (a report also
  starting with JSON or leaving a fence open is still refused: a heading over
  JSON would pass the gate and remain unusable). Two sites, each **after** its
  `check_citations` — the derived heading comes from the question, which can
  itself contain a `path.rs:1-2`, and a server-written line must never enter
  the provenance report. The draft site (`research_inner`) also spares the run
  a rewrite turn; the final site (`run_research`) catches a *streamed* rewrite,
  so there — and only there — the stored report carries a line the live view
  did not show. `title` is read **before** the repair, so a repaired run stores
  none and its readers fall back to the question the heading came from.
- **Budgets**: `effort` selects `[research.effort.{low,medium,high}]`; a
  request overrides axis-by-axis (`Budget::resolve`), capped by
  `[research].max_request_{seconds,tokens,steps,report_sections,report_words}`
  + `max_evidence_width` (edge check `validate::research_budget` → 400; the
  shape axes get their own code, `validation.research_shape_out_of_range`,
  because they carry floors and two accept `0` = off; config validation
  rejects any ceiling below `effort.high`). `GET /config` publishes ladder +
  ceilings + `max_concurrent`, the context caps, a derived `worst_case_seconds`
  per level (`max_seconds + report_timeout_ms`, since the two bound *different
  phases* and reading the first as the whole wait understates `high` by five
  minutes) and `observed` — measured p50/p90 per `(model, effort)` from
  `research_runs` (`worker::research_stats`, model-catalog tick and its
  keep-the-last-snapshot rule; a pair under `MIN_RUNS_FOR_ESTIMATE` is absent
  rather than noisy). The ladder says what a level *grants*; `observed` is the only
  thing that says what it *takes*, which is what makes `effort` priceable before
  the fact. The axes:
  - **`max_seconds`** (300/900/3600) is a HARD deadline — poll **and**
    `DeadlineToken` (child of the job token; `stopped_by` tells it from a
    disconnect, job token tested first). A deadline stop is not a failure.
  - **Report window** `[research].report_timeout_ms` (120 s), token a child
    of the **job** token (never the budget one); expires with a draft → ship
    it; with nothing → `forced_synthesis`. A truncated run names its limit in
    the report; the sufficiency turn is skipped.
  - **`turn_timeout_ms` must sit ABOVE every budget** — startup-enforced
    (`validate` refuses `<= max_request_seconds`); it is a dead-socket guard,
    not a bound. Its blind spot is a socket that is **alive and mute**:
    `[research].first_token_timeout_ms` (120 s, `0` = off) abandons a turn that
    produced no token of any channel, as `OllamaError::Silent` → a named
    `ollama.unavailable` failure instead of a whole budget spent waiting on an
    Ollama that is (re)loading a model. It bounds the **silent prefix only** —
    armed across `post_chat` *and* the wait for the first delta (the stall can
    be in either; Ollama holds the connection open while it loads), spent by
    the first thinking/content/tool-call delta. Startup keeps it strictly under
    `turn_timeout_ms` (above it, it could never fire) and at/above 5 s (below,
    it preempts a merely long prompt evaluation).
  - **Runaway-thinking guard** `[research].max_turn_thinking_chars` (8192,
    `0` = off): abandons the turn as an **empty** `ChatOutcome` (every phase
    already recovers from one); instrumented in place
    (`research_runaway_thinking_turns` + `warn!`); `TokenTally::record` must
    not let its zero `num_ctx` overwrite a known window; its GPU cost lands
    in `turns_unreported`.
  - **`max_tokens`** (400k/1.2M/6M) is the cost axis (`prompt_eval + eval`;
    transcript resent every turn → super-linear in turns).
  - **`context_fraction`** (0.5/0.7/0.85): a guard against Ollama's silent
    transcript trim, checked against `tally.peak_prompt_tokens` *before* the
    next turn; not request-overridable, with `search_top_k` — the two axes a
    request cannot touch.
  - **`max_steps`** (8/20/64): coarse backstop (a step is a poor unit).
  - **`search_top_k`** (5 at every level) is width, not budget; config
    validation refuses `> [search].max_top_k` at **startup** (`search_core`
    leaves validation to callers). Deliberately still TOML-only: widening it
    was measured not to fix the failures it looks like it would.
  - **Shape axes** (request-overridable, resolved like the rest by
    `Budget::resolve` except `checkpoint_every_steps`, which the handler
    resolves into `ResearchParams` — it is a `[research]` scalar, not an
    effort axis): `max_report_sections` (6 at every level, `3..=12`; floor =
    `MIN_SECTIONED_PLAN_ITEMS`, kept as `config::MIN_REPORT_SECTIONS` so the
    validators share it), `max_report_words` (see the output-volume bullet),
    `checkpoint_every_steps` (see the checkpoints bullet) and
    `evidence_width` (1 at every level, `1..=[research].max_evidence_width`) —
    an integer multiplier on `READ_CHUNKS_LIMIT`/`GREP_LIMIT`/`CALLERS_LIMIT`/
    `FILE_HISTORY_LIMIT`/`SYMBOLS_LIMIT`, threaded as a `limit` param into the
    research-only core fns and stored on `StateResearchTools` (constant per
    run, so the `ResearchTools` trait is untouched). It deliberately does
    **not** scale `outline`/`list_files` (navigation — when 300 rows bind the
    fix is a narrower glob), `search` (its own axis), or `MAX_EXCERPT_*`
    (response caps). Width is resent every turn — it compounds into
    `max_tokens`. None of the shape grants are journalled (shape knobs never
    were); the resolved values ride on the "Starting a research job." log
    line, the only record of what a run was actually granted.

  Each stop has a loop-level test; the time one uses **real** `Instant` in
  small increments (`tokio::test(start_paused)` does not move it).
- **`progress`**: `RunProgress` (spent vs granted per axis + `turns` +
  `binding` + `shares`) emitted before the first turn, after every step and turn;
  `done` carries it + `reason`. **No ticker** (would race the cancellation token
  and make tests clock-dependent). `binding` names the axis with the largest
  **share spent** — a maximum, not a warning, and not what stopped the run
  (`done.reason` is); its fold seeds at `Time`, so an all-zero progress reports
  `"time"`. It was read as "about to run out" for its whole life, so `shares` (the
  four percentages it is chosen from) now ships beside it and scout promotes both.
- **A step reports where it landed** (`spans`, `path:start-end`, from the same
  `shown` locations citation provenance is scored against; capped with
  `spans_truncated`). `hits: 3` on a 4000-line file names no lines, which made the
  trace unusable for the only thing it is for.
- **The identifier rule governs code; documentation inverts it.** `*.md` is
  indexed, and `system_prompt`'s identifier paragraph carries the exception
  (*documentation is written in English; ask it in English*) — the corpus
  half and the prompt half must never ship apart. Any future prose channel
  inherits this.
- **`outline`/`list_files`**: pure SQL (`outline_core`/`list_files_core`,
  `idx_project_file_symbols_file`); intended path `list_files → outline →
  symbols/search/callers → read_chunks` (the prompt says so — half the
  feature). `outline` reports `indexed` separately from an empty symbol list.
  `list_files`' glob is SQLite `GLOB` (`*` crosses `/`, unlike `.mindex`).
  Post-stream errors are `error` events; `NoMatch` is a tool result.
- **Scope is enforced on every tool**: `ResearchTools` takes a `ToolScope` as
  a required argument on every model-facing method (a later tool cannot be
  the next exception). Evaluated in SQLite by `build_file_filter`, appended
  as a `file_path IN (SELECT …)` subquery, not a join. Path-keyed tools
  (`outline`, `read_chunks`): explicit refusal (`in_scope` flag);
  name/text-keyed (`symbols`, `callers`, `grep`): rows dropped **and
  counted** against one unscoped `COUNT(*)`. All gated on `is_scoped()` —
  unscoped runs build byte-for-byte the old SQL (public `/symbols` provably
  unaffected). `SymbolsRequest.include/exclude` binds append **last**.
  `file_versions` is unfiltered (asks only about shown paths). The scope is
  *told* to the model (one `ToolScope::describe` feeds prompt + state note).
- **`note`/`revise_plan`** mutate the run (`apply_local`, bypass
  `ResearchTools`), each costs a step; notes pinned into the state note every
  turn + pushed before the report turn; caps announced, never silent
  (`MAX_NOTES` 24, `MAX_NOTE_CHARS` 500). **`grep`** is a case-insensitive
  `LIKE` over `project_file_chunks.code` (`grep_core`); **`like_escape` is
  mandatory** (`_` is a wildcard); reports line + chunk span; bounded by the
  scope subquery, `GREP_LIMIT`, `GREP_MIN_PATTERN_CHARS`. FTS5 deferred. **An
  empty result has three meanings, not one** — out-of-scope, nothing
  searchable, genuinely absent — so `GrepResponse` carries
  `searched_chunks`/`searched_files` (one extra `COUNT` over the same scope
  subquery, read **only on a miss**, the `out_of_scope` probe's rule) and
  `format_grep`'s empty branch is three-way. A glob matching no file used to
  read as proof of absence, which is how one run honestly reports 0 hits for a
  literal the next finds 5 times.
- **`callers` is deliberately approximate** (no target column;
  `parent_name` by byte-span containment; `direction: "out"` via
  `idx_project_file_symbols_parent`); the collision/alias caveat is repeated
  **on every result**; empty answers distinguish "never referenced" from "no
  such name" (two reads); parent-less references reported, not dropped.
  LSP/SCIP was rejected; `symbols_cross_language_tests.rs` pins the
  span-containment property across five languages, and its allow-list forces
  a decision per new tagged language.
- **The loop terminates on counters, not a clock** (regression guard): every
  iteration breaks or increments exactly one of `steps`, `parse_retries`
  (≤ `MAX_PARSE_RETRIES`), `duplicate_calls` (≤ `MAX_DUPLICATE_CALLS`); one
  level up, `reopens ≤ MAX_REOPENS` and the revalidation caps. A new
  rejection path needs a new bounded counter — or price the refusal as a
  **step** (what every refusal added since does).
- **A plan turn opens the run, a sufficiency turn closes it** (both
  `NO_TOOLS`) — the thinking channel is discarded from the transcript
  (`ChatMessage` has no `thinking` field), so the plan is pushed back as an
  **assistant** message; degrades to a plan-less run. Sufficiency re-opens
  only on model-chosen stop + an unspent axis + `declares_unanswered`
  (server-dictated vocabulary); the re-open nudge is the one place
  `revise_plan` is offered by name.
- **The run-state note is pinned, not appended** (`RunState` →
  `format_state_note`): one `user` message rebuilt and re-pushed before every
  turn, placed after the previous turn's `role: "tool"` replies.
- **`num_ctx`** = `min(model limit from /api/show, [research].
  max_num_ctx_tokens)` — a VRAM ceiling (default 131072), not a window; an
  unreachable `/api/show` degrades to the ceiling, never zero.
- **Model catalog** (`worker::ollama_catalog` → `research.models` in
  `GET /config`, refresh `models_refresh_interval_seconds` 300): a failed
  tick keeps the previous list (`refreshed_at` not re-stamped — the only
  "no models" vs "never reached" signal); gated on nothing; never primed at
  startup; `health()` is a provided method over `list_models`; the
  `/api/tags` reader is `#[serde(default)]` throughout.
- **`[research].allowed_models`** (glob whitelist, compiled once at startup;
  empty = any): the resolved model is checked in `post_research` *before* the
  semaphore → 400 `research.model_not_allowed`; `GET /config` publishes
  `research.models` already filtered by it plus the raw patterns; a non-empty
  list must cover a non-empty `default_model` — startup refuses otherwise
  (every defaulted request would 400). Match is case-sensitive and includes
  the tag: `"gemma4:*"` does not cover bare `"gemma4"`.
- **Ollama errors**: `chat_stream` reads the error body (never
  `error_for_status`); a 500 containing `error parsing tool call` is resent
  with the same transcript at the next seed (`MAX_TOOL_CALL_PARSE_RETRIES`;
  safe because the 500 precedes the stream); anything else fails with
  Ollama's own words. Token counts are `Option` → `turns_unreported`; the
  WARN when `prompt_tokens` reaches `num_ctx_tokens` is the *only* symptom of
  a silently truncated transcript.
- **`Step` carries a typed `StepCall`** (each action names its own key on the
  wire). **SSE contract lives in four places that move together**:
  `post_research`'s doc comment, its `#[utoipa::path]` 200 description, the
  VS Code client (`api.ts` + `researchView.ts`), scout's reader (whitelists
  silently drop unknowns). One `data:` line per frame
  (`serde_json::to_string` escapes newlines) — keep it that way. Pinned by
  `progress_wire_fields_are_stable`,
  `done_event_carries_the_reason_and_the_run_cost_on_the_wire`,
  `done_names_no_run_when_the_journal_write_failed`,
  `each_action_names_its_argument_on_the_wire`,
  `citations_wire_fields_are_stable`,
  `a_server_written_report_says_so_on_the_wire`. `done` carries nullable
  `run_id`/`seq` (null when the journal write failed; rendered as "not
  saved" in VS Code). `started` is always the **first** frame; event order after
  the report is fixed: `summary` → `citations` → `excerpts` (only with a verified
  citation) → `done`.
- **A stream ending without a terminal event is a failure**:
  `SseEventStream` synthesises one `error` (`internal.error`) when the
  channel closes without `done`/`error` (a detached-job panic otherwise reads
  as a completed stream); `SseWireEvent` is generic, so streaming `/index`
  gets it free; `a_stream_that_ended_properly_gets_no_synthetic_terminal`.
- **A report is arbitrary UTF-8 — never index it by byte**
  (`a_report_is_arbitrary_utf8_and_must_never_panic_the_parser`); the only
  safe indexes into model output are `char_indices` or ASCII-derived
  positions.
- **The report turn passes no tools** (field *omitted*) + swaps in
  `REPORT_SYSTEM_PROMPT`. The content gate in `chat_turn` withholds
  `{`-replies (`is_withheld` makes the re-ask safe); a second one →
  `research.no_report`. One `write_report` for draft and rewrite
  (`ReportOutcome`: `Written`/`Empty`/`ToolCall`).
- **`done.reason`** (`DoneReason`): `finalized` / `time_exhausted` /
  `tokens_exhausted` / `budget_exhausted` / `context_exhausted` /
  `unparseable` / `repeated_calls` — one per `break`; wire contract
  (`done_reason_wire_values_are_stable`); adding a `break` means adding a
  variant.

## Git history channel

`project_commits` + `project_commit_paths` say **why** the code became what it
is. Opt-in (`mindex-index --history`), metadata-only: **no embeddings, no
Qdrant, no chunks, no derivation version**; one model-facing tool,
`file_history`. Commit metadata is the whole feature (history questions are
SQL questions); semantic search over messages is deliberately excluded — the
ladder, its cost and the rationale live in **`docs/claude/git-history.md`**;
read it before touching `tools/indexer/src/git.rs` or `/history`. Hard
invariants:

- **Not pseudo-files**: own tables keep commits out of `/drift` and out of
  `build_search_query`'s candidate set by construction (pinned:
  `commit_rows_are_invisible_to_drift`,
  `test_commit_paths_never_surface_in_drift`).
- **`project_commit_paths.path` has no FK** (RESTRICT would refuse inserts,
  CASCADE would erase history on GC); the join to the code channel is soft,
  and `file_history` must report an un-indexed path as such.
- **Hard delete, no GC** (the `project_file_symbols` lifecycle; inverts if
  messages ever gain vectors).
- **`POST /v0/{guid}/history` is a full-set replace within `since`** (no
  update path — a sha is the hash of its content); `since` bounds only the
  **deletion** half and is load-bearing (without it a windowed walk wipes
  older history every pass). The posted set goes through a temp table, never
  `NOT IN (?, …)`.
- **Retention is `DELETE /v0/{guid}/history`**, operator-facing, called by no
  client: `keep_last=N` (newest by `committed_at`, `sha DESC` tie-break) and
  `older_than=<unix>` **intersect** — a commit dies only if both condemn it;
  naming neither = 400 `validation.history_bound_missing`. Destructive but
  not lossy (the repo refills it).
- **One producer: `mindex-index`** — rule 10 does not fire; do not teach the
  watcher/extension/MCP to walk git. `--history-only` runs the phase without
  enabling the channel (what the post-commit hook relies on). Missing
  git/non-repo = WARN, never a failed run.
- **`--relative` is not optional** below the repo root (else the soft join is
  empty for every file; pinned by
  `the_walk_asks_git_for_root_relative_paths`). The four `git log
  --format --raw -M -z` parsing traps (derived subject, `-z` + `%x1e`/`%x1f`,
  status-letter arity, the leading `"\n:"` header) are each pinned by a test
  in `tools/indexer/src/git.rs`; `old_path`'s biconditional validation turns
  a mis-parsed stream into a 400.
- **Four client-side drops, all announced** (age+count bounds together, short
  messages, truly-auto merge commits — the conjunction spares squash-merges —
  and all-paths-out-of-scope); an over-cap message is truncated with a
  marker, never dropped.
- **`file_history` reports three flags** (`history_indexed` / `in_scope` /
  `path_indexed`) — an empty list has three meanings. Out of scope is an
  explicit refusal. Its `shown` evidence is only the asked path, span-less.
  **No commit citation grammar**: a sha is verified by `git show`; claims
  anchor to `path:start-end` with the sha in prose (a shas-only report parses
  to `total: 0` and trips the ungrounded gate).

## Retrieval pipeline

Three named vectors per collection: `dense` (1024-d cosine), `sparse`
(SPLADE-style), `colbert` (1024-d, multivector MaxSim). Search: prefetch
top-200 dense + top-200 sparse → RRF fusion → ColBERT rerank → top-k.
`post_search` runs **two** SQLite queries around Qdrant — candidate
`qdrant_guid`s first, then `code`/metadata for *only* the top-k winners; never
load `code` for the whole active set. Results are **sorted by score
descending** before responding (don't rely on Qdrant's order). Sparse weights
≤ 1e-5 are dropped before upsert. Batch sizes: `--embed-batch` chunks per
`/encode` (default 256, the GPU-load lever), 256 points per Qdrant
upsert/delete (`embed.rs`). Embed-response vectors are positionally aligned
with the chunk list.

**The query path may run on a second embedder instance.**
`[model].query_server_url` (absent = one instance does both; `RouterState`
holds the *same* `Arc` twice) puts `/search` and every research search on its
own BGE-M3 — typically `--device cpu` (a query is one ~20-token text,
latency-bound), freeing the ~6 GiB of VRAM the resident fp32 model otherwise
holds. Both instances must be the same model at the same precision and
**nothing checks that they are**: reduced precision on one side flips
low-weight token ids in and out of the sparse set, presenting as "search
sometimes can't find the obvious thing", not as an error. `GET /health` pings
the second one separately (`checks.query_embedder`) — only when actually
split, hence an `Option` compared by `Arc::ptr_eq`, not by URL.

The embedder client (`bge_m3.rs`) retries HTTP **429** up to 3× (200/400/800
ms, respecting the cancellation token in sleeps), then gives up — the file
goes `failed` and the retry worker re-attempts later (layered backoff). Each
`/encode` attempt has a whole-request timeout (`[model].encode_timeout_ms`,
default 10 min) so a wedged embedder can't hang the retry worker.

## Slicer

`Slicer` (`slicing/traits.rs`) walks the tree-sitter AST depth-first,
selecting **named nodes** whose token span (HF tokenizer) is **128–512
tokens** (BGE-M3's sweet spot; measured, not computed — token boundaries don't
align with AST nodes and tokenization is context-dependent). `code` is
extended left to line start over pure indentation, then over the node's **doc
comment and attributes** (`ABSORBED_KINDS`, matched as substrings — no
per-language table). Not cosmetic: a doc comment is a *preceding sibling*,
never a child, so without the extension the prose that says **why** is dropped
from every chunk — the actual cause of the "an NL query retrieves the test,
not the implementation" finding. Absorption stops at a blank line (a detached
comment documents nothing in particular), at `max_tokens`, and at the furthest
byte already emitted (a large `#[utoipa::path(...)]` clears `min_tokens`
alone, becomes a chunk, *and* is the preceding sibling of the function below —
without the bound both chunks would contain it).

**Node selection alone leaves ~37% of lines in no chunk**, so the walk is
followed by a **gap pass** (`[slicer].fill_gaps`, default on): everything
inside a node below `min_tokens` (consts, type aliases, small helpers, trait
signatures — and their doc comments, which left-extension can never reach when
there is no chunk to extend) plus everything between the selected children of
an oversized node, packed into line-aligned windows up to `max_tokens`,
breaking at blank lines. Fragments under `GAP_MIN_TOKENS` (24) merge into the
previous window, not dropped. Measured here: line coverage 63% → **99.7%**,
doc-comment coverage 40% → **100%**, chunks 553 → 972. Roughly doubles
embedding work — hence the knob. Beyond coverage: only 47% of symbol
definitions were inside any chunk, so `read_chunks` dead-ended on a coin flip
— the prescribed pipeline failed at its last hop.

`SlicedChunk.start_byte/end_byte` are `#[cfg(test)]`-gated (never persisted),
as is `from_gap` — **the token window governs node selection, not gap chunks**
(a gap chunk's floor is `GAP_MIN_TOKENS`). The window is counted over
**whole-file** token offsets, so re-encoding a chunk alone is a different
measurement (an edge token splits differently without its surroundings; 512
can re-encode at 513). `chunks_satisfy_token_window` therefore asserts
128–512 ±`WINDOW_SLACK` — without the slack the test is a tripwire on
whichever file lands on a boundary (`src/research.rs` did).

**A line is not bounded by anything, so neither pass may cut only at line
boundaries.** A minified file (or one paragraph of prose) is one line for its
whole length — and what came out was an *unstorable* chunk: a Qdrant
multivector point holds ≤ 1 048 576 elements, ColBERT emits one 1024-wide row
per token, the embedder adds `[CLS]`/`[SEP]`, so anything above
`STORABLE_TOKENS_CEILING` (1022 tokens) is **refused — failing the whole
upsert batch** (a huge one first exhausts embedder GPU memory). Hence
`token_boundary` (`slicing/traits.rs`), the last resort of both passes: cut on
a boundary the tokenizer itself reported. Two easy mistakes: the ceiling is
`min`-clamped **in both constructors**, not config-validated, because
`[slicer].max_doc_chunk_tokens` **defaults to 1024** — over the ceiling before
a single chunk is cut, and rejecting the default at startup would refuse a
config the operator never chose; and a slicer must aim `RETOKENIZATION_SLACK`
*under* the ceiling (a cut measured at 1022 re-encodes at 1023).
Documentation blocks are truncated to the same ceiling before being embedded
for the semantic term (a block is not a chunk and has no size bound).

**Documentation is chunked by a second slicer, and every rule above
inverts.** `MarkdownSlicer` (`slicing/markdown.rs`, `markdown` only) walks
tree-sitter-md's **block** grammar to atomic blocks — descending into
`list_item` — then packs runs of adjacent blocks into chunks by dynamic
programming (`best[j] = min_i best[i] + cost(i..j)`, exact, O(n²)): one cost
term per chunk against a penalty for swallowing a level-3+ heading, `+∞` above
`[slicer].max_doc_chunk_tokens` and across any level-1/2 heading (greedy "fill
to the cap" buries subsection headings to save chunks it did not need to
save). Three inversions, each measured: **no lower bound** (a 40-token section
is a complete claim), **chunks nest and merge**, and **the cap is 1024, not
512** (512 answers 15/23 documentation questions vs 18/23 — it cuts
explanations away from what they explain). `MODEL_MAX_TOKENS` (512) is the
*code* window's quality ceiling, not the model's capacity (the embedder
truncates at its `--maxlen`, default 8192).

**Boundaries come from two signals; the second refines the first.** Structure
sets the hard rules; *semantic shift* (embedding distance between blocks,
weighted `[slicer].doc_semantic_weight`) decides among what structure leaves
open. Measured on **this** repo the term changes nothing (moves 7-13% of
boundaries, MRR@10 0.3931 either way) — a fact about this densely-headed
corpus, not the technique; it is on by default because it is worth most where
headings are sparse and the packing would otherwise cut mid-topic. (A further
model-driven boundary re-check pass measured *worse*; not shipped.) Separately
real: block structure beats a line-based `#` splitter, MRR@10 0.3714 →
0.3931, recall@10 18/23 → 20/23. Three consequences of the term being on: it
costs **one `/encode` per document**, so block embedding happens *outside*
the prepare transaction — hence the two-phase `plan` → `segment` API; an
**unreachable embedder degrades to structure-only** with a WARN rather than
failing the file (a refinement must never be a dependency); and chunk
boundaries now depend on the **embedder's model and precision**, which
`CHUNKS_DERIVATION_VERSION` cannot see — the same blind spot as a
grammar-crate bump. Weight 0 restores pure structure and skips the
round-trip.

## Concurrency & cancellation

- **Async-first.** SQLite runs in `spawn_blocking` via
  `db_pool.transaction()`; Qdrant/embed are `.await`-ed — no `block_on`. Every
  long loop / I/O respects a `CancellationToken`; client-cancelled requests
  return HTTP 499.
- **Cancellation propagation (subtle).** A handler's `CancellationGuard`
  wraps a *fresh* token cancelled only by its own `Drop`. On client disconnect
  axum drops the handler future → `Drop` fires, but the future is gone, so
  in-handler `Cancelled` arms are defensive, rarely hit. The token's real job
  is letting in-flight `spawn_blocking` (slicer) and the embed `select!` bail
  after abandonment; the half-written row is recovered by the retry worker.
  Clean shutdown uses a *separate* token tree rooted in `main.rs`.
- **`IndexClaim` is an in-process keyed lock**, so mindex assumes **one
  process per database**. It serializes handler↔handler and handler↔worker
  races on a file (contention → 429) within one process only; two processes
  against one SQLite/Qdrant would need a DB-level compare-and-swap claim (a
  conditional `… → indexing` update + an epoch column checked at
  `mark_indexed`) — a schema migration, hence not done speculatively.
- **Connection-return is cancellation-safe** (regression guard,
  `sqlite3.rs`). The blocking task pushes its connection back into the pool
  *itself* (`conns.blocking_lock().push`), not the awaiting code after
  `handle.await`: dropping a `spawn_blocking` JoinHandle does **not** cancel
  the task, and if release depended on the awaiting future, a future dropped
  mid-transaction would leak the conn — after `db_pool_size` (4) such events
  the pool is permanently `PoolEmpty`. A closure panic is the one unreturned
  case (logged on `JoinError`).

## SQLite pool

Fixed-size pool of `rusqlite::Connection` behind a
`tokio::sync::Mutex<Vec<_>>` (pop/push). Per-connection PRAGMAs: WAL,
`foreign_keys=ON`, `synchronous=NORMAL`, 16 KB pages. Handlers run **multiple
sequential `transaction()` calls** (one per logical step), not one giant
transaction — the soft-delete pattern keeps state recoverable if a later step
fails.

## post_index shape

Two phases so the GPU sees big batches: (1) **`prepare` every file** —
hash-check (`Ok(None)` = unchanged, skipped) → set `indexing` → main tx (mark
old chunks deleted + slice + insert) → `Prepared` with that file's chunks; own
`indexing_file` span each (no `Entered` guard across `.await`). (2)
**`embed_all`** chunks from all prepared files in one batched
`embed::embed_and_upsert` pass, then **`mark_indexed`** each + tally. Recovery
is per-batch: any failure sends every already-prepared file to
`failed`/`cancelled` via `recover_all`; the retry worker re-embeds later.
`tree_sitter::Parser` is `Send` — slicer built inside the `spawn_blocking`
closure. Body limit: `[server].max_body_mib` (default 256 MiB) via
`DefaultBodyLimit` (axum's 2 MB default is far too small); over-cap =
problem+json 413 (`request.body_too_large`), not axum's plain-text.

**`?stream=yes` reports the same pipeline as SSE**, and the pipeline is one
function either way: `run_index_job` is shared verbatim; the query only picks
who builds the terminal (`Json` body vs a `done`/`error` event), so the modes
cannot drift. The cancellation shapes differ on purpose, mirroring research's:
JSON mode keeps its `CancellationGuard` (handler-future drop = cancel); SSE
mode spawns the job detached and the *stream's* Drop cancels the token (a
guard in the handler would fire the instant the response is constructed).
Recovery runs inside the job, so a disconnected streaming client still lands
its batch in `cancelled`. The event vocabulary
(`started`/`prepared`/`skipped`/`embedded`/`indexed`/`done`/`error`,
`IndexEvent` in `models.rs`) is a wire contract in four places that move
together: `post_index`'s doc comment, its OpenAPI 200 description, the
`mindex-index` reader (`tools/indexer/src/client.rs`) and the VS Code client
(`api.ts`); both consumers drop unknown events silently; shapes pinned by
`index_event_names_are_stable` +
`index_event_data_names_its_fields_on_the_wire`. `embedded` (one per embed
batch, via `embed_and_upsert`'s optional progress callback — its one
deliberate side-channel) carries cumulative `chunks_done`/`chunks_total` plus
the server's own `elapsed_ms`, making a client's chunks-per-second a
measurement; both clients compute it over a sliding window and fall back to
plain JSON transparently when an older server ignores the query
(`StreamOutcome.streamed` / content-type sniff). `done.files` is byte-for-byte
the JSON response body, so both modes tally identically. A typo'd `?stream=`
value or key is a 400 (`IndexQuery` is `deny_unknown_fields`), never a silent
fall-through.

## Mockable interfaces

Three traits; production type is the sole real impl, fakes in `#[cfg(test)]`:
**`BGEm3Model`** (embedder, `Arc<dyn>` in `RouterState` + retry worker),
**`VectorStore`** (all Qdrant ops; error is `VectorStoreError`, a rendered
string — `QdrantError` isn't test-constructible), **`Tokenizing`** (the
slicer's only tokenizer need; fakes avoid the HF download). New seam = minimal
trait + owned error if the real one isn't constructible. `SQLite3Pool` is
deliberately **not** a trait (its generic-closure `transaction` isn't
object-safe) — test against a real `:memory:` pool.

## Error handling, validation & logging

**Client error contract: `ApiError` → RFC 7807 (`backend/error.rs`).** Every
non-2xx is `application/problem+json` (`ProblemDetails`) with a **stable,
namespaced machine `code`** (`validation.top_k_out_of_range`,
`selector.empty`, …) + English `title`/`detail` + optional `field`/`meta`; the
`code` is the localization key. `ApiError` is the *single* enum; its
`code()/status()/title()/detail()/meta()` and the lone `IntoResponse` impl are
the only place a response shape is built. **Codes are an API contract**: the
`codes_are_stable` snapshot test fails on any change (also update the
catalogue in `openapi.rs` `info.description` + clients). Handlers return
`Result<_, ApiError>`; domain errors (`SQLite3PoolError`, `SlicerError`,
`EncodeError`, `VectorStoreError`, `EmbedUpsertError`) convert via
`From`/constructors at the call site — the call site keeps the contextual log
+ sysadmin hint, `From` never logs. Mappings: `SQLite3PoolError::Cancelled` →
499 (rest `Internal`); embed request/decode → `EmbedderUnavailable` 503;
Qdrant search → `QdrantUnavailable`, upsert/drop → `Internal`. No external
error crates.

**Validation happens at the edge (`backend/v0/validate.rs`), before any
work** — bad input is a 400 with a precise `code`, never an opaque 500 from a
SQLite `CHECK`. It mirrors the schema constraints (`validate_path` = the path
CHECK + `..`-traversal guard; `validate_sha256_hex` = 64 hex) and enforces the
`[limits]`/`search.max_*` caps; `require_nonempty_selector` is the shared
guard for the destructive endpoints. Schema CHECKs and shape-validation
triggers stay as defense-in-depth. Handlers take `ApiJson`/`ApiPath`/`ApiQuery`
(`extract.rs`), not bare axum extractors, so malformed body/path/query is the
same problem+json envelope (`request.malformed_body`/`malformed_path`), not
axum's plain-text 400.

- No `unwrap`/`expect` in production paths (workers may `unwrap_or_default` on
  best-effort queries); startup-only panics name the file and what to check.
- **Logging shape:** a mandatory message stating *what operation failed*
  (never bare `error!(?err)`); error as a field (`error = ?e`/`%e`, not
  interpolated); identifiers as fields (`%` String/Uuid, `?` enums). Handlers
  carry `project_guid`/`pl`/`path` on the span; workers pass them explicitly.
  Infra failures end with a one-line sysadmin hint (embedder reachability +
  the `0.0.0.0` vs `127.0.0.1` gotcha, Qdrant, DB writability); logic errors
  don't.

## Metrics

`GET /metrics`, OpenMetrics text, same HTTPS listener — `prometheus-client`
with one owned `Registry` threaded as `Arc<Metrics>` through constructors
(`backend/metrics.rs`). No global recorder (the config rule; also the only
shape letting two unit tests each own an independent metric set). Scraped by
host VictoriaMetrics (`scheme: https`, no `tls_config` — the mkcert root is in
the system trust store, the only path that works since the VM unit sets
`ProtectHome=true`); rendered by the provisioned Grafana dashboard in
`deploy/grafana/`.

**Metric names and types are a contract, like `ApiError` codes** — a
dashboard is a client, a renamed family is a silently blank panel.
`metric_names_are_stable` pins *both*; the type matters more (a counter→gauge
flip renames nothing and breaks every `rate()`). Two encoding quirks the test
encodes: OpenMetrics puts `_total` on a counter's **sample** lines, not its
`# TYPE` line (family `mindex_gc_runs`, stored series `mindex_gc_runs_total`
— the test reconstructs and pins the series name), and `encode` emits `# EOF`,
so the body must be served as `metrics::CONTENT_TYPE`, never `text/plain`.

**The cardinality rule: every label value comes from a set the server
defines** — `MatchedPath`, `ApiError::code()`, `ProgrammingLanguage::name()`,
`DoneReason::as_str()`, tool names, file statuses. Never a raw URI, path,
query, or model-supplied string without a bound. `project_guid` is the sole
open-ended label: UUID-validated first, off by default on the HTTP families
(`[metrics].per_project_http_labels`). Two products are **split rather than
crossed** — `research_runs{model,done_reason}` vs
`research_runs_by_effort{model,effort}` (`model` is client-supplied), and
`project_chunks_active{project,language}` vs
`project_chunks_deleted{project}`. Histograms are not labelled by project at
all.

**Clear-and-repopulate, and why counters are exempt.** A `Family` retains a
label set for the life of the process (a deleted project would report its last
known count until restart). `worker/metrics.rs` builds each tick's whole value
map from SQL *first*, then clears and repopulates in one synchronous block
**with no `.await` between them** — the only thing keeping a scrape out of the
gap, so `apply` must never become `async`. Two structural guards: only
`StateMetrics` is ever cleared, and it holds **gauges only** — clearing a
counter reads as a process restart and permanently re-baselines every
`rate()`. `StateMetrics` is written by that worker and nothing else.

**Why decorators, and the four things that cannot be one.** `VectorStore`,
`BGEm3Model`, `ResearchTools` and `ResearchJournal` are wrapped once in
`main.rs`/`post_research`: a seam decorator cannot miss a caller;
`MeteredJournal` alone yields nearly the whole research set (`RunRecord`
already carries it). The exceptions, each structural: `SQLite3Pool` is not a
trait, so it is instrumented in place at its single choke point; the
embedder's 429 retry loop lives inside `encode` (invisible from outside);
Ollama's tool-call-parse retry and silent transcript truncation happen inside
one `chat_stream` call; and the indexing-claim conflict is swallowed at
`Err(ApiError::FileInFlight) => {}` while the request still 200s, so HTTP
middleware can never see it. All four use an `Option<...>` field set by a
`with_metrics` builder — keeping test pools and bare test clients
constructing unchanged.

**In-flight gauges are `Drop` guards** — a disconnected client's future is
*dropped*, so code after `next.run(req).await` never runs; research SSE
streams die **only** by disconnect, so an inc/dec pair would ratchet the gauge
upward within a day. The guard also records the abandonment as `status=499,
code="request.cancelled"`, keeping `http_requests_total` reconcilable with
`http_requests_in_flight`. Same logic: `research_active` is **derived** in the
collector from `max_concurrent - available_permits()`, not incremented around
the spawn.

**`enabled` means exposed, not measured.** `Arc<Metrics>` is always built and
written into; `[metrics].enabled` gates only the route and the collector (the
alternative is an `Option` check at sixty call sites for a relaxed atomic
add). Two edge consequences: the HTTP layer sits **outermost** and must be a
`Router::layer` (outside the router there is no `MatchedPath`) — so a request
matching no route never reaches it and unknown-path 404s are uncounted rather
than bucketed under a fabricated label; and the HTTP/3 body-limit
short-circuit answers before the router, so it records itself — a second such
short-circuit needs the same three lines or it goes uncounted in silence.

**A rare labelled counter must be charted with `increase()`, never `rate()`**
— the rule that once made the whole research dashboard row read empty. A
`Family` series does not exist until its first event, so its first scraped
sample is already **1** — no preceding 0 — and a label set seeing one event
per process lifetime (normal for `research_runs{model,done_reason}` and every
per-run histogram) stays flat at 1: `rate()` over that is 0 forever.
`increase()` counts the first sample of a newly-appeared counter
(VictoriaMetrics does; upstream Prometheus does not — and seeding at zero is
impossible here, `model` is client-supplied). High-traffic families hide the
defect. Paired rendering rules: a handful of events a day is drawn as **bars**
with a `sum` legend; a quantile over a rare histogram as **points with gaps
kept**; a share-of-total stat needs `or vector(0)` (the healthy case is that
the `unverified` series does not exist).

**Three families exist to answer one open question**: does announcing a length
ceiling change what a model writes? `research_report_words{model}` against the
granted `max_report_words` is the measurement — if they are uncorrelated, the
prompt half of that knob is dead weight and only `num_predict` earns its place.
`research_report_length_caps` is expected to stay at **zero** (any value means
`REPORT_WORDS_TO_TOKENS` or the model is wrong, and a cut landed mid-token);
`research_report_context_sheds` may legitimately stay at zero forever on this
hardware, in which case the shed path is insurance and should be described as
such rather than as a mechanism.

**What is deliberately not measured.** Per-project code bytes as a gauge
(`SUM(LENGTH(code))` full-scans the biggest column every tick and evicts the
page cache the candidate query depends on — the write-time
`index_code_bytes_total` counter answers it better). A drift *level* (the
server never walks a tree; `post_drift` counts what checks *reported*; a real
drift gauge belongs to `mindex-watch`). Tokio runtime internals (need
`--cfg tokio_unstable`) — "threads and pools" is covered by the SQLite pool,
the claim table, the research semaphore, in-flight requests and the research
runtime's worker count.

`/metrics` is the one route with no `#[utoipa::path]` and no `openapi.rs`
entry — not JSON, not versioned, not problem+json; its consumer is a scraper.
`openapi_spec_is_complete_and_versioned` asserts the *absence* so the omission
reads as a decision.

## Performance conventions (hot paths)

Build `ChunkAsVector` by **moving** `dense_vecs`/`colbert_vecs` (`into_iter`),
not cloning; split the sparse `HashMap` into parallel index/value arrays in a
**single pass** with the `>1e-5` threshold applied once. Lives in `embed.rs`
(shared by `post_index` and the retry worker).

## Languages

The supported set *is* the `ProgrammingLanguage` enum + the grammar crates in
`Cargo.toml`; extension map in `tools/indexer/src/scanner.rs`. Hard
constraint: every grammar crate must depend on `tree-sitter ≥ 0.23`
(`LanguageFn` API) — older ones cause a native `links` conflict; verify with
`cargo info` + registry source before adding. **Adding a language touches all
of these** (each omission fails differently — 400 → SQLite CHECK 500 →
silently skipped file):

1. `ProgrammingLanguage` enum + `ToSql`/`FromSql` + `ALL`/`name()`
   (`backend/v0/models.rs` — crate-root `src/models.rs` is a two-line module
   list), lowercase serde name. Missing `name()` arm = compile error; missing
   `FromSql` arm fails on **read** (rows insert fine, then 500 on any query
   selecting the column); missing serde rename = 400
   `request.malformed_body`. Omission from `ALL` is the only **silent** one:
   absence from `GET /config`, *and* silent exclusion from
   `every_language_constructs_or_declines` (`slicing/symbols.rs`), which
   iterates `ALL` — a broken tags query for that language ships untested.
2. `CHECK` constraint on `project_files.programming_language` — in **two**
   places, and editing only the first is silent. `v1.0.0_schema.sql` builds a
   *fresh* database and is never re-read; a database in use needs a new
   migration rebuilding `project_files` with the widened list —
   `v1.1.0_toml_yaml_languages.sql` is the pattern (rule 8 has the rebuild's
   shape and hazards). Both files must end with the same list.
3. `tree-sitter-<lang>` in `Cargo.toml` (verify ≥ 0.23).
4. Arm in `tree_sitter_language(pl)` (`handlers.rs`) — total match, missing
   arm = compile error. A grammar here does **not** commit the language to the
   AST-walk slicer: `markdown` returns tree-sitter-md's *block* grammar and is
   dispatched to `MarkdownSlicer` by the one `pl == Markdown` branch in the
   prepare tx; a second such language adds a branch there, not a second code
   path.
5. Arm in `queries_for(pl)` (`slicing/symbols.rs`) — total match; `None` is
   legal (no symbols). Prefer the crate's `TAGS_QUERY` const; if the crate
   only *packages* `queries/tags.scm` (or ships a broken one), vendor the file
   under `slicing/queries/` with a provenance header (scala/csharp precedent).
   Add a fixture test in `symbols.rs` when a query exists. **Bump
   `SYMBOLS_DERIVATION_VERSION`** — without it already-indexed files never
   gain the new language's symbols. Today bash, html, css, json, toml, yaml,
   haskell, zig and sql ship no tags query → no symbols (chunking and search
   unaffected); revisit when upstream adds one. `markdown` is `None`
   *permanently*: headings are a table of contents, and making `outline` mean
   "sections" for one language buys a second meaning for the same tool. The
   vendored `.scm` files are verbatim copies — refresh when their crates bump.
6. `detect_language` + `Language::name()` in **three** extension maps, each
   silently skipping the file otherwise: `tools/indexer/src/scanner.rs`,
   `tools/watcher/src/scanner.rs` (a verbatim copy) and
   `tools/vscode/src/languages.ts` (`EXT_TO_LANGUAGE`).
7. `ext_to_lexer()` in `mindex-search.sh` (pygments map); its `VALID_LANGS` is
   only the offline fallback (canonical list from `GET /config`).
8. The VS Code **language mark**, four places — fails as a red test
   (`langIcons.test.ts` is exhaustive over `ALL_LANGUAGES`): `DEVICON_MARKS`
   (`esbuild.mjs`) if devicon draws the language, else `LANG_FALLBACK_CODICON`
   (`shared/langIcons.ts`) — sql and toml are the two it does not; a rule in
   `media/lang.css`; the base colour in the test's `BRAND` table. The two CSS
   hex values are **derived, not chosen** — the test recomputes them with its
   `adapt()` (mix toward white on dark / black on light in 5% steps until
   3:1); run that function and paste its output. `shared/langGlyphs.ts` is
   generated; rebuild, never hand-edit.
9. Rebuild the image. A container whose volume predates the `CHECK` change
   picks the widened list up from the migration in step 2 — which is what
   makes dropping the volume unnecessary, and why step 2 is not optional.

## Four clients, one working-tree view (sync rule)

`mindex-index`, `mindex-watch`, the VS Code extension and the MCP
`index_files` tool all answer the same question — *which files are in this
project and what is in them* — from four separate implementations. The server
never walks a tree; it only believes what a client posts. So **any change to
what a file set is, what a path spells, or what bytes get hashed must land in
every client in the same commit.** The concrete list:

- **The file set**: the `.mindex` walk (`tools/indexer/src/scanner.rs::scan`,
  `tools/watcher`'s `build_manifest`,
  `tools/vscode/src/scanner.ts::scanWorkspace`) — excludes-before-includes,
  glob dialect (`globset` with `literal_separator(true)` vs picomatch
  defaults), symlink policy, and the extension map (**three** copies,
  **Languages** step 6).
- **The bytes**: what is hashed for `/drift` must be exactly what `/index`
  would post — the server hashes `code.as_bytes()`, so a client hashing
  anything else (raw vs decoded, BOM kept vs stripped) reports permanent
  drift.
- **The refusals**: a file a client will not post must not appear in its
  manifest either. Binary, unreadable and over `mindexfile::MAX_CODE_BYTES`
  files are dropped from both, in every client (`scanner.ts` keeps its own
  copy of that constant). Claiming a file the server would reject is worse
  than dropping it: the 400 fails the whole batch, and the file reports
  `missing` forever.

This class of bug **never surfaces as an error** — only as drift reindexing
cannot clear, or an index quietly missing a third of the tree; hence the
checklist. `tools/mindexfile` exists to shrink the surface — the one Rust
parser, now also holding the shared size cap; the TypeScript mirror
(`mindexFile.ts` + `globContract.test.ts`'s shared fixture table) is the only
sanctioned copy.

**Git history is deliberately outside this rule**, single-producer — see **Git
history channel**. A commit list is not a file set, a path spelling, or hashed
bytes; do not "fix" that by teaching the watcher or the extension to walk git.

And the client is only as fresh as its build: the extension runs `dist/`, so a
change to `src/` never `npm run compile`d leaves a plugin scanning by
yesterday's rules. Recompile before concluding the plugin is wrong.

## Tooling gotchas (full docs: each tool's `--help` / README)

- `mindex-index`: identity and scope come from `.mindex` at `--root`;
  `--project`/`--include`/`--exclude`/`--language` **replace** (never extend)
  the matching key. `--print-guid` resolves the GUID and exits.
  `chunk_count == 0` = sliced to no chunks (<128 tokens), *not* unchanged —
  hash-unchanged files are absent entirely. `--check` runs `POST /drift`
  instead of uploading; non-zero exit on actionable drift (`--json` for
  scripts). `--force` bypasses the unchanged-skip (hash *and* derivation
  versions) — an escape hatch for what versioning can't see, not routine;
  scope it with `--include`/`--exclude`. `--symbols-only` rebuilds just the
  symbol table (no GPU, no Qdrant); its summary counts symbol rows, not
  chunks. `--history` additionally reconciles the git channel (off by
  default; `git_refs` in `.mindex` picks the refs, `--git-ref` replaces the
  list like every scope flag); `--history-only` restricts a run to that phase
  *without* switching the channel on. Watch the drop counts it prints.
- `mindex-watch`: inotify daemon — debounced reindex/delete (`--debounce-ms`,
  1000) + full drift sweep every `--drift-interval` (300 s) for offline
  changes. Reads `.mindex`. `--dry-run` makes no mutating call but still runs
  the read-only drift check.
- `mindex-search.sh`: the single search frontend. Prints results **ascending
  by score** (best match last, above the prompt); every option has a
  `MINDEX_*` env fallback (flag wins). Language validation fetches
  `GET /config` at runtime; baked-in `VALID_LANGS` is only the offline
  fallback. 404 = no match, not an error.
- MCP `mindex` (`tools/mcp/mindex/`): the **primary agent interface** —
  `search` (top-5 cap fixed in the adapter), `symbols` (exact-name lookup,
  10-per-role cap), `index_files`/`delete_files`, `drift`, `cancel_indexing`,
  read-only introspection. `index_files` is **only** for the few just-touched
  files, bodies passed **verbatim** (unchanged files are hash-skipped
  server-side); bulk jobs go through `mindex-index`. `search` takes optional
  `include`/`exclude` (`{paths, programming_languages}`) passed straight to
  `/search`. No network at handshake.
- VS Code (`tools/vscode`): the **Ask** sidebar WebviewView (`askView.ts`) is
  the one input surface for Search/Research (segmented toggle); search
  results stay in the QuickPick, research streams into a WebviewPanel; the
  SSE client is hand-rolled in `api.ts` (no reconnects — a drop is a cancel,
  by contract). **Full UX rationale: `docs/claude/vscode.md`** — read it
  before modifying the extension. Load-bearing rules: Research History is an
  editor-area panel (keyset paging by `seq`; one `AbortController` aborted
  per keystroke, and callers swallow `AbortError` themselves — `api.request`
  *rejects* on abort while `research()` resolves; keep the asymmetry). The
  context QuickPick offers **valid runs only**, tracks
  `onDidChangeSelection` (not `onDidAccept`'s visible selection), and
  `undefined` (cancel) ≠ empty array (clear). Stored reports open as
  read-only Markdown documents (scheme `mindex-research`) with a provenance
  block. The form offers only server-confirmed inventory (`chunks_active >
  0` languages, `research.models` via `StatusMonitor.refresh()`, which
  re-reads `/config` every pass) but validates against `ALL_LANGUAGES`
  (offering is a hint, validating is a contract; `undefined`/empty inventory
  falls back to `ALL_LANGUAGES`); state is pushed by `postMessage` and
  rebuilt, never by reassigning `webview.html`. `Availability {ask,
  research, reason}` is split (Ollama down disables only Research; health
  stays `"ok"`); a degradation aborts running work via `RunRegistry`,
  resetting handles **before** any notification (its thenable resolves only
  on dismissal), reported as a failure, not a cancellation; none of it is
  observable without `[mindex.statusPollSeconds]` (default 30, `0` = off).
  Language marks: generated `shared/langGlyphs.ts` (never hand-edit),
  two-toned colours derived and recomputed by `langIcons.test.ts`. A reindex
  reads the server's claims from `/status.indexing_claims` + the follow-up
  `/drift`'s `indexing` bucket — never from the `/index` response, which
  swallows claim conflicts and 200s (a refused reindex otherwise reads as
  `unchanged`); the drift check runs **before** the summary; the poll drops
  to 3 s while claims are outstanding; everything funnels through the one
  `reindex()` helper (total re-entry guard, the `reindexRunning` flag in
  `activate()`). Progress is a **feed**, not a percentage (indexing is
  batched): `IndexFeed` + `RateWindow` over the server's **cumulative**
  `chunks_done`; a `withProgress` message is structurally single-line, so
  paths live in a `StatusBarItem` tooltip. Drift's `Sync all` is a synthetic
  row present only while there is actionable drift; its prose lives in
  `viewsWelcome`, not `TreeView.message`.
- MCP `scout` (`tools/mcp/scout/`): token-economy layer, one tool —
  `research`, a thin SSE client over `POST /v0/{guid}/research`. The whole
  investigation runs on the server's local model; scout holds no prompt, no
  chunk budget, no Ollama connection — it owns the *reader* (`_STEP_KEYS`,
  `_USAGE_KEYS`, `_CITATION_KEYS`, whitelists that silently drop unknown
  fields) and the `_INSTRUCTIONS` telling the caller to trust the report but
  check `citations.unverified_paths` and `done_reason`, and to chain
  follow-ups via `context_run_ids` rather than re-investigating from cold.
  The cheap-breadth half; `mindex.search` is the paid-precision half. Fully
  removable layer.
- `.mindex` (repo-root, committed — index scope is part of the project):
  **YAML**, required `guid:` (either UUID spelling, normalized) + optional
  `exclude_paths:`/`include_paths:`/`languages:`/`git_refs:` **lists**.
  Unknown key = error (`deny_unknown_fields`), scalar-instead-of-list = error
  (a mistyped `exclude_path:` would otherwise index the tree it was meant to
  keep out). One file, repo root, no nesting. **`tools/mindexfile` is the
  only Rust parser** (indexer + watcher path-depend on it);
  `tools/vscode/src/mindexFile.ts` mirrors it; the post-commit hook parses
  nothing (shells out to `mindex-index --print-guid`). Adding a fourth parser
  is the mistake this crate exists to prevent. The extension also *writes*
  one (`tools/vscode/src/mindexTemplate.ts`, from the Drift view's welcome
  button) — its header comment restates the schema in prose and so drifts
  silently; keep it in step alongside the parser. Its exclude list is
  deliberately thin — only unambiguous root dot-dirs active, build artifacts
  commented out (a wrong guess shrinks the index with no error, and excludes
  apply *before* includes, so a blanket rule cannot be carved back open).
  Globs are root-relative, forward-slash, `*` stopping at `/` (`globset`
  needs `literal_separator(true)`; picomatch does it by default) — the
  shared fixture table in `mindexfile`'s tests and `globContract.test.ts`
  pins the subset; divergence surfaces as permanent phantom drift, not an
  error. The MCP servers don't parse it — the agent reads it and passes GUID
  + filters as call args.

## Docker & CI

- Toolchain pinned 1.95 (`libsqlite3-sys 0.38` needs ≥1.87). `cargo-chef` is
  **not** used (needed 1.88+, conflicted) — layer caching is
  `cargo fetch --locked` over a stub `src/main.rs`. Legacy builder supported;
  no `--mount=type=cache`.
- Three compose files, same `Dockerfile`:
  - **Prod** (`docker-compose.yml`): qdrant + mindex. The perf harness is
    host-side scripts in `perf/` driving this stack (`command:` flags read
    env; swap profiles via `--env-file perf/env/<f>.env`). **No host ports**;
    outbound-only `extra_hosts: host.docker.internal:host-gateway` reaches
    the host-run embedder (`:11211`, deliberately not composed — ~8 GB torch
    deps). TOML-only knobs require mounting a `config.toml`.
  - **Exposed overlay** (`docker-compose.exposed.yml`): opt-in via `-f`;
    publishes API (`11111`) + Qdrant dashboard (`6333`) on `127.0.0.1` only
    (neither has auth). The sanctioned way to open the stack.
  - **Test** (`docker-compose.test.yml`): qdrant + mock-embedder + mindex +
    test-runner. Run with `--exit-code-from test-runner
    --abort-on-container-exit`. Healthchecks use `/dev/tcp` / `urllib` (no
    curl in images). Mounts `tests/integration/mindex-test-config.toml`
    (small caps) so limit tests can exercise edge rejections. **Edit
    `v1.0.0_schema.sql` and you must `down -v` before the next run**: a
    volume already stamped at 1 skips the edited schema in silence and every
    request touching a new column 500s with `no such column` — the price of
    editing the schema in place, only paid pre-release.

## Tests

- **Unit**: `cargo test --bin mindex`; each `tools/` crate carries its own.
  Read the test files for coverage — highlights: the connection-leak and GC
  orphan-prevention regressions, the `codes_are_stable` snapshot,
  trigger-level illegal transitions, `sweep_candidates` selection rules. No
  server/Docker; some slicer tests need the BGE-M3 tokenizer in the HF cache
  (a fake-`Tokenizing` test avoids it).
- **Integration** (`tests/integration/`, pytest in Docker): mock embedder
  returns deterministic vectors seeded by text hash (stable ranking
  assertions). Fresh project GUID per test. Suites map by filename
  (`test_e2e`, `test_filters…`, `test_management`, `test_validation`,
  `test_concurrency`).

## Linting (zero warnings everywhere — non-default flags matter)

- Rust: `cargo clippy --bin mindex` + `cargo clippy` in each `tools/` crate
  (own workspaces), and `cargo fmt --check` in each — all four crates are
  edition 2024, where `collapsible_if` fires on the `if let` + `if` nesting
  that let-chains replace, and rustfmt's 2024 style edition sorts imports
  differently.
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
    phantom drift.
