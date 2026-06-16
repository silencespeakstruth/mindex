# CLAUDE.md: mindex Architecture & Standards

## Project Overview
`mindex` is a high-performance, asynchronous RAG indexing and search engine built in Rust. It exposes an HTTPS API that indexes source code files using `tree-sitter` for semantic AST chunking, `BGE-M3` for multi-vector embeddings (dense, sparse, ColBERT), `Qdrant` for vector storage, and `SQLite3` for relational metadata. It is an **internal service** — TLS is the only transport-layer security; no API authentication is implemented or planned.

## Repository Layout
```
mindex/
  src/
    main.rs                   — CLI (clap), startup, migration runner, worker spawning, signal handling
    backend/
      http3.rs                — RouterState, EmbeddingModel enum, CancellationGuard, run()
      v0/
        handlers.rs           — post_index, post_search, update_file_status, slicer_err_to_pool_err
        models.rs             — request/response types, ProgrammingLanguage, UUIDv4, GlobPattern
    db/
      sqlite3.rs              — SQLite3Pool, SQLite3PoolError
      qdrant.rs               — ensure_project, insert_batch, delete_batch, search, collection_name
      migrations/
        v0.1.0_schema.sql     — projects, project_files, project_file_chunks tables
    models/
      bge_m3.rs               — BGEm3HttpClient, BGEm3Model trait, EncodeError
    slicing/
      traits.rs               — Slicer, SlicedChunk, SlicerError
    worker/
      gc.rs                   — GC worker: sweeps deleted chunks from Qdrant + SQLite hourly
      retry.rs                — Retry worker: re-embeds stuck/failed files every 60 s
  scripts/
    entrypoint.sh             — Docker entrypoint: auto-generates self-signed TLS cert on first start
  tests/
    mock_embedder/            — FastAPI mock BGE-M3 server (deterministic vectors, seeded by text hash)
      main.py
      Dockerfile
      requirements.txt
    integration/              — pytest end-to-end test suite
      conftest.py             — wait_for_mindex fixture (120 s), client, project fixtures
      test_e2e.py             — 8 integration tests
      Dockerfile
      requirements.txt
  tools/
    indexer/                  — Standalone Rust crate: mindex-index CLI (own Cargo.toml/Cargo.lock)
      src/
        main.rs               — CLI (clap), orchestration, progress display (indicatif + console)
        scanner.rs            — walkdir + globset: file discovery, extension→language detection
        client.rs             — reqwest: upload_batch(), IndexRequest/IndexResponse types
    search/
      mindex-search            — Bash CLI: terminal search frontend (curl + jq + pygmentize)
      mindex-search-edit       — POSIX sh wrapper: $EDITOR pipeline glue, env-var driven, no bashisms
  Dockerfile                  — Multi-stage: rust:1.95-bookworm builder → debian:bookworm-slim
  docker-compose.yml          — Production stack: qdrant + mindex
  docker-compose.test.yml     — Standalone test stack: qdrant + mock-embedder + mindex + test-runner
  rust-toolchain.toml         — Pins channel = "1.95" (matches rustc 1.95.0 locally and in CI)
```

## HTTP Server
- Framework: `axum` + `axum-server` with `rustls` (TLS 1.2/1.3).
- HTTP/3 support is **in-progress** — the `Protocol::Http3` CLI variant and `http3.rs` module name are forward-looking; the current `run()` function uses HTTP/1.1 + HTTP/2 only.
- Default bind: `127.0.0.1:11111`. Default TLS certs: `cert.pem` / `key.pem`.
- Routes: `POST /v0/{project_guid}/index`, `POST /v0/{project_guid}/search`.
  - Route params use `{param}` syntax (axum 0.8+), **not** `:param`.
- All route handlers must use `#[debug_handler]`. Maintain strict separation between `State<RouterState>` and `Path<T>` extractors.

## Qdrant Architecture (Critical)
Qdrant uses **one collection per project**, named `{project_guid_simple}_v0` via `collection_name()`. The `_v0` suffix is the collection schema version defined by `COLLECTION_SCHEMA_VERSION` in `db/qdrant.rs`. Within each collection, chunk-level discrimination is done via a `has_id` filter populated from SQLite.

Two-layer isolation:
1. **Collection = project.** Every `ensure_project`, `insert_batch`, `delete_batch`, and `search` call must pass `collection_name(project_guid.0.as_simple().to_string().as_str())` as the collection name.
2. **`has_id` filter = SQLite-derived active chunk GUIDs.** Before calling Qdrant, SQLite is queried to collect all `qdrant_guid` values where `c.status = 'active'` and that match the project + any additional filters (language, path GLOBs). Those GUIDs are passed as the Qdrant `has_id` filter, narrowing the vector search to the relevant subset.

- **Collection schema** (three named vectors per collection):
  - `dense`: 1024-dim cosine
  - `sparse`: sparse vector (SPLADE-style)
  - `colbert`: 1024-dim cosine, multivector MaxSim
- **Search pipeline**: prefetch top-200 dense + top-200 sparse → RRF fusion → ColBERT MaxSim reranking → top-k.
- **Sparse threshold**: filter out sparse weights `< 1e-5` before sending to Qdrant.
- **Batch sizes**: 64 chunks per embedding call, 256 points per Qdrant upsert/delete.
- **Append-only hot path:** Qdrant vectors are **never deleted during indexing**. Old vectors become orphaned; the GC worker removes them asynchronously. This decouples indexing latency from Qdrant delete latency.
- **Empty project 404:** `post_search` returns 404 immediately when the SQLite `has_id` set is empty (no active chunks), without calling Qdrant. This avoids a 503 from a non-existent collection.

## Embedding Model Server
`BGEm3HttpClient` calls a **custom Python/FlagEmbedding server** at `--model-server` (default `http://localhost:11211`).

API contract:
- `POST /encode` — body: `{"texts": ["..."]}` — response: `{"dense_vecs": [[f32, ...]], "sparse_vecs": [{u32: f32, ...}], "colbert_vecs": [[[f32, ...]]]}`.

Only one model is supported at runtime, set via `--model` (default `BAAI/bge-m3`). The `EmbeddingModel` enum in `http3.rs` is the extension point for adding models.

## SQLite3 Pool
`SQLite3Pool` is a fixed-size pool of `rusqlite::Connection` instances behind a `tokio::sync::Mutex<Vec<Connection>>`. Acquire = `Vec::pop`; release = `Vec::push`. Returns `PoolEmpty` if all connections are checked out.

Each connection is configured at startup:
```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;
PRAGMA page_size = 16384;
```

Transactions run on `tokio::task::spawn_blocking` threads. Each `db_pool.transaction()` call acquires one connection, runs the closure in a blocking thread, and releases the connection. **Multiple sequential `transaction()` calls are used per handler** (one per logical step), rather than one giant transaction spanning the whole request.

**Connection return is cancellation-safe (critical).** The blocking task pushes the connection back into the pool *itself* (`conns.blocking_lock().push(conn)` at the end of the closure), rather than the awaiting async code releasing it after `handle.await`. This matters because dropping a `spawn_blocking` `JoinHandle` does **not** cancel the task — it runs to completion regardless. If release depended on the awaiting future, then any time that future is dropped mid-transaction (client disconnect, `with_cancellation_token` firing), the connection would be silently lost. After `db_pool_size` (default 4) such events the pool would be permanently empty and every subsequent `transaction()` would return `PoolEmpty`. Returning the connection from inside the blocking task makes release independent of the caller's fate. (A panic in the closure is the one case where the connection is not returned — that is a programmer error, not a runtime condition, and is logged on `JoinError`.)

## File Status State Machine
`project_files.status` tracks the indexing lifecycle:

```
just_uploaded → indexing → indexed
                    ↓         ↓
                cancelled   (done)
                failed → (retried by worker, up to MAX_RETRIES=3)
```

- `just_uploaded`: file row exists but no indexing attempt has started.
- `indexing`: set durably (committed in its own transaction) before the heavy work begins. Survives server crashes; the retry worker picks up stuck files after a 5-minute grace period.
- `indexed`: set after Qdrant upsert succeeds. `sha256` is updated to the new hash only at this point.
- `cancelled`: client disconnected during embedding or Qdrant upsert.
- `failed`: embedding or Qdrant upsert failed. `retry_count` is incremented. Retry worker re-attempts up to `MAX_RETRIES` times.

## Soft-Delete & GC Architecture
`project_file_chunks.status` ∈ `{'active', 'deleted'}`.

**Re-index flow** (on sha256 mismatch):
1. `UPDATE project_file_chunks SET status='deleted' WHERE ... AND status='active'` — old chunks marked deleted, not removed.
2. New chunks inserted with `status='active'`.
3. New vectors upserted to Qdrant (old orphaned vectors remain).
4. On success: `project_files.status='indexed'`, `sha256` updated.

**GC worker** (`worker/gc.rs`, runs hourly):
- Reads batches of 256 `status='deleted'` chunks.
- Groups by `project_guid`, calls `delete_batch()` on each Qdrant collection.
- Hard-deletes from SQLite **only the chunks whose Qdrant `delete_batch` succeeded** (`confirmed_deleted`). If a collection's delete fails (transient Qdrant error), its rows stay `'deleted'` so the next sweep retries them. Deleting the SQLite row before Qdrant confirms would orphan the vector permanently — nothing would track it for a future delete. If *every* collection in a batch fails, the inner sweep loop `break`s rather than spinning on the same undeletable rows; the next hourly tick retries.

**FK constraint**: `project_file_chunks` FK to `project_files` is `ON DELETE RESTRICT`. Chunks must be explicitly managed; no silent cascade. To delete a project: hard-delete chunks first (or mark deleted and let GC clean up), then delete the project row.

**Search exclusion**: the SQLite query in `post_search` always includes `AND c.status = 'active'`, so soft-deleted chunks never appear in the `has_id` filter sent to Qdrant.

## post_index Transaction Sequence
For each file, `post_index` executes these sequential transactions (separate `db_pool.transaction()` calls):

1. **Project setup tx** (once per request): `INSERT INTO projects` if not exists.
2. **Qdrant `ensure_project`** (once per request, async).
3. Per file:
   - **Hash-check tx**: `SELECT sha256`; skip file if unchanged.
   - **Set-indexing tx**: UPSERT `project_files` with `status='indexing'` (committed before heavy work).
   - **Main-work tx**: `UPDATE chunks SET status='deleted'` + slice + `INSERT` new chunks. Returns `Vec<(UUIDv4, code)>` for embedding.
   - **Async embed + Qdrant upsert** (in async context, no `block_on`).
   - **Set-indexed tx**: `UPDATE project_files SET status='indexed', sha256=new_hash`.
   - On any error: **Recovery tx** sets `status='cancelled'` or `status='failed'` (with `retry_count++`).

`tree_sitter::Parser` is `Send` (explicitly impl'd by tree-sitter), so the slicer can be created inside `spawn_blocking` closures. The `Arc<Tokenizer>` is moved into the closure; `Slicer::new(lang, &tokenizer)` borrows within the closure's scope.

**Known correctness follow-up (observability, not data):** the per-file `info_span!("indexing_file", …)` is currently entered with `let _guard = file_span.enter();` and that guard is held across the file's `.await` points. Holding a `tracing` `Entered` guard across `.await` is the `await_holding_span_guard` anti-pattern: when the future parks at an await, the guard is not dropped, so the span stays entered on the worker thread and can mis-attribute *other* concurrently-running tasks' log events to this file span. It does **not** corrupt indexing data — only log/span attribution under concurrency. The correct fix is to wrap the per-file body in `async { … }.instrument(file_span).await`, but the body uses `continue`/`return` for control flow, so it needs the outcome re-encoded as a returned enum; deferred to avoid reshaping a tested path without e2e coverage. The outer `post_index`/`post_search` request spans use `.instrument(span)` correctly and are unaffected.

## Core Technical Standards
- **Async-First:** All I/O (Qdrant, SQLite, embedding inference) must be asynchronous. Never use blocking calls directly on a Tokio thread. Embedding and Qdrant calls happen in async context; SQLite calls happen in `spawn_blocking` via `db_pool.transaction()`.
- **No `block_on` in handlers:** The old pattern of calling `tokio::runtime::Handle::current().block_on(...)` inside `spawn_blocking` for Qdrant/embed is eliminated. Qdrant and embed calls are now `.await`-ed directly in the async handler.
- **Cancellation:** Every long-running loop or I/O call must respect a `CancellationToken`. Use `tokio_util::sync::CancellationToken`.
- **`CancellationGuard`:** Handlers create a `CancellationGuard(CancellationToken::new())` on entry. This is an RAII wrapper — `Drop` calls `cancel()`, so any in-flight work is cancelled when the handler returns (including on error paths).
- **`from_cancelled`:** The `OptionResultExt` trait converts `Option<Result<T, E>>` → `Result<T, E>`, mapping `None` (returned by `with_cancellation_token` on timeout/cancel) to `Err(SQLite3PoolError::Cancelled)`.
- **Status 499:** Client-cancelled requests return HTTP 499 (`cancelled_499()`), following nginx convention.

### How cancellation actually propagates (important subtlety)
The handler's `CancellationGuard` wraps a **fresh** token created at entry, *not* a token derived from the connection. Nothing cancels it during normal execution — it is only cancelled by its own `Drop`. Consequences:
- **Client disconnect** is handled by axum/hyper, which *drops the handler future*. Dropping the future runs `CancellationGuard::Drop` → `cancel()`. But the future is already gone, so it never resumes to run the `EncodeError::Cancelled` / `SQLite3PoolError::Cancelled` cleanup arms in the handler. Those arms (which set status `cancelled` and return 499) are therefore effectively **defensive / rarely-hit in practice**, not the primary disconnect path. The primary path is: future dropped → in-flight `spawn_blocking` (slicer) and the embed HTTP `tokio::select!` observe the now-cancelled token and bail out early, so they stop burning CPU/credits on a result nobody will read. The half-written DB state is recovered later by the retry worker (file stuck in `indexing` ≥5 min).
- The token's real, load-bearing job is thus **resource short-circuiting after the future is abandoned**, plus clean worker/server shutdown on SIGTERM (a separate token tree rooted in `main.rs`). It is *not* a mechanism for the handler to observe disconnect and run inline cleanup — don't rely on it for that.
- This is why `transaction()` returning the connection from inside the blocking task (see SQLite3 Pool) is essential rather than optional: it is the only release path that survives the future being dropped.

## Error Handling
- Domain error types: `SQLite3PoolError`, `SlicerError`, `EncodeError`.
- All custom errors must be convertible to HTTP status codes.
- Avoid external error-handling crates. Follow the existing `slicer_err_to_pool_err` and `from_cancelled` patterns.
- `slicer_err_to_pool_err` maps `SlicerError::Cancelled` → `SQLite3PoolError::Cancelled` (not HTTPStatusCode) so the downstream match arm can distinguish cancellation from other errors cleanly.
- Propagate errors with `?`. No `unwrap()` or `expect()` in production code paths (workers use `.unwrap_or_default()` only for best-effort queries where empty fallback is safe). Startup-only panics (`SQLite3Pool::new`) use `unwrap_or_else(|err| panic!(...))` with a message naming the file and what the operator should check.

### Logging conventions
All `error!`/`warn!`/`info!` calls follow one shape so logs are greppable and actionable:
- **A human-readable message string is mandatory** and states *what operation failed* (not just the error value). e.g. `"Qdrant upsert failed; marking file 'failed'."` — never a bare `error!(?err)`.
- **The error value is a structured field named `error`**: `error = ?e` (Debug) or `error = %e` (Display). Don't interpolate it into the message.
- **Identifiers are structured fields, not interpolated:** `project_guid`, `path`, `collection`, `chunk_count`, etc. Use `%` (Display) for `String`/`Uuid`, `?` (Debug) for enums/structs. Request handlers carry `project_guid` (and `pl`/`path` for indexing) on the tracing **span**, so per-event logs there don't repeat them; background workers have no span, so they pass `%project_guid, %path` explicitly.
- **Infrastructure failures end with a one-line sysadmin hint** telling the operator what to check: model-server reachability (and the `0.0.0.0` vs `127.0.0.1` bind gotcha), Qdrant reachability at `--qdrant-server`, DB writability, free bind address. Logic errors (bad UUID from Qdrant, slicer parse failure) don't need a hint.
- Spans use `project_guid = %project_guid.0` (hyphenated Display), consistently across `post_index` and `post_search`.

## BGE-M3 & Retrieval Pipeline
- **Indexing:**
  - Hash file content with `Sha256`; skip re-indexing if the hash matches what is stored in SQLite with `status='indexed'`.
  - On hash mismatch: mark old chunks `status='deleted'` (do NOT delete from Qdrant immediately). Insert new chunks. Upsert new vectors to Qdrant. Update `sha256` and `status='indexed'` only after both SQLite and Qdrant succeed.
  - Batch embedding calls at 64 chunks; batch Qdrant upserts at 256 points.
- **Retrieval:**
  - Filter at the SQLite level first (project, language, path GLOB, **`status='active'`**) to get the candidate `qdrant_guid` set.
  - If the set is empty, return 404 immediately without calling Qdrant.
  - Pass those GUIDs as a `has_id` filter to Qdrant — this is the sole project isolation mechanism and also excludes soft-deleted vectors.
  - Multi-vector (dense/sparse/colbert) alignment: each vector list from the embed response is positionally aligned with the chunk list.

## Performance & Scaling

Conventions to preserve when editing hot paths:
- **Move, don't clone, embed-response vectors.** When building `ChunkAsVector` from a `BGEm3EmbedResponse`, the per-batch `dense_vecs`/`colbert_vecs` are consumed with `into_iter()` and moved into the points (they're owned per-batch and unused afterwards), not `.clone()`d. The sparse `HashMap` is split into the parallel `(indices, values)` arrays Qdrant needs in a **single pass** with the `> 1e-5` threshold applied once — not two filtered passes. This pattern lives in both `post_index` and `worker/retry.rs`; keep them in sync.
- **Sparse threshold** (`weight > 1e-5`) is applied before the vector ever leaves the process, so Qdrant never stores near-zero sparse dimensions.

Known scaling consideration (not yet optimized — **the top performance follow-up**):
- **`post_search` materializes the full `code` of every active chunk in the project**, not just the top-k. The SQLite candidate query selects `qdrant_guid` *and* `code` + line/column metadata for all `status='active'` chunks matching the filters, into a `HashMap<UUIDv4, (code, ...)>`, purely so the post-Qdrant step can look up display data for the ~`top_k` (default 5) winners. For a large project this loads megabytes of source text per query, of which >99% is discarded. The clean fix is two queries: (1) select only `qdrant_guid` to build the `has_id` filter, then (2) after Qdrant returns the top-k ids, `SELECT ... WHERE qdrant_guid IN (<=top_k ids)` to fetch display data for just those. This was left as-is in the current pass because it reshapes a tested core path; do it deliberately, with the e2e suite green, when project sizes grow.
- The `has_id` filter sent to Qdrant grows linearly with the project's active-chunk count (it lists every candidate GUID). This is the documented isolation mechanism and is fine for moderate projects; very large collections may want a stored Qdrant payload field (e.g. `project_guid`) + a `match` filter instead, trading the per-query GUID list for an indexed payload lookup.

## SlicedChunk Fields
```rust
pub struct SlicedChunk {
    pub code: String,        // source text from start of node's line to end_byte (includes leading whitespace)
    #[cfg(test)]
    pub start_byte: usize,   // node.start_byte() (not line_start)
    #[cfg(test)]
    pub end_byte: usize,
    pub start_line: usize,   // 1-indexed
    pub end_line: usize,     // 1-indexed
    pub start_column: usize, // byte offset of the node within its start line
    pub end_column: usize,   // byte offset of the exclusive end within its end line
}
```
Leading whitespace: `code` is extended to the start of the node's line only when the intervening bytes are pure whitespace (indentation). Mid-line nodes are not extended.

`start_byte`/`end_byte` are `#[cfg(test)]`-gated (both on the struct and at the construction site in the slicer's main loop): they exist only to let the slicer's own unit tests assert byte-exact alignment between `code` and the source. Production code (`handlers.rs`) never reads them — it only destructures `code`/`start_line`/`end_line`/`start_column`/`end_column` — so leaving them as plain `pub` fields would otherwise be permanently flagged by `dead_code` on every non-test build.

## Slicer
`Slicer` (`slicing/traits.rs`) traverses the tree-sitter AST depth-first and selects **named nodes** whose token span (measured against the HuggingFace tokenizer) falls in the range **128–512 tokens** — the range where BGE-M3 performs best. Token boundaries do not align with AST node boundaries; the tokenizer is context-dependent. The slicer has 9 unit tests in `slicing/traits.rs`.

## Workers
Both workers are spawned with `tokio::spawn` in `main.rs` and receive a child of `sigterm_token`, so they shut down cleanly on SIGINT/SIGTERM.

**GC worker** (`worker/gc.rs`):
- Interval: 1 hour (`tokio::time::interval`, `MissedTickBehavior::Skip`).
- Inner loop: read 256 deleted chunks, group by project_guid, call `delete_batch` per collection, hard-delete from SQLite. Repeat until no deleted chunks remain.

**Retry worker** (`worker/retry.rs`):
- Interval: 60 seconds.
- Finds files with `status IN ('just_uploaded', 'indexing') AND status_updated_at < unixepoch() - 300` (stuck ≥5 min) OR `status='failed' AND retry_count < MAX_RETRIES AND status_updated_at < unixepoch() - 60`.
- Reads `status='active'` chunks from SQLite (their `code` column holds the text), re-embeds, upserts to Qdrant. Does **not** re-slice — sliced code is stored verbatim in the `code` column.
- On success: `status='indexed'`. On failure: `status='failed'`, `retry_count++`.
- `MAX_RETRIES = 3`. After 3 failures the file stays `status='failed'` and requires manual re-indexing.

## Indexer CLI (`tools/indexer/`)

Standalone Rust binary (`mindex-index`) that walks a directory tree and uploads source files to a running mindex server. It is **not** part of the main Cargo workspace — it has its own `Cargo.toml` and `Cargo.lock`.

**Build:** `cd tools/indexer && cargo build --release`

**Key flags:**

| Flag | Default | Purpose |
|------|---------|---------|
| `--server` | `https://127.0.0.1:11111` | mindex server URL |
| `--project` | (required) | 32-char hex UUID without dashes |
| `--root` | `.` | all stored paths are relative to this |
| `--include GLOB` | all recognised extensions | repeatable; matched against rel path |
| `--exclude GLOB` | — | repeatable; evaluated before includes |
| `--no-verify` | off | skip TLS cert check (self-signed default cert) |
| `--protocol` | `v0` | API version in URL path |
| `--batch-size` | `100` | files per HTTP request |
| `-v / --verbose` | off | print one line per file |

**Typical invocation (indexing mindex itself):**
```bash
cd tools/indexer
./target/release/mindex-index \
  --project $(uuidgen | tr -d - | tr '[:upper:]' '[:lower:]') \
  --root /data/silencespeakstruth/Projects/mindex \
  --include '**/*.rs' \
  --exclude 'target/**' --exclude 'tools/**' \
  --no-verify --verbose
```
Note: always `--exclude 'tools/**'` when indexing the mindex repo itself — otherwise the indexer's own `long_about` strings contaminate search results.

**Cancellation:** Ctrl+C drops the TCP connection. The server detects client disconnect and cancels in-flight work (returns HTTP 499). The CLI exits with code 1 and prints a partial summary.

**Response semantics:** `chunk_count == 0` in the server response means the slicer produced no chunks (file below 128-token threshold), not that the file was unchanged. Hash-unchanged files are silently skipped server-side and never appear in the response.

**Language detection** (extension → API key): `.rs`→rust, `.py`→python, `.js/.mjs/.cjs/.jsx`→javascript, `.ts/.mts/.cts`→typescript, `.tsx`→tsx, `.go`→go, `.c/.h`→c, `.cpp/.cc/.cxx/.hpp`→cpp, `.java`→java, `.cs`→csharp, `.rb`→ruby, `.php`→php, `.sh/.bash`→bash, `.html/.htm`→html, `.css`→css, `.json`→json, `.scala/.sc`→scala, `.hs/.lhs`→haskell, `.ml/.mli`→ocaml, `.zig`→zig, `.sql`→sql.

## Search CLI (`tools/search/mindex-search`)

A standalone Bash script (not Rust) that POSTs to `/v0/{project}/search` and renders results in the terminal. Built around `curl` + `jq`; uses `pygmentize` for syntax highlighting if present on `$PATH` (falls back to plain text otherwise — `bat`/`highlight`/`chroma` are not assumed to be installed).

**Designed to sit at the end of a shell pipe:**
```bash
tmp=$(mktemp); "${EDITOR:-vim}" "$tmp"
tools/search/mindex-search --project "$PROJECT" --no-verify < "$tmp"
rm -f "$tmp"
```
Or let it drive the editor itself with `--edit` (skips stdin, opens `$EDITOR` on a temp file, reads the result back). `--query TEXT` bypasses both for one-off non-interactive use.

**Key flags** (mirrors the indexer CLI's flag-naming conventions):

| Flag | Default | Purpose |
|------|---------|---------|
| `--server` | `https://127.0.0.1:11111` | mindex server URL |
| `--project` | (required) | 32-char hex GUID without dashes |
| `--protocol` | `v0` | API version in URL path |
| `--no-verify` | off | skip TLS cert check (self-signed default cert) |
| `--top-k N` | server default (5) | maps to `SearchRequest.top_k` |
| `--include-lang LANG` / `--exclude-lang LANG` | — | repeatable; validated against the 21 supported API keys before sending |
| `--include-path GLOB` / `--exclude-path GLOB` | — | repeatable; maps to `SearchFilter.paths` |
| `--edit` | off | open `$EDITOR` on a temp file instead of reading stdin |
| `--query TEXT` | — | use TEXT directly, skipping stdin/`--edit` |
| `--theme STYLE` | `monokai` | pygments style name |
| `--color always\|auto\|never` | `auto` | `auto` disables color when stdout isn't a TTY or `$NO_COLOR` is set |
| `-v / --verbose` | off | print the outgoing request JSON to stderr |

**Output order is intentionally inverted relative to the API/Qdrant response:** results are re-sorted ascending by `score` before printing, so the single best match is the last thing printed — right above the next shell prompt, where it's most visible without scrolling.

**Rendering:** each result prints a separator, `path:start_line-end_line  score=X.XXXX` header, then the full `code` field (never truncated) with a left-hand line-number gutter starting at `start_line`. The gutter is generated independently of the syntax highlighter (via `awk` after `pygmentize`) so highlighting never corrupts line-number alignment. The language passed to `pygmentize -l` is inferred from the result's file extension using the same extension→language table as the indexer CLI, since `SearchResult` carries no `programming_language` field.

**Error handling:** every status code the API can actually return is handled explicitly and distinctly — `200` (render), `404` (no active chunks matched; not an error, exits 1 with a plain message), `400`/`422` (malformed request; prints the server's body text, which axum's `Json` extractor populates on schema mismatches like an unknown language enum value), `499` (server treated the request as a cancelled client disconnect), `503` (embedding server or Qdrant down), `500` (server said + generic message), and any other code (raw status + body). A `curl` transport failure (connection refused, TLS error, timeout) is reported separately from HTTP-level errors. `--project`, `--top-k`, and `--include/exclude-lang` are validated client-side before any network call.

### `tools/search/mindex-search-edit` — POSIX sh pipeline wrapper

A separate, pure-POSIX `#!/bin/sh` script (no bashisms — no arrays, `[[ ]]`, or `local`; uses the `set -- "$@" ...` trick to build argument lists and `set -f` to suppress local glob expansion of path patterns like `src/**`). It is the piece meant to be distributed/packaged across heterogeneous environments and invoked directly by end users; `mindex-search` itself is the rendering engine it drives.

It implements the editor → pipe → render pipeline end to end:
1. Validates required tools (`mktemp`, `grep`, `tr`, `dirname`, `basename`) and resolves the `mindex-search` binary (`$MINDEX_SEARCH_BIN`, else alongside this script, else `$PATH`) — and validates `$MINDEX_PROJECT` is set — all *before* opening an editor, so a missing prerequisite never wastes a typed query.
2. Opens `$EDITOR` (default `vi`; may contain arguments, e.g. `EDITOR="code --wait"`) on a `mktemp`-created temp file, unless `$MINDEX_QUERY` is set, in which case that value is used directly and the editor step is skipped entirely (the mechanism for non-interactive/cron/CI use).
3. Translates `MINDEX_*` environment variables into `mindex-search` flags and execs it with the query file on stdin, propagating its exit code unchanged.

Every other `mindex-search` flag has a same-named `MINDEX_*` environment variable (`MINDEX_SERVER`, `MINDEX_PROTOCOL`, `MINDEX_NO_VERIFY`, `MINDEX_TOP_K`, `MINDEX_INCLUDE_LANG`/`MINDEX_INCLUDE_PATH`/`MINDEX_EXCLUDE_LANG`/`MINDEX_EXCLUDE_PATH` as space-separated lists, `MINDEX_THEME`, `MINDEX_COLOR`, `MINDEX_VERBOSE`). Any CLI arguments given to the wrapper are forwarded after the environment-derived flags, so they win on conflicting scalar options (`mindex-search`'s flag parser is last-one-wins). See `mindex-search-edit --help` for the full reference.

## Supported Languages

21 languages supported via tree-sitter grammars. All crates must require `tree-sitter ≥ 0.23` (the new `LanguageFn`/`LANGUAGE` constant API). Older crates cause a native `links` conflict and cannot coexist with `tree-sitter 0.26`.

| API key       | Enum variant                      | Crate                         | Constant used                   |
|---------------|-----------------------------------|-------------------------------|---------------------------------|
| `rust`        | `ProgrammingLanguage::Rust`       | `tree-sitter-rust 0.24`       | `LANGUAGE`                      |
| `python`      | `ProgrammingLanguage::Python`     | `tree-sitter-python 0.25`     | `LANGUAGE`                      |
| `javascript`  | `ProgrammingLanguage::JavaScript` | `tree-sitter-javascript 0.25` | `LANGUAGE`                      |
| `typescript`  | `ProgrammingLanguage::TypeScript` | `tree-sitter-typescript 0.23` | `LANGUAGE_TYPESCRIPT`           |
| `tsx`         | `ProgrammingLanguage::Tsx`        | `tree-sitter-typescript 0.23` | `LANGUAGE_TSX`                  |
| `go`          | `ProgrammingLanguage::Go`         | `tree-sitter-go 0.25`         | `LANGUAGE`                      |
| `c`           | `ProgrammingLanguage::C`          | `tree-sitter-c 0.24`          | `LANGUAGE`                      |
| `cpp`         | `ProgrammingLanguage::Cpp`        | `tree-sitter-cpp 0.23`        | `LANGUAGE`                      |
| `java`        | `ProgrammingLanguage::Java`       | `tree-sitter-java 0.23`       | `LANGUAGE`                      |
| `csharp`      | `ProgrammingLanguage::CSharp`     | `tree-sitter-c-sharp 0.23`    | `LANGUAGE`                      |
| `ruby`        | `ProgrammingLanguage::Ruby`       | `tree-sitter-ruby 0.23`       | `LANGUAGE`                      |
| `php`         | `ProgrammingLanguage::Php`        | `tree-sitter-php 0.24`        | `LANGUAGE_PHP` (PHP+HTML mode)  |
| `bash`        | `ProgrammingLanguage::Bash`       | `tree-sitter-bash 0.25`       | `LANGUAGE`                      |
| `html`        | `ProgrammingLanguage::Html`       | `tree-sitter-html 0.23`       | `LANGUAGE`                      |
| `css`         | `ProgrammingLanguage::Css`        | `tree-sitter-css 0.25`        | `LANGUAGE`                      |
| `json`        | `ProgrammingLanguage::Json`       | `tree-sitter-json 0.24`       | `LANGUAGE`                      |
| `scala`       | `ProgrammingLanguage::Scala`      | `tree-sitter-scala 0.26`      | `LANGUAGE`                      |
| `haskell`     | `ProgrammingLanguage::Haskell`    | `tree-sitter-haskell 0.23`    | `LANGUAGE`                      |
| `ocaml`       | `ProgrammingLanguage::Ocaml`      | `tree-sitter-ocaml 0.25`      | `LANGUAGE_OCAML` (.ml files)    |
| `zig`         | `ProgrammingLanguage::Zig`        | `tree-sitter-zig 1.1`         | `LANGUAGE`                      |
| `sql`         | `ProgrammingLanguage::Sql`        | `tree-sitter-sequel 0.3`      | `LANGUAGE`                      |

**Not yet supported** — crates still require `tree-sitter 0.21–0.22`, causing a native `links` conflict:
`yaml`, `toml`, `kotlin`, `lua`, `elixir`, `nix`, `erlang`, `swift`.
Add them once the upstream crates are updated to the `0.23+` API.

## Language Extensibility
When adding a new `ProgrammingLanguage`, touch all of these — `sql` (`tree-sitter-sequel`) was added this way and missing any single one produces a distinct, separately-discovered failure (422 from the API, then a SQLite `CHECK constraint failed`, then a silently-skipped file in the indexer):
1. Add the variant to the `ProgrammingLanguage` enum in `models.rs` and its `ToSql`/`FromSql` impls. Use a lowercase SQL name matching the serde rename. Skipping this → server returns `422 unknown variant`.
2. Add the SQL name to the `CHECK` constraint in `v0.1.0_schema.sql`. Skipping this → `SqliteFailure(ConstraintViolation, "CHECK constraint failed...")`, surfaced to the client as a bare `500`.
3. Add the `tree-sitter-<lang>` crate to `Cargo.toml`. **Verify** its `tree-sitter` dependency is `≥ 0.23` (check the crate's own `Cargo.toml`, e.g. via `cargo info <crate>` + inspecting the downloaded source in `~/.cargo/registry/src/`) — modern grammar crates depend on the lightweight `tree-sitter-language` ABI crate and only dev-depend on full `tree-sitter`, which is what avoids the `links` conflict. Older crates depend on full `tree-sitter` directly at an old version and will conflict.
4. Add an arm to the `let ts_language = match pl { ... }` block in the main-work tx closure in `post_index`. Use `Language::new(tree_sitter_<lang>::LANGUAGE)` for crates that export a plain `LANGUAGE` constant, or the crate-specific name (e.g. `LANGUAGE_TYPESCRIPT`, `LANGUAGE_OCAML`).
5. Add the extension(s) to `detect_language` in `tools/indexer/src/scanner.rs` and the matching arm to `Language::name()` in the same file. Skipping this → the indexer silently counts the file under `skipped_unknown` and it never reaches the server at all (no error, just absent from results — the easiest of these to miss).
6. Add the language to `VALID_LANGS` in `tools/search/mindex-search` (client-side `--include-lang`/`--exclude-lang` validation) and, if it has a natural source-file extension, to `ext_to_lexer()` in the same file for syntax-highlighted search output.
7. Update the table in this file's **Supported Languages** section (bump the language count) and the indexer's **Language detection** line above.
8. **Rebuild and recreate, don't just restart:** `docker compose build mindex` (the running container is a stale image otherwise) — and if step 2's `CHECK` constraint changed, the existing `mindex_db` Docker volume still has the *old* constraint baked into its already-created table (`CREATE TABLE IF NOT EXISTS` does not retroactively alter it). Per the Operational Rules below, no migration upgrade path is maintained — drop and recreate: `docker compose down && docker volume rm mindex_mindex_db && docker compose up -d`.

## Docker & CI

### Toolchain Pinning
`rust-toolchain.toml` pins `channel = "1.95"`. Key constraints from `Cargo.lock`:
- `libsqlite3-sys 0.38.0` requires rustc ≥ 1.87 (`cfg_select!` macro).
- `icu_collections 2.2.0` requires rustc ≥ 1.86.
- `cargo-chef` is **not used** — it required rustc 1.88+ but caused its own dependency conflicts. Replaced by `cargo fetch --locked` for layer caching.

### Dockerfile
Two-stage build:
```
FROM rust:1.95-bookworm AS builder
  COPY Cargo.toml Cargo.lock
  RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo fetch --locked
  COPY src ./src
  RUN cargo build --release --locked

FROM debian:bookworm-slim
  apt-get install ca-certificates openssl
  COPY --from=builder /app/target/release/mindex /usr/local/bin/mindex
  COPY scripts/entrypoint.sh /entrypoint.sh
```
`cargo fetch --locked` pre-downloads all dependencies into the image layer. When only `src/` changes, Docker reuses the fetch layer and only re-compiles. The legacy Docker builder (no BuildKit) is supported — `--mount=type=cache` is not used.

### scripts/entrypoint.sh
Generates a self-signed RSA-4096 cert at `/certs/cert.pem` + `/certs/key.pem` on first start if absent. Subsequent starts reuse the existing cert. The cert volume (`mindex_certs`) persists across container restarts.

### docker-compose.yml (production)
- Services: `qdrant/qdrant:v1.14.1` + `mindex` (built from `Dockerfile`).
- `mindex` volumes: `mindex_db:/data`, `mindex_certs:/certs`, `hf_cache:/root/.cache/huggingface` (tokenizer cache).
- `extra_hosts: ["host.docker.internal:host-gateway"]` lets the container reach the Python embedding server on the host (Linux; harmless on Docker Desktop).

### docker-compose.test.yml (standalone test stack)
Does **not** extend the base compose (to avoid host port binding conflicts). Run with:
```
docker compose -f docker-compose.test.yml up --build \
  --exit-code-from test-runner --abort-on-container-exit
```

Services and healthcheck strategy:

| Service        | Image / Build                  | Healthcheck                                      |
|----------------|-------------------------------|--------------------------------------------------|
| `qdrant`       | `qdrant/qdrant:v1.14.1`       | `bash -c 'exec 3<>/dev/tcp/localhost/6333'` — no curl in the image |
| `mock-embedder`| `tests/mock_embedder/`        | `python -c "urllib.request.urlopen(...)"` — no curl in python:3.12-slim |
| `mindex`       | `Dockerfile`                  | none (test-runner uses `wait_for_mindex` fixture) |
| `test-runner`  | `tests/integration/`          | —                                                |

`mindex` has no host port binding; `test-runner` connects via the Docker internal network (`https://mindex:11111`).

## Integration Test Suite (`tests/`)

### Mock Embedder (`tests/mock_embedder/main.py`)
FastAPI server on port 11211 implementing the same `POST /encode` contract as the real BGE-M3 server. Returns **deterministic vectors seeded by MD5 of the input text**:
- `dense`: 1024-dim unit-normalized Gaussian (`numpy` RNG seeded by `int(md5(text), 16) % 2**32`).
- `sparse`: up to 16 whitespace-split words → `{int(md5(word), 16) % 30000: 0.1 + len(word)*0.01}`.
- `colbert`: up to 8 per-token dense vectors per text.

Determinism ensures that the same text always produces the same vectors, making search ranking assertions stable across test runs.

### Test Fixtures (`tests/integration/conftest.py`)
- `wait_for_mindex` (session-scoped, `autouse=True`): polls `POST /v0/{'0'*32}/search` every 1 s for up to 120 s until mindex accepts a connection. Raises `RuntimeError` on timeout.
- `client`: `httpx.Client(verify=False, timeout=30.0)` — TLS verification disabled for the self-signed cert.
- `project`: returns `uuid4().hex` (32-char hex, no hyphens) — each test gets a fresh project GUID with no shared state.

### Test Cases (`tests/integration/test_e2e.py`)
8 end-to-end tests using the `rust` language key with a ~50-line Rust snippet large enough to produce ≥1 chunk (128–512 BGE-M3 tokens):

| Test | What it verifies |
|------|-----------------|
| `test_index_new_file_returns_chunk_count` | Index response has `chunk_count ≥ 1` for a new file |
| `test_search_finds_indexed_content` | Search returns the correct path and `process_records` in code |
| `test_reindex_unchanged_file_is_noop` | Re-indexing identical content returns `chunk_count = 0` (hash match) |
| `test_reindex_changed_file_returns_new_chunks` | Modified content produces `chunk_count ≥ 1` |
| `test_search_after_reindex_reflects_new_content` | Search surfaces v2-specific markers after re-index |
| `test_search_result_has_line_numbers` | Every result has `start_line ≥ 1`, `end_line ≥ start_line`, columns ≥ 0 |
| `test_search_empty_project_returns_404` | Search on a never-indexed project returns HTTP 404 |
| `test_multiple_files_indexed_independently` | Two files in one request each return `chunk_count ≥ 1` |

## Manual End-to-End Walkthrough

To stand up the full stack by hand and index + search a real project (e.g. mindex itself):

1. **Embedding model server must be reachable from inside the `mindex` container at `host.docker.internal:11211`.** If it runs on the host (outside Docker, e.g. a local `bge-m3-api`/FlagEmbedding process), it must bind `0.0.0.0`, not `127.0.0.1` — traffic from the container arrives via the Docker bridge gateway IP (e.g. `172.18.0.1`), not via loopback, and a server bound to `127.0.0.1` only refuses it with `ECONNREFUSED` even though the route itself is fine. (`tests/mock_embedder`'s own Dockerfile already binds `0.0.0.0`, which is why it doesn't hit this.)
2. `docker compose up -d` (qdrant + mindex). Don't assume the container picked up your latest source changes — `build: .` only rebuilds the image on an explicit `docker compose build mindex`; `up -d` alone reuses whatever image already exists.
3. Don't hardcode `https://127.0.0.1:11111` in scripts meant to be portable: ask Compose for the real published port, `docker compose port mindex 11111`, and poll it until the API responds (any of `200`/`404` to a search on an all-zero project GUID is "alive").
4. Build the indexer once: `cd tools/indexer && cargo build --release`.
5. Index with multiple languages in a single invocation — one `--include GLOB` per language is enough; the indexer groups files by extension-detected language automatically, no per-language invocation needed:
   ```bash
   tools/indexer/target/release/mindex-index \
       --project "$MINDEX_PROJECT" --root /data/silencespeakstruth/Projects/mindex \
       --include '**/*.rs' --include '**/*.py' --include '**/*.sql' \
       --exclude 'target/**' --exclude 'tools/**' --exclude '.git/**' \
       --no-verify --verbose
   ```
   If the total file count is smaller than `--batch-size` (default 100), everything goes out as a single HTTP request — the progress bar will not move at all until that one request completes (which, with a real BGE-M3 model rather than the mock, can take well over a minute). This is expected, not a hang; pass a smaller `--batch-size` for incremental progress feedback.
6. Search with `tools/search/mindex-search --project "$MINDEX_PROJECT" --no-verify --query "..."`. Results print in ascending score order (best match last); `--include-lang`/`--exclude-lang` filter by the languages indexed in step 5.

## Operational Rules
- **Data Integrity:** SQLite writes involving multiple rows must be inside a `transaction`. The soft-delete pattern ensures consistency: if the main-work transaction rolls back, old chunks remain `active` and the file status is recoverable.
- **Chunk FK is RESTRICT, not CASCADE.** Never delete a `project_files` row while its chunks exist. Always mark chunks `deleted` first (or let them be GC'd), then delete the project row.
- **Qdrant Safety:** Always call `ensure_project(collection_name(...))` before any vector operation; always use batch upsert/delete. Never delete from Qdrant in the hot indexing path — mark chunks deleted in SQLite and let the GC handle it.
- **Migration:** New schema changes go in a new SQL file added to the `MIGRATIONS` slice in `main.rs`. Migrations run inside a transaction on startup. The DB can be dropped and recreated freely; no migration upgrade path is maintained.
- **Shared `Arc<Qdrant>`:** A single `Arc<Qdrant>` instance is created in `main.rs` and shared between the HTTP server (`RouterState`) and the two workers. Do not create separate Qdrant clients per component.

## When Modifying Code
1. Any new loop touching Qdrant, SQLite, or the model server must check/respect the `CancellationToken`.
2. Any multi-row database write must be inside a `transaction`.
3. New endpoints must be registered in `backend::http3::run` and must respect `RouterState`. Route params use `{param}` syntax.
4. Always use `collection_name(project_guid.0.as_simple().to_string().as_str())` as the Qdrant collection name. Never hardcode collection names.
5. The SQLite query in any search path must include `AND c.status = 'active'` to exclude soft-deleted chunks.
6. Status transitions in `project_files` must always update `status_updated_at = unixepoch()` in the same statement.
7. When adding a language, follow the full **Language Extensibility** checklist above (8 steps: enum, schema CHECK, Cargo.toml, handlers.rs match arm, indexer scanner.rs, search CLI's `VALID_LANGS`, docs, and a Docker image rebuild + DB volume recreate) — not just the enum and match arm.
