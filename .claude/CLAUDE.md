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
- Hard-deletes the rows from SQLite after Qdrant confirms deletion.

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

## Core Technical Standards
- **Async-First:** All I/O (Qdrant, SQLite, embedding inference) must be asynchronous. Never use blocking calls directly on a Tokio thread. Embedding and Qdrant calls happen in async context; SQLite calls happen in `spawn_blocking` via `db_pool.transaction()`.
- **No `block_on` in handlers:** The old pattern of calling `tokio::runtime::Handle::current().block_on(...)` inside `spawn_blocking` for Qdrant/embed is eliminated. Qdrant and embed calls are now `.await`-ed directly in the async handler.
- **Cancellation:** Every long-running loop or I/O call must respect a `CancellationToken`. Use `tokio_util::sync::CancellationToken`.
- **`CancellationGuard`:** Handlers create a `CancellationGuard(CancellationToken::new())` on entry. This is an RAII wrapper — `Drop` calls `cancel()`, so any in-flight work is cancelled when the handler returns (including on error paths).
- **`from_cancelled`:** The `OptionResultExt` trait converts `Option<Result<T, E>>` → `Result<T, E>`, mapping `None` (returned by `with_cancellation_token` on timeout/cancel) to `Err(SQLite3PoolError::Cancelled)`.
- **Status 499:** Client-cancelled requests return HTTP 499 (`cancelled_499()`), following nginx convention.

## Error Handling
- Domain error types: `SQLite3PoolError`, `SlicerError`, `EncodeError`.
- All custom errors must be convertible to HTTP status codes.
- Avoid external error-handling crates. Follow the existing `slicer_err_to_pool_err` and `from_cancelled` patterns.
- `slicer_err_to_pool_err` maps `SlicerError::Cancelled` → `SQLite3PoolError::Cancelled` (not HTTPStatusCode) so the downstream match arm can distinguish cancellation from other errors cleanly.
- Propagate errors with `?`. No `unwrap()` or `expect()` in production code paths (workers use `.unwrap_or_default()` only for best-effort queries where empty fallback is safe).

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

## SlicedChunk Fields
```rust
pub struct SlicedChunk {
    pub code: String,        // source text from start of node's line to end_byte (includes leading whitespace)
    pub start_byte: usize,   // node.start_byte() (not line_start)
    pub end_byte: usize,
    pub start_line: usize,   // 1-indexed
    pub end_line: usize,     // 1-indexed
    pub start_column: usize, // byte offset of the node within its start line
    pub end_column: usize,   // byte offset of the exclusive end within its end line
}
```
Leading whitespace: `code` is extended to the start of the node's line only when the intervening bytes are pure whitespace (indentation). Mid-line nodes are not extended.

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

**Language detection** (extension → API key): `.rs`→rust, `.py`→python, `.js/.mjs/.cjs/.jsx`→javascript, `.ts/.mts/.cts`→typescript, `.tsx`→tsx, `.go`→go, `.c/.h`→c, `.cpp/.cc/.cxx/.hpp`→cpp, `.java`→java, `.cs`→csharp, `.rb`→ruby, `.php`→php, `.sh/.bash`→bash, `.html/.htm`→html, `.css`→css, `.json`→json, `.scala/.sc`→scala, `.hs/.lhs`→haskell, `.ml/.mli`→ocaml, `.zig`→zig.

## Supported Languages

20 languages supported via tree-sitter grammars. All crates must require `tree-sitter ≥ 0.23` (the new `LanguageFn`/`LANGUAGE` constant API). Older crates cause a native `links` conflict and cannot coexist with `tree-sitter 0.26`.

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

**Not yet supported** — crates still require `tree-sitter 0.21–0.22`, causing a native `links` conflict:
`yaml`, `toml`, `kotlin`, `lua`, `elixir`, `nix`, `erlang`, `swift`.
Add them once the upstream crates are updated to the `0.23+` API.

## Language Extensibility
When adding a new `ProgrammingLanguage`:
1. Add the variant to the `ProgrammingLanguage` enum in `models.rs` and its `ToSql`/`FromSql` impls. Use a lowercase SQL name matching the serde rename.
2. Add the SQL name to the `CHECK` constraint in `v0.1.0_schema.sql`.
3. Add the `tree-sitter-<lang>` crate to `Cargo.toml`. **Verify** its `tree-sitter` dependency is `≥ 0.23`; older versions cause a `links` conflict.
4. Add an arm to the `let ts_language = match pl { ... }` block in the main-work tx closure in `post_index`. Use `Language::new(tree_sitter_<lang>::LANGUAGE)` for crates that export a plain `LANGUAGE` constant, or the crate-specific name (e.g. `LANGUAGE_TYPESCRIPT`, `LANGUAGE_OCAML`).

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
7. When adding a language: update `models.rs`, `v0.1.0_schema.sql`, `Cargo.toml`, and the `let ts_language = match pl` block in `handlers.rs`. Verify the new tree-sitter crate requires `tree-sitter ≥ 0.23` before adding.
