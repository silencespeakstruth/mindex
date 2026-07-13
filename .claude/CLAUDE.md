# CLAUDE.md — mindex architecture & conventions

This file captures what is **not obvious from reading the code**: invariants,
conventions, non-trivial "why", gotchas, and regression guards. It deliberately
does **not** mirror the code — no flag tables (`--help`), no per-test lists (read
the tests), no language table (it's the `ProgrammingLanguage` enum + `Cargo.toml`),
no struct/SQL dumps. Deferred work, limitations, and assumptions live in `TODO.md`.

## Overview

`mindex` is an async RAG indexing + search engine in Rust. HTTPS API → `tree-sitter`
AST chunking → `BGE-M3` multi-vector embeddings (dense/sparse/ColBERT) → `Qdrant`
vectors + `SQLite3` metadata. Internal service: TLS is the only transport security,
no API auth.

## Configuration (two-level: TOML file + CLI flags)

`config.rs` owns it. Precedence is **CLI flag > TOML file > compiled default**, and
the *only* place defaults live is the `Default` impls in `config.rs` — clap holds no
`default_value` (every flag is `Option<T>`, so "passed" is distinguishable from
"absent"; that's what makes the override layering work). `resolve()` finds the file by
XDG canon (`--config`/`$MINDEX_CONFIG` → `$XDG_CONFIG_HOME/mindex/config.toml` →
`$XDG_CONFIG_DIRS/*/mindex/config.toml`; a missing file is fine → defaults), logs
every path it checked, the source it loaded, and **every flag override**, then
validates. Validation collects *all* problems (not fail-fast) with what/why/how-to-fix
messages; any error aborts startup (`deny_unknown_fields` makes a mis-typed key a parse
error). Keys carry their unit (`*_ms/_seconds/_minutes/_days/_chunks/_tokens/_bytes/
_points/_mib`). The indexer CLI has the same scheme in `tools/indexer/src/config.rs`
(`mindex/indexer.toml`). Documented examples: `config.example.toml`,
`tools/indexer/indexer.example.toml`.

**Only genuine tuning knobs are configurable.** Structural invariants stay as `const`
next to their code with a "why not configurable" comment — they would break the system
if changed alone: the BGE-M3 vector width `1024` (`qdrant.rs::VECTOR_DIM`),
`ENCODE_MAGIC`, `COLLECTION_SCHEMA_VERSION`, HTTP `499`, and `PRAGMA foreign_keys=ON`
/ `journal_mode=WAL`. **Threading:** config values reach code through constructors/
params, never globals — `EmbedTuning` (embed.rs), `BGEm3Tuning` (bge_m3.rs),
`RetryTuning` (retry.rs), `QdrantStore`'s prefetch fields, `Slicer::new` token window,
`SQLite3Pool::new` page/synchronous, and the rest via `RouterState` fields. A new knob =
add the key to the right `config.rs` section + its `Default` + a validation rule, then
thread it to the consumer (don't reintroduce a `const`). **Request-shape limits are
knobs too:** the `[limits]` section (`max_code_bytes`, `max_files_per_request`,
`max_drift_files`, `max_selector_patterns`) and `[search].max_top_k`/`max_query_bytes`
bound a request at the API edge — they reach the handlers via `RouterState` and feed the
validation layer (see Error handling & validation). They are TOML-only (no CLI flag), so
tuning them in a container means mounting a `config.toml`, not adding a `--flag`.

## Repository layout

```
src/
  main.rs               CLI (clap), startup, migrations, worker spawn, signal handling
  config.rs             Cli (flags), Config (TOML sections), resolve() — XDG load + override + validate
  backend/
    http3.rs            RouterState, EmbeddingModel, CancellationGuard, run() (HTTP/1.1+2 today)
    error.rs            ApiError (the client-error catalogue) + ProblemDetails (RFC 7807 body)
    extract.rs          ApiJson/ApiPath/ApiQuery — extractors that render rejections as ApiError
    openapi.rs          assembled OpenAPI doc (paths + schemas, incl. ProblemDetails)
    v0/{handlers,models,validate}.rs   post_index/post_search; request/response types, ProgrammingLanguage, UUIDv4, GlobPattern; request-edge validation
  db/
    sqlite3.rs          SQLite3Pool, SQLite3PoolError
    qdrant.rs           VectorStore trait (+ QdrantStore impl), VectorStoreError, ChunkAsVector, collection_name/collection_for
    files.rs            set_file_status — shared project_files.status transition
    migrations/   v0.1.0_schema.sql (projects, project_files, project_file_chunks);
                  v0.2.0_status_machine.sql (status-transition triggers + project_file_status_log);
                  v0.3.0_validation_checks.sql (defense-in-depth shape triggers: sha256-hex, line/column span, non-empty code, retry_count >= 0)
  models/bge_m3.rs      BGEm3HttpClient, BGEm3Model trait, EncodeError
  embed.rs              embed_and_upsert — shared embed→upsert pipeline, EmbedUpsertError
  slicing/traits.rs     Slicer, SlicedChunk, SlicerError, Tokenizing trait
  worker/{gc,retry}.rs  GC sweep + status-log prune (hourly), retry of stuck/failed files (60s)
scripts/entrypoint.sh   Docker entrypoint: self-signed cert on first start
tests/                  mock_embedder/ (FastAPI), integration/ (pytest), see Tests
tools/indexer/          mindex-index CLI (own Cargo.toml/lock, not in workspace)
tools/watcher/          mindex-watch CLI (own Cargo.toml/lock) — inotify daemon that live-syncs the index
tools/search/           mindex-search.sh (bash) — search frontend (flags + MINDEX_* env)
tools/mcp/mindex/       mindex (Python/Poetry) — MCP stdio server; search + live-index for a coding agent
tools/mcp/scout/        scout (Python/Poetry) — MCP stdio server; token-saving digest (decomposed queries → local-LLM summary)
embedder/               vendored BGE-M3 server (3 heads); host-run + GPU, NOT in the image — see embedder/README.md
Dockerfile, docker-compose{,.test}.yml, rust-toolchain.toml (pins 1.95)
```

## Core invariants (violating these causes bugs)

**Project isolation = collection + has_id filter.** Qdrant uses one collection per
project, `{guid_simple}_v0` (`COLLECTION_SCHEMA_VERSION` in `qdrant.rs`). Always
derive names from `collection_for(project_guid)`; never hardcode. Within a
collection, the candidate set is a `has_id` filter built from SQLite: the search
path queries `qdrant_guid` for chunks matching project + filters + **`status='active'`**
— this is the *sole* isolation mechanism and also excludes soft-deleted vectors.

**Append-only hot path.** Indexing never deletes from Qdrant. On reindex
(sha256 mismatch) old chunks are marked `status='deleted'` in SQLite, new ones
inserted `active`, new vectors upserted; old vectors orphan until the GC worker
removes them. Decouples indexing latency from Qdrant delete latency.

**GC hard-deletes only confirmed rows** (regression guard, `worker/gc.rs`). A sweep
deletes from SQLite *only* chunks whose Qdrant `delete_batch` succeeded; if a
collection's delete fails the rows stay `deleted` for the next sweep. Deleting the
SQLite row before Qdrant confirms would orphan the vector forever (nothing tracks
it). If every collection in a batch fails, the inner loop breaks rather than
spinning. The same pass prunes `project_file_status_log` past `STATUS_LOG_RETENTION`
(30 days) and runs `prune_deleted_files` — drops `deleted` `project_files` rows once
their chunks are gone (chunk→file FK is RESTRICT, so only after the sweep). That
sweep-then-drop ordering is what makes `DELETE /files` eventually physical; `POST /gc`
runs the same pass (`gc::collect`) synchronously. GC is **global** (not per-project),
so a pass is serialized process-wide by a single `Arc<AtomicBool>` flag via `GcGuard`
(`worker/gc.rs`) shared between the handler and the hourly worker: a `POST /gc`
arriving while a pass is already running returns **409**, and the worker **skips its
tick** if a manual pass holds the flag (it retries an hour later). The guard frees the
flag on `Drop`, so a panic/early-return can't wedge GC off.

**Status state machine** (`project_files.status`), enforced by SQLite triggers
(`v0.2.0_status_machine.sql`), not just convention. Legal moves: **any → `indexing`**
(start / reindex / retry), **any → `deleted`** (`DELETE /files`; GC then removes the
emptied row), and **`indexing` → `indexed`|`cancelled`|`failed`** (a terminal is
reachable only from in-progress work); a new row may only enter as
`just_uploaded`/`indexing`. `deleted` is terminal **except** `deleted→indexing`
(re-indexing a path pending deletion resurrects it — the any→`indexing` rule).
Anything else (e.g. `failed→indexed`, `just_uploaded→indexed`) raises
`SQLITE_CONSTRAINT_TRIGGER`. `indexing` is committed durably *before* heavy work
(crash-recoverable; the retry worker picks up files stuck in `indexing` longer than
`--stuck-grace-mins`, default **30 min**). That grace **must exceed the longest
legitimate in-flight request**: cross-file batching holds a whole batch in `indexing`
through the embed pass, so a too-short grace lets the worker race a live batch
(re-embedding its files and tripping illegal transitions). A stuck file with **no
active chunks** (too short → 0 chunks) is marked `indexed`, not `failed` (a wrong
`failed` would trap it, since `failed→indexed` is illegal). `sha256` is (re)written
when the file enters `indexing` (the prepare upsert) and confirmed again at
`indexed`, so the stored hash always matches the chunks in the table; the
`retry_count` reset still lands only on `indexed`. Status writes go through
`db::files::set_file_status` (stamps `status_updated_at`, WARNs on a rejected
transition); every transition is logged to `project_file_status_log` by AFTER-triggers.
A file that exhausts `MAX_RETRIES` (3) stays `failed` and is never retried again
(`worker::retry::warn_permanently_failed` surfaces it at startup and hourly).

```mermaid
stateDiagram-v2
    [*] --> indexing : POST /index (new file)
    [*] --> just_uploaded : initial upload

    just_uploaded --> indexing : index / reindex / retry
    indexing --> indexing : idempotent upsert

    indexing --> indexed : work succeeds
    indexing --> failed : work fails
    indexing --> cancelled : POST /cancel (mid-flight)

    indexed --> indexing : reindex (sha256 mismatch)
    failed --> indexing : retry worker (retry_count < MAX_RETRIES)
    cancelled --> indexing : reindex resurrects
    deleted --> indexing : reindex resurrects

    just_uploaded --> deleted : DELETE /files
    indexing --> deleted : DELETE /files
    indexed --> deleted : DELETE /files
    failed --> deleted : DELETE /files
    cancelled --> deleted : DELETE /files

    deleted --> [*] : GC removes emptied row
```

Note the missing edges are the point: `failed → indexed` is **illegal** (retry loops back
through `indexing`), and a row may only *enter* at `just_uploaded`/`indexing` — never
straight into a terminal. `POST /cancel` is `indexing → cancelled` (the file row is **not**
deleted). The same diagram lives in the root `README.md`.

**sha256 skip / empty 404.** Re-indexing identical content is skipped by hash.
`post_search` returns 404 immediately when the SQLite candidate set is empty (no
active chunks), without calling Qdrant (avoids a 503 from a missing collection).

**Management endpoints** (`handlers.rs`, routed in `http3::run`, *not* under `/v0`).
`DELETE /projects/{guid}` is an **immediate hard delete** (rows, then drop the
collection last so a retry re-attempts it), idempotent 204. `DELETE
/projects/{guid}/files` is a **soft delete** — its search `include`/`exclude`
selector goes in the **request body** (globs don't fit the path); it marks
files+chunks `deleted` for GC, returns 204 if none matched else 200+count, and
rejects an empty selector (400) so it can't wipe the project. `POST
/projects/{guid}/cancel` is a **best-effort, transactional index-cancel** — same
body selector as the soft delete, same empty-selector 400 — but it matches **only
`status='indexing'`** files (so an already-`indexed`/`failed` file is never touched:
a too-late cancel is a no-op), marks their chunks `deleted` and moves the files
`indexing → cancelled`. It deliberately does **not** take the per-file `IndexClaim`
(so it can interrupt a held one); correctness against a live `/index` rests on two
re-reads, not a lock: `post_index` calls `drop_cancelled` between Phase 1 and Phase 2
(re-reads prepared files' status, drops + chunk-soft-deletes any now-`cancelled` one
before the embed — this also closes the prepare race where cancel lands before the
chunks exist), and the **retry worker re-checks status after acquiring the claim**
(else `cancelled → indexing`, a legal move, would resurrect it). A cancel that lands
mid-embed lets that pass finish; `mark_indexed`'s `cancelled → indexed` is then
trigger-rejected and GC reclaims the orphaned vectors. `POST /projects/{guid}/retry`
**requeues `failed` files** (same body selector, but **empty body = all failed**,
since retry is non-destructive): it is a **metadata-only** write — `retry_count = 0`,
status stays `failed`, so it skips the state-machine triggers and takes no
`IndexClaim` — and it deliberately leaves `status_updated_at` untouched so the retry
worker (whose `failed` branch needs `status_updated_at < now-60`) re-embeds on the
next sweep rather than after another grace window. `POST /projects/{guid}/drift` is a
**read-only** working-tree comparison — it takes a posted `path → sha256` manifest
(entry count capped by `[limits].max_drift_files`) and, without touching any state,
classifies each path against the SQLite baseline into four buckets: `stale` (indexed
but hash differs → needs reindex), `missing` (present in the manifest but not indexed —
`failed`/never-indexed rows count as missing), `orphaned` (indexed but absent from the
manifest → should be deleted), and `indexing` (in-flight, deliberately excluded from
`stale`/`missing` since its stored hash is the *incoming* value not yet embedded). An
unknown project isn't a 404 — every posted file is simply `missing`. It's the query
behind `mindex-index --check`, the `drift` MCP tool, and the watcher's periodic sweep.
`GET /projects` (list all, summary
counts), `GET /projects/{guid}` (per-language stats), `GET /projects/{guid}/files`
(per-file listing — status / language / hash / active-chunk count / `retry_count`,
with optional `?status=`/`?language=` filters; `?status=failed` is the dead-letter
view), `POST /gc` (one synchronous GC pass), `GET /status` (live runtime/concurrency
state — held indexing claims, GC flag, SQLite pool headroom, global status counts),
`GET /config` (static knobs + the canonical supported-language list, read by the
search frontend), `GET /health` (pings SQLite + Qdrant + embedder, reports files
indexing) and `GET /version` round it out.

**FK is RESTRICT.** `project_file_chunks → project_files` is `ON DELETE RESTRICT`.
Never delete a `project_files` row while chunks exist; mark chunks deleted (let GC
clean up), then delete the parent.

## Retrieval pipeline

Collection has three named vectors: `dense` (1024-d cosine), `sparse` (SPLADE-style),
`colbert` (1024-d cosine, multivector MaxSim). Search: prefetch top-200 dense +
top-200 sparse → RRF fusion → ColBERT MaxSim rerank → top-k. `post_search` runs
**two** SQLite queries around Qdrant — first the candidate `qdrant_guid`s for the
`has_id` set, then `code`/metadata for *only* the top-k winners — never loading
`code` for the whole active set (don't collapse these back into one query). It then
**sorts results by score descending** before responding (don't rely on Qdrant's
return order). Sparse weights `≤ 1e-5`
are dropped before upsert. Batch sizes: `--embed-batch` chunks per `/encode` call
(default 256 — the GPU-load lever, paired with the embedder's own `--batch`), 256
points per Qdrant upsert/delete (`embed.rs`). Embed-response vector lists are
positionally aligned with the chunk list.

The embedder client (`bge_m3.rs::BGEm3HttpClient`) retries HTTP **429** (embedder
busy/backpressure) up to 3× with exponential backoff (200/400/800ms), respecting
the cancellation token during sleeps; if it's still 429, it gives up — the file is
marked `failed` and the retry worker re-attempts later (layered backoff). Each
`/encode` attempt also carries a whole-request timeout (`[model].encode_timeout_ms`,
default 10 min) so a wedged embedder can't hang the retry worker indefinitely — the
attempt fails and re-enters the same `failed` → retry path.

## Slicer

`Slicer` (`slicing/traits.rs`) walks the tree-sitter AST depth-first and selects
**named nodes** whose token span (HF tokenizer) is **128–512 tokens** — BGE-M3's
sweet spot. Token boundaries don't align with AST nodes and tokenization is
context-dependent, so the window is measured, not computed. `code` is extended
left to the node's line start *only* when the intervening bytes are pure indentation
(mid-line nodes are not extended). `SlicedChunk.start_byte/end_byte` are
`#[cfg(test)]`-gated — used only by the slicer's own byte-alignment tests, never
persisted — so they don't trip `dead_code` in non-test builds.

## Concurrency & cancellation

- **Async-first.** All I/O is async. SQLite runs in `spawn_blocking` via
  `db_pool.transaction()`; Qdrant/embed are `.await`-ed directly in handlers — no
  `block_on`.
- Every long loop / I/O respects a `tokio_util` `CancellationToken`. Client-cancelled
  requests return HTTP 499 (`cancelled_499()`, nginx convention).
- **How cancellation actually propagates (subtle).** A handler's `CancellationGuard`
  wraps a *fresh* token, cancelled only by its own `Drop`. On client disconnect
  axum drops the handler future → `Drop` fires `cancel()`, but the future is gone,
  so the in-handler `Cancelled` cleanup arms are **defensive, rarely hit**. The
  token's real job is letting in-flight `spawn_blocking` (slicer) and the embed
  `select!` bail early after the future is abandoned; the half-written DB row is
  recovered later by the retry worker. (Clean shutdown uses a *separate* token tree
  rooted in `main.rs`.)
- **Connection-return is cancellation-safe** (regression guard, `sqlite3.rs`). The
  blocking task pushes its connection back into the pool *itself*
  (`conns.blocking_lock().push`), not the awaiting code after `handle.await`.
  Dropping a `spawn_blocking` JoinHandle does **not** cancel the task, but if release
  depended on the awaiting future, a future dropped mid-transaction would leak the
  connection — after `db_pool_size` (4) such events the pool is permanently empty
  (`PoolEmpty` forever). A closure panic is the one case the conn isn't returned
  (logged on `JoinError`).

## SQLite pool

Fixed-size pool of `rusqlite::Connection` behind a `tokio::sync::Mutex<Vec<_>>`
(acquire = pop, return = push). Per-connection PRAGMAs at startup: WAL,
`foreign_keys=ON`, `synchronous=NORMAL`, 16 KB pages. Handlers run **multiple
sequential `transaction()` calls** (one per logical step), not one giant
transaction — so the soft-delete pattern keeps state recoverable if a later step
fails.

## post_index shape

`post_index` runs a `FileIndexer` in **two phases** so the GPU sees big batches
(not one file's chunks at a time):
1. **`prepare` every file** — hash-check (`Ok(None)` = unchanged, skipped) → set
   `indexing` → main tx (mark old chunks deleted + slice + insert) → returns a
   `Prepared` carrying that file's chunks. Each runs in its own `indexing_file`
   span (no `Entered` guard across `.await`).
2. **`embed_all`** the chunks from *all* prepared files in one batched pass
   (`embed::embed_and_upsert`, `--embed-batch` chunks per `/encode`).
3. **`mark_indexed`** each file + tally the response.

Recovery is per-batch: if any `prepare` or the shared embed fails, every
already-prepared file (still `indexing`, chunks inserted) is recovered to
`failed`/`cancelled` via `recover_all` and the retry worker re-embeds them later.
`tree_sitter::Parser` is `Send`, so the slicer is built inside the `spawn_blocking`
closure. The `/index` request body limit is `[server].max_body_mib` / `--max-body-mib` (default 256 MiB) via
`DefaultBodyLimit` — axum's 2 MB default is far too small for multi-file posts. A body
over that cap is rendered as problem+json (`ApiError::BodyTooLarge` → **413**
`request.body_too_large`), not axum's default plain-text 413.

## Mockable interfaces

Three traits let handlers/workers be unit-tested without live infra; the production
type is the only real impl, fakes live in `#[cfg(test)]`:
- **`BGEm3Model`** (`models/bge_m3.rs`) — embedder; held as `Arc<dyn BGEm3Model>` in
  `RouterState`/`EmbeddingModel` and the retry worker.
- **`VectorStore`** (`db/qdrant.rs`) — all Qdrant ops; impl'd for `Qdrant`, shared as
  `Arc<dyn VectorStore>`. Error is `VectorStoreError` (rendered string, not the
  unconstructible `QdrantError`) so fakes can simulate failures.
- **`Tokenizing`** (`slicing/traits.rs`) — the slicer's only tokenizer need
  (`token_offsets`); impl'd for `tokenizers::Tokenizer`; fakes avoid the HF download.

New seam = minimal trait + production type as sole impl + owned error if the real
one isn't test-constructible. `SQLite3Pool` is intentionally **not** a trait (its
generic-closure `transaction` isn't object-safe) — test DB code against a real
`:memory:` pool.

## Error handling, validation & logging

**The client error contract is `ApiError` → RFC 7807 (`backend/error.rs`).** Every
non-2xx response is an `application/problem+json` body (`ProblemDetails`) carrying a
**stable, namespaced machine `code`** (`validation.top_k_out_of_range`, `selector.empty`,
`project.not_found`, `embedder.unavailable`, …) plus English `title`/`detail` and an
optional `field`/`meta`. The `code` is the **localization key** — the server stays
English; a client maps `code` → its own catalogue and interpolates `meta`. `ApiError` is
the *single* enum (one variant per kind); its `code()`/`status()`/`title()`/`detail()`/
`meta()` and the lone `IntoResponse` impl are the only place a response shape is built.
**Codes are an API contract:** the `codes_are_stable` snapshot test fails on any
rename/add/remove, so changing one is deliberate (also update the OpenAPI catalogue in
`openapi.rs`'s `info.description` and any client). Handlers return `Result<_, ApiError>`;
domain errors reach it via `From`/explicit constructors at the call site (which keep the
contextual log + sysadmin hint — `From` itself is a pure mapping and never logs).

- Domain errors (`SQLite3PoolError`, `SlicerError`, `EncodeError`, `VectorStoreError`,
  `EmbedUpsertError`) convert into `ApiError` (`SQLite3PoolError::Cancelled` → `Cancelled`
  = 499, else `Internal`; embed/encode request/decode → `EmbedderUnavailable` = 503; Qdrant
  search → `QdrantUnavailable`, upsert/drop → `Internal`). No external error crates;
  `from_cancelled` still maps the `None` from `with_cancellation_token` (timeout/cancel) to
  `SQLite3PoolError::Cancelled` first.

**Validation happens at the edge (`backend/v0/validate.rs`), before any work**, so bad
input is a 400 with a precise `code` — never an opaque 500 from a SQLite `CHECK`. It
**mirrors the schema constraints** (`validate_path` = the `project_files.path` CHECK plus
a `..`-traversal guard; `validate_sha256_hex` = 64 hex chars) and enforces the `[limits]`/
`search.max_*` caps (`top_k`, query length, per-file code size, file/drift counts,
selector size). `require_nonempty_selector` is the shared empty-selector guard for the
destructive management endpoints. The schema `CHECK`s/`v0.3.0` triggers remain as
**defense-in-depth** behind this (a bug that bypasses the edge still can't write bad
rows). **Uniform rejections:** handlers take `ApiJson`/`ApiPath`/`ApiQuery` (`extract.rs`)
instead of the bare axum extractors, so a malformed body, a non-UUID path, or a bad query
string is the same problem+json envelope (`request.malformed_body`/`malformed_path`) — not
axum's default plain-text 400.
- No `unwrap`/`expect` in production paths (workers use `unwrap_or_default` for
  best-effort queries). Startup-only panics (`SQLite3Pool::new`) use
  `unwrap_or_else(|e| panic!(...))` naming the file and what to check.
- **Logging shape (uniform):** a mandatory message stating *what operation failed*
  (never bare `error!(?err)`); the error as a field `error = ?e`/`%e` (not
  interpolated); identifiers as fields (`%` for String/Uuid, `?` for enums).
  Handlers carry `project_guid`/`pl`/`path` on the span; workers (no span) pass
  them explicitly. Infra failures end with a one-line sysadmin hint (model-server
  reachability + the `0.0.0.0` vs `127.0.0.1` gotcha, Qdrant reachability, DB
  writability); logic errors don't.

## Performance conventions (hot paths)

Build `ChunkAsVector` by **moving** the per-batch `dense_vecs`/`colbert_vecs`
(`into_iter`), not cloning; split the sparse `HashMap` into parallel index/value
arrays in a **single pass** with the `>1e-5` threshold applied once. Lives in
`embed.rs` (shared by `post_index` and the retry worker).

## Languages

The supported set *is* the `ProgrammingLanguage` enum (`models.rs`) + the matching
crates in `Cargo.toml`; the extension→language map is `tools/indexer/src/scanner.rs`.
Hard constraint: every grammar crate must depend on `tree-sitter ≥ 0.23` (the
`LanguageFn`/`LANGUAGE` API) — older crates depend on full `tree-sitter` at an old
version and cause a native `links` conflict. Verify with `cargo info` + the source
in `~/.cargo/registry/src/` before adding. (Languages still blocked on upstream are
in `TODO.md`.)

**Adding a language touches all of these** — each omission fails differently (422 →
SQLite CHECK 500 → silently skipped file), so do the whole list:
1. `ProgrammingLanguage` enum + `ToSql`/`FromSql` + `ALL`/`name()` (`models.rs`),
   lowercase serde name. `ALL`/`name()` is the single source `GET /config` exposes,
   so a new variant must be added to `ALL` (else it's absent from the served list).
2. `CHECK` constraint in `v0.1.0_schema.sql`.
3. `tree-sitter-<lang>` in `Cargo.toml` (verify ≥ 0.23).
4. Arm in `tree_sitter_language(pl)` (`handlers.rs`) — total match, missing arm = compile error.
5. `detect_language` + `Language::name()` in `scanner.rs` (else the indexer silently skips the file).
6. `ext_to_lexer()` in `tools/search/mindex-search.sh` (the pygments lexer map). Its
   `VALID_LANGS` array is now only an **offline fallback** — the canonical validation
   list comes from `GET /config` at runtime — so it need not be hand-synced (though
   keeping it current keeps the offline path accurate).
7. Rebuild the image (`docker compose build mindex`) and, since the `CHECK` changed,
   drop the DB volume (`docker compose down && docker volume rm mindex_mindex_db && up -d`)
   — `CREATE TABLE IF NOT EXISTS` won't alter an existing table.

## Tooling (`tools/`)

Both CLIs document themselves via `--help`; only the non-obvious bits here.

- **`tools/indexer/` (`mindex-index`)** — walks a tree, uploads files. Detects
  language by extension and groups automatically, so one invocation with several
  `--include` globs covers multiple languages. **Always `--exclude 'tools/**'` when
  indexing mindex itself** (the CLIs' `long_about` text pollutes results).
  `chunk_count == 0` in the response means "sliced to no chunks" (below 128 tokens),
  *not* unchanged — hash-unchanged files are skipped server-side and absent entirely.
  `--check` runs a `POST /drift` instead of uploading: it walks + hashes the tree,
  reports stale/missing/orphaned, and exits non-zero on any actionable drift
  (`--json` prints the raw drift body for scripts).
- **`tools/watcher/` (`mindex-watch`)** — own crate (own `Cargo.toml/lock`, not in the
  workspace, same two-level XDG config scheme as the indexer via `mindex/watcher.toml`
  + `watcher.example.toml`). An **inotify daemon** that keeps the index live: it watches
  the project root, debounces filesystem events (`--debounce-ms`, default 1000), and
  reindexes changed files / `delete_files` removed ones — the same live-sync an agent
  does by hand through the MCP server, but automatic. It reads the same repo-root
  `.mindex` (GUID + optional `include_paths`/`exclude_paths`/`languages` scope) and
  every `--drift-interval` seconds (default 300) runs a full `POST /drift` sweep to catch
  changes made while it was offline. `--dry-run` logs every planned action but makes no
  mutating call (the read-only drift check still runs). It is the daemon counterpart to
  the agent-driven MCP maintenance and the manual `mindex-index --check`.
- **`tools/search/mindex-search.sh` (bash)** — the single search frontend. POSTs search,
  renders with `pygmentize` if present (else plain). Results print **ascending by
  score** so the best match is last, right above the prompt. Every API status is
  handled distinctly (404 = no match, not an error; 499/503/500 mapped; curl
  transport failure reported separately). The query comes from `--query`, then
  `$EDITOR` (`--edit`), then stdin. Every option has a `MINDEX_*` env-var fallback
  (so it can run fully env-driven, e.g. an alias or CI job); an explicit flag wins
  over its variable. (The old `mindex-search-edit` POSIX-sh wrapper was folded in.)
  Language-flag validation pulls the valid set from the server's `GET /config`
  (`refresh_valid_langs`, fetched only when a `--include/--exclude-lang` is given),
  falling back to the baked-in `VALID_LANGS` array when `/config` is unreachable —
  so the language list is no longer hand-synced with the server.
- **`tools/mcp/mindex/` (`mindex`, Python/Poetry)** — MCP stdio server (sibling of the
  CLIs; hits the same HTTP API). The **intended primary way an agent drives mindex**:
  `search` for precise code to read or edit (top-5 cap fixed in the adapter — the
  model can't raise it), `index_files`/`delete_files` to keep the index live as it
  edits. `drift` (wraps `POST /drift`) lets the agent verify the index against the
  working tree before trusting search, and `cancel_indexing` (wraps `POST /cancel`)
  aborts in-flight work for a selector; `health`/`list_projects`/`project_stats` round
  out the read-only introspection tools.
  Live reindex is meant to be called freely — unchanged files are hash-skipped
  server-side — but `index_files` carries full bodies, so it is **only** for the few
  files just touched, passed **verbatim**; a *bulk* (re)index or path-exclude job goes
  through `mindex-index`, not a loop of `index_files`. Reads the project GUID from a
  repo-root `.mindex` file (gitignored). No network at handshake (connects even with
  mindex down). `search` takes optional `include`/`exclude` filters
  (`{paths, programming_languages}`) passed straight through to `/search` — the only
  filtered MCP path (the tools otherwise expose just GUID+query); the backend already
  supported them, the adapter just plumbs them. See `tools/mcp/mindex/README.md`.
- **`tools/mcp/scout/` (`scout`, Python/Poetry)** — second MCP server, a
  **token-economy** layer in front of the same `/search` API: the agent sends 2-4
  decomposed sub-queries, a local LLM (Ollama, default `qwen2.5:14b`) reads the
  matching chunks and returns only a compact summary + `[path:start-end]` pointers,
  so raw code never enters the agent's context (roughly an order-of-magnitude context
  saving on a survey). It's the **cheap-breadth half** (server `scout`, tool `digest`);
  `mindex`'s raw `search` is the **paid-precision half** — orient with `digest`, then
  follow its pointers with `search` for exact code. Recall is governed by
  `DIGEST_MAX_CHUNKS`/`DIGEST_NUM_CTX`, which **must move together**: too small a
  `num_ctx` silently truncates the digester's prompt and drops the lowest-scored
  long-tail chunks. `digest` also takes optional `include`/`exclude` (same shape as
  `search`), applied to every sub-query. mindex is untouched and the layer is fully
  removable. See `tools/mcp/scout/README.md`.

The `.mindex` file (repo-root, gitignored) is **GUID on the first non-comment line**;
optional `exclude_paths:` / `include_paths:` / `languages:` lines below it carry
project-standing search scope that the `digest` agent reads and passes as the
`include`/`exclude` filters above (the MCP servers themselves don't parse `.mindex` —
they take the GUID + filters as call args).

## Docker & CI

- Toolchain pinned to 1.95 (`rust-toolchain.toml`); `libsqlite3-sys 0.38` needs
  ≥1.87, `icu_collections 2.2` needs ≥1.86. `cargo-chef` is **not** used (needed
  1.88+ and conflicted) — layer caching is `cargo fetch --locked` over a stub
  `src/main.rs`, so only `src/` changes trigger recompiles. Legacy builder (no
  BuildKit) supported; no `--mount=type=cache`.
- `entrypoint.sh` generates a self-signed RSA-4096 cert into the `mindex_certs`
  volume on first start.
Three compose files (all build the **same** `Dockerfile`):
- **Prod compose** (`docker-compose.yml`): the turnkey/reference stack — `qdrant` +
  `mindex` — *and* the perf-benchmark harness (the `command:` flags read from the
  environment; swap a profile with `docker compose --env-file perf/env/<f>.env up -d`,
  see `.env.example`). **Publishes no host ports** — the whole stack lives on the
  internal compose network (mindex → Qdrant at `http://qdrant:6334`); the only host
  boundary it crosses is *outbound*, via `extra_hosts: host.docker.internal:host-gateway`,
  which lets the container reach a **host-run embedder** (`:11211`, deliberately not
  composed — its ~8 GB torch deps keep it on the host; a temporary wrapper until an
  off-the-shelf server emits all three BGE-M3 heads). It is the canonical reference for
  the server's flags. Because the `[limits]`/`search.max_*` knobs are TOML-only, tuning
  them in the container means mounting a `config.toml` (the flags here don't cover them;
  defaults are sensible).
- **Exposed overlay** (`docker-compose.exposed.yml`): opt-in, **not** auto-merged —
  pass it explicitly (`docker compose -f docker-compose.yml -f docker-compose.exposed.yml
  up -d`). Publishes the mindex API (`11111`) and the Qdrant dashboard (`6333`) on
  `127.0.0.1` only (neither has auth), so host tools can reach the API. Base stack stays
  port-less; this is the sanctioned way to open it, replacing the old untracked
  `docker-compose.override.yml`.
- **Test compose** (`docker-compose.test.yml`, doesn't extend base): qdrant +
  mock-embedder + mindex + test-runner — the integration-test stack (mindex's *primary*
  containerized use). Run:
  `docker compose -f docker-compose.test.yml up --build --exit-code-from test-runner --abort-on-container-exit`.
  Healthchecks use `/dev/tcp` (qdrant) and `urllib` (embedder) because neither image
  has curl. `mindex` has no host port; test-runner reaches it on the internal net.
  `mindex` mounts `tests/integration/mindex-test-config.toml` (small `[limits]`/
  `[search].max_*` caps) so the request-shape limit tests can exercise the edge
  rejections — those knobs are TOML-only, so they can't be set by a compose `command:`
  flag. Migrations are additive (incl. `v0.3.0`'s `IF NOT EXISTS` triggers), so a re-run
  against the persisted `test_mindex_db` volume does **not** need a volume drop.

## Tests

- **Unit** (`cargo test --bin mindex`): slicer (incl. a fake-`Tokenizing` test, no
  HF download), `build_search_query` SQL/param numbering, `embed_and_upsert` via fake
  `BGEm3Model` + fake `VectorStore`, SQLite pool (incl. the connection-leak
  regression), GC sweep (incl. the orphan-prevention regression via a `FakeStore`),
  the `ApiError` → `ProblemDetails` envelope + the `codes_are_stable` contract snapshot
  (`backend/error.rs`), the per-rule validation tests (`backend/v0/validate.rs`), the
  migration runner (`apply_pending_migrations` — fresh/partial/up-to-date DBs +
  `user_version` stamping), the `v0.2.0`/`v0.3.0` triggers (illegal transitions +
  defense-in-depth shape checks rejected at the SQLite layer), and the retry worker's
  `sweep_candidates` selection rules (stuck grace / failed cooldown / retry budget).
  The `tools/` crates carry their own unit tests too (`mindex-watch`'s
  `convert_event`/`classify` + `.mindex` parsing; the indexer's `scan()` language
  detection + globs). No server/Docker; some slicer tests need the BGE-M3 tokenizer from
  the HF cache.
- **Integration** (`tests/integration/`, pytest in Docker): mock embedder returns
  deterministic vectors seeded by text hash (stable ranking assertions). `test_e2e.py`
  is the rust happy path; `test_filters_and_languages.py` covers non-rust languages
  and include/exclude (language + path-GLOB) filters; `test_management.py` covers the
  stats / delete-project / delete-files / GC endpoints (each deletion test calls
  `POST /gc` to confirm physical removal); `test_validation.py` asserts the
  problem+json envelope + expected `code` for bad path / over-cap top_k / empty query /
  empty selector / bad sha256 / malformed body / non-UUID path (and the request-shape
  limit caps — code size / file counts / drift manifest / 413 body cap — via the mounted
  `mindex-test-config.toml`); `test_concurrency.py` covers a `DELETE /projects` racing a
  live index, search with the embedder down (503 `embedder.unavailable`), and pool
  saturation. Fresh project GUID per test.

## Linting (zero warnings everywhere — non-default flags matter)

- Rust: `cargo clippy --bin mindex`, `cd tools/indexer && cargo clippy`, and
  `cd tools/watcher && cargo clippy` (each `tools/` crate is its own workspace).
- Shell: `shellcheck scripts/entrypoint.sh`, `shellcheck --shell=bash tools/search/mindex-search.sh`;
  format with `shfmt -i 4 -ci` (4-space + indented case — bare `shfmt` defaults to tabs).
- Python (`tests/`): `ruff check`, `ruff format --check` **and** `black --check`
  (kept compatible — avoid the long `assert cond, "msg"` they format differently),
  `mypy` (`fastapi` is `# type: ignore` — its stubs live only in the mock's image).
- SQL: `sqlfluff lint src/db/migrations/` — dialect + relaxed layout rules come
  from repo-root `.sqlfluff` (schema is intentionally column-aligned).
- On intentional code, prefer a scoped `#[allow(...)]`/config exclusion **with a
  reason** over contorting code (see `OptionResultExt::from_cancelled`,
  `qdrant::VectorStore::search`, `.sqlfluff`). Never project-wide suppression.

## When modifying code

1. New loops touching Qdrant/SQLite/model-server must respect the `CancellationToken`.
2. Multi-row DB writes go inside a `transaction`.
3. New endpoints register in `backend::http3::run`, use `RouterState`, `{param}` route
   syntax, `#[debug_handler]`, and the `ApiJson`/`ApiPath`/`ApiQuery` extractors (not the
   bare axum ones) so rejections stay problem+json. Return `Result<_, ApiError>` and
   validate inputs at the top via `backend::v0::validate` (a new check = a new `ApiError`
   variant + its `code`/`status`/`detail` arms + the `codes_are_stable` snapshot + a unit
   test). They also need a `#[utoipa::path(...)]` annotation (tag = one of the existing
   groups, every error response citing `body = ProblemDetails`, a `**Concurrency:**` note)
   **and** an entry in `backend::openapi.rs`'s `paths(...)` (+ any new body/response type in
   `schemas(...)`) — a handler missing there is silently absent from Swagger, not a
   compile error; the `openapi_spec_is_complete_and_versioned` test guards the path count.
   Swagger UI is served at `/swagger-ui`, the raw spec at `/api-docs/openapi.json`; the
   UI assets are vendored into the binary (`utoipa-swagger-ui` `vendored` feature) so no
   network fetch happens at build or runtime.
4. Reach Qdrant only via `VectorStore`; derive collection names from `collection_for`.
5. Any search path's SQLite query must include `AND c.status = 'active'`.
6. Status writes use `set_file_status` (stamps `status_updated_at`) and must be a
   legal transition (triggers enforce it — see the state machine). New
   status-changing paths need a transition test.
7. Adding a language → the full checklist under **Languages**.
8. Schema change → new migration file in the `MIGRATIONS` slice **with the next sequential
   integer version** (e.g. `(4, include_str!("db/migrations/v0.4.0_foo.sql"))`). Startup
   reads `PRAGMA user_version` and applies only migrations whose version exceeds it, then
   sets `user_version` to the highest applied version. All SQL must use `IF NOT EXISTS` so
   a cold re-run on a DB that was already at that version is a no-op; the DB is otherwise
   droppable (no upgrade path). SQLite can't `ALTER` a `CHECK` onto an existing table, so
   add new constraints as `BEFORE INSERT/UPDATE` triggers (the `v0.2.0` status machine /
   `v0.3.0` validation checks pattern) — additive, so they apply to a persisted DB without
   a volume drop. A *column* change (vs. a constraint) still needs a fresh DB.
