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

## Repository layout

```
src/
  main.rs               CLI (clap), startup, migrations, worker spawn, signal handling
  backend/
    http3.rs            RouterState, EmbeddingModel, CancellationGuard, run() (HTTP/1.1+2 today)
    v0/{handlers,models}.rs   post_index/post_search; request/response types, ProgrammingLanguage, UUIDv4, GlobPattern
  db/
    sqlite3.rs          SQLite3Pool, SQLite3PoolError
    qdrant.rs           VectorStore trait (+impl for Qdrant), VectorStoreError, ChunkAsVector, collection_name/collection_for
    files.rs            set_file_status — shared project_files.status transition
    migrations/   v0.1.0_schema.sql (projects, project_files, project_file_chunks);
                  v0.2.0_status_machine.sql (status-transition triggers + project_file_status_log)
  models/bge_m3.rs      BGEm3HttpClient, BGEm3Model trait, EncodeError
  embed.rs              embed_and_upsert — shared embed→upsert pipeline, EmbedUpsertError
  slicing/traits.rs     Slicer, SlicedChunk, SlicerError, Tokenizing trait
  worker/{gc,retry}.rs  GC sweep + status-log prune (hourly), retry of stuck/failed files (60s)
scripts/entrypoint.sh   Docker entrypoint: self-signed cert on first start
tests/                  mock_embedder/ (FastAPI), integration/ (pytest), see Tests
tools/indexer/          mindex-index CLI (own Cargo.toml/lock, not in workspace)
tools/search/           mindex-search (bash) + mindex-search-edit (POSIX sh)
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
spinning. The same hourly GC tick also prunes `project_file_status_log` rows older
than `STATUS_LOG_RETENTION` (30 days), logging the count removed.

**Status state machine** (`project_files.status`), enforced by SQLite triggers
(`v0.2.0_status_machine.sql`), not just convention. Legal moves: **any → `indexing`**
(start / reindex / retry) and **`indexing` → `indexed`|`cancelled`|`failed`** (a
terminal is reachable only from in-progress work); a new row may only enter as
`just_uploaded`/`indexing`. Anything else (e.g. `indexed→failed`, `failed→indexed`,
`just_uploaded→indexed`) raises `SQLITE_CONSTRAINT_TRIGGER`. So the retry worker
moves `failed → indexing → {indexed|failed}` (never `failed→failed`). `indexing`
is committed durably *before* heavy work (crash-recoverable; retry worker picks up
files stuck ≥5 min). `sha256` is written only when reaching `indexed`; `retry_count`
resets to 0 there and bumps on each `→failed`. Any status write sets
`status_updated_at = unixepoch()` — use `db::files::set_file_status`, which also
logs a WARN if the transition is rejected. Every transition is recorded in
`project_file_status_log` by AFTER-triggers (durable audit trail). A file that
exhausts `MAX_RETRIES` (3) stays `failed` and is never retried again — surfaced by
`worker::retry::warn_permanently_failed` at startup and hourly.

**sha256 skip / empty 404.** Re-indexing identical content is skipped by hash.
`post_search` returns 404 immediately when the SQLite candidate set is empty (no
active chunks), without calling Qdrant (avoids a 503 from a missing collection).

**FK is RESTRICT.** `project_file_chunks → project_files` is `ON DELETE RESTRICT`.
Never delete a `project_files` row while chunks exist; mark chunks deleted (let GC
clean up), then delete the parent.

## Retrieval pipeline

Collection has three named vectors: `dense` (1024-d cosine), `sparse` (SPLADE-style),
`colbert` (1024-d cosine, multivector MaxSim). Search: prefetch top-200 dense +
top-200 sparse → RRF fusion → ColBERT MaxSim rerank → top-k. Sparse weights `≤ 1e-5`
are dropped before upsert. Batch sizes: 64 chunks per embed call, 256 points per
Qdrant upsert/delete (`embed.rs`). Embed-response vector lists are positionally
aligned with the chunk list.

The embedder client (`bge_m3.rs::BGEm3HttpClient`) retries HTTP **429** (embedder
busy/backpressure) up to 3× with exponential backoff (200/400/800ms), respecting
the cancellation token during sleeps; if it's still 429, it gives up — the file is
marked `failed` and the retry worker re-attempts later (layered backoff).

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

`post_index` builds a `FileIndexer` (borrowed view of db/store/tokenizer/embedder)
and calls `index_one` per file → `Ok(None)` (unchanged, skipped), `Ok(Some(n))`
(indexed n chunks), or `Err` (status already recovered, carries HTTP status).
`index_one` instruments its own future with the `indexing_file` span (no `Entered`
guard held across `.await`). Per file: hash-check → set `indexing` → main tx (mark
old chunks deleted + slice + insert) → `embed::embed_and_upsert` → set `indexed`;
any error path recovers status to `cancelled`/`failed`. `tree_sitter::Parser` is
`Send`, so the slicer is built inside the `spawn_blocking` closure.

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

## Error handling & logging

- Domain errors (`SQLite3PoolError`, `SlicerError`, `EncodeError`, `VectorStoreError`,
  `EmbedUpsertError`) are convertible to HTTP statuses. No external error crates;
  follow `slicer_err_to_pool_err` / `from_cancelled`. `from_cancelled` maps the
  `None` from `with_cancellation_token` (timeout/cancel) to `Cancelled`.
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
1. `ProgrammingLanguage` enum + `ToSql`/`FromSql` (`models.rs`), lowercase serde name.
2. `CHECK` constraint in `v0.1.0_schema.sql`.
3. `tree-sitter-<lang>` in `Cargo.toml` (verify ≥ 0.23).
4. Arm in `tree_sitter_language(pl)` (`handlers.rs`) — total match, missing arm = compile error.
5. `detect_language` + `Language::name()` in `scanner.rs` (else the indexer silently skips the file).
6. `VALID_LANGS` + `ext_to_lexer()` in `tools/search/mindex-search`.
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
- **`tools/search/mindex-search` (bash)** — POSTs search, renders with `pygmentize`
  if present (else plain). Results print **ascending by score** so the best match is
  last, right above the prompt. Every API status is handled distinctly (404 = no
  match, not an error; 499/503/500 mapped; curl transport failure reported
  separately). `mindex-search-edit` (POSIX sh, no bashisms) is the
  `$EDITOR → pipe → render` wrapper, configured entirely via `MINDEX_*` env vars;
  CLI args forwarded to `mindex-search` win over env (last-one-wins).

## Docker & CI

- Toolchain pinned to 1.95 (`rust-toolchain.toml`); `libsqlite3-sys 0.38` needs
  ≥1.87, `icu_collections 2.2` needs ≥1.86. `cargo-chef` is **not** used (needed
  1.88+ and conflicted) — layer caching is `cargo fetch --locked` over a stub
  `src/main.rs`, so only `src/` changes trigger recompiles. Legacy builder (no
  BuildKit) supported; no `--mount=type=cache`.
- `entrypoint.sh` generates a self-signed RSA-4096 cert into the `mindex_certs`
  volume on first start.
- **Prod compose** (`docker-compose.yml`): `qdrant` + `mindex`;
  `extra_hosts: host.docker.internal:host-gateway` lets the container reach a
  host-run embedder.
- **Test compose** (`docker-compose.test.yml`, doesn't extend base): qdrant +
  mock-embedder + mindex + test-runner. Run:
  `docker compose -f docker-compose.test.yml up --build --exit-code-from test-runner --abort-on-container-exit`.
  Healthchecks use `/dev/tcp` (qdrant) and `urllib` (embedder) because neither image
  has curl. `mindex` has no host port; test-runner reaches it on the internal net.

## Tests

- **Unit** (`cargo test --bin mindex`): slicer (incl. a fake-`Tokenizing` test, no
  HF download), `build_search_query` SQL/param numbering, `embed_and_upsert` via fake
  `BGEm3Model` + fake `VectorStore`, SQLite pool (incl. the connection-leak
  regression), GC sweep (incl. the orphan-prevention regression via a `FakeStore`).
  No server/Docker; some slicer tests need the BGE-M3 tokenizer from the HF cache.
- **Integration** (`tests/integration/`, pytest in Docker): mock embedder returns
  deterministic vectors seeded by text hash (stable ranking assertions). `test_e2e.py`
  is the rust happy path; `test_filters_and_languages.py` covers non-rust languages
  and include/exclude (language + path-GLOB) filters. Fresh project GUID per test.

## Linting (zero warnings everywhere — non-default flags matter)

- Rust: `cargo clippy --bin mindex` and `cd tools/indexer && cargo clippy`.
- Shell: `shellcheck scripts/entrypoint.sh`, `shellcheck --shell=bash tools/search/mindex-search`,
  `shellcheck --shell=sh tools/search/mindex-search-edit`; format with
  `shfmt -i 4 -ci` (4-space + indented case — bare `shfmt` defaults to tabs).
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
3. New endpoints register in `backend::http3::run`, use `RouterState`, `{param}` route syntax, `#[debug_handler]`.
4. Reach Qdrant only via `VectorStore`; derive collection names from `collection_for`.
5. Any search path's SQLite query must include `AND c.status = 'active'`.
6. Status writes set `status_updated_at = unixepoch()` (use `set_file_status`) and
   must be a legal transition (triggers enforce it; only `any→indexing` and
   `indexing→terminal`). New status-changing code paths need a transition test.
7. Adding a language → the full checklist under **Languages**.
8. Schema change → new migration file in the `MIGRATIONS` slice; the DB is
   droppable (no upgrade path).
