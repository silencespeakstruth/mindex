# CLAUDE.md: mindex Architecture & Standards

## Project Overview
`mindex` is a high-performance, asynchronous RAG indexing and search engine built in Rust. It exposes an HTTPS API that indexes source code files using `tree-sitter` for semantic AST chunking, `BGE-M3` for multi-vector embeddings (dense, sparse, ColBERT), `Qdrant` for vector storage, and `SQLite3` for relational metadata. It is an **internal service** — TLS is the only transport-layer security; no API authentication is implemented or planned.

## Module Map
```
src/
  main.rs                   — CLI (clap), startup, migration runner, signal handling
  backend/
    http3.rs                — RouterState, EmbeddingModel enum, CancellationGuard, run()
    v0/
      handlers.rs           — post_index, post_search
      models.rs             — request/response types, ProgrammingLanguage, UUIDv4, GlobPattern
  db/
    sqlite3.rs              — SQLite3Pool, SQLite3PoolError
    qdrant.rs               — ensure_collection, insert_batch, delete_batch, search
    migrations/
      v0.1.0_schema.sql     — projects, project_files, project_file_chunks tables
  models/
    bge_m3.rs               — BGEm3HttpClient, BGEm3Model trait, EncodeError
  slicing/
    traits.rs               — Slicer, SlicedChunk, SlicerError
```

## HTTP Server
- Framework: `axum` + `axum-server` with `rustls` (TLS 1.2/1.3).
- HTTP/3 support is **in-progress** — the `Protocol::Http3` CLI variant and `http3.rs` module name are forward-looking; the current `run()` function uses HTTP/1.1 + HTTP/2 only.
- Default bind: `127.0.0.1:11111`. Default TLS certs: `cert.pem` / `key.pem`.
- Routes: `POST /v0/:project_guid/index`, `POST /v0/:project_guid/search`.
- All route handlers must use `#[debug_handler]`. Maintain strict separation between `State<RouterState>` and `Path<T>` extractors.

## Qdrant Architecture (Critical)
Qdrant uses **one collection per project**, named by the project's UUID in simple (no-hyphen) form (`project_guid.as_simple()`). Within each collection, chunk-level discrimination is done via a `has_id` filter populated from SQLite.

Two-layer isolation:
1. **Collection = project.** Every `ensure_project`, `insert_batch`, `delete_batch`, and `search` call must use `project_guid.0.as_simple().to_string()` as the collection name.
2. **`has_id` filter = SQLite-derived chunk GUIDs.** Before calling Qdrant, SQLite is queried to collect all `qdrant_guid` values that match the project + any additional filters (language, path GLOBs). Those GUIDs are passed as the Qdrant `has_id` filter, narrowing the vector search to the relevant subset.

- **Collection schema** (three named vectors per collection):
  - `dense`: 1024-dim cosine
  - `sparse`: sparse vector (SPLADE-style)
  - `colbert`: 1024-dim cosine, multivector MaxSim
- **Search pipeline**: prefetch top-200 dense + top-200 sparse → RRF fusion → ColBERT MaxSim reranking → top-k.
- **Sparse threshold**: filter out sparse weights `< 1e-5` before sending to Qdrant.
- **Batch sizes**: 64 chunks per embedding call, 256 points per Qdrant upsert/delete.

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

Transactions run on `tokio::task::spawn_blocking` threads.

## Core Technical Standards
- **Async-First:** All I/O (Qdrant, SQLite, embedding inference) must be asynchronous. Never use blocking calls directly on a Tokio thread.
- **Cancellation:** Every long-running loop or I/O call must respect a `CancellationToken`. Use `tokio_util::sync::CancellationToken`.
- **`CancellationGuard`:** Handlers create a `CancellationGuard(CancellationToken::new())` on entry. This is an RAII wrapper — `Drop` calls `cancel()`, so any in-flight work is cancelled when the handler returns (including on error paths).
- **`from_cancelled`:** The `OptionResultExt` trait converts `Option<Result<T, E>>` → `Result<T, E>`, mapping `None` (returned by `with_cancellation_token` on timeout/cancel) to `Err(SQLite3PoolError::Cancelled)`.
- **Status 499:** Client-cancelled requests return HTTP 499 (`cancelled_499()`), following nginx convention.

## Error Handling
- Domain error types: `SQLite3PoolError`, `SlicerError`, `EncodeError`.
- All custom errors must be convertible to HTTP status codes.
- Avoid external error-handling crates. Follow the existing `handle_slicer_error` and `from_cancelled` patterns.
- Propagate errors with `?`. No `unwrap()` or `expect()` in production code paths.

## BGE-M3 & Retrieval Pipeline
- **Indexing:**
  - Hash file content with `Sha256`; skip re-indexing if the hash matches what is stored in SQLite.
  - On hash mismatch: delete the old Qdrant vectors and SQLite rows before re-inserting.
  - Batch embedding calls at 64 chunks; batch Qdrant upserts/deletes at 256 points.
- **Retrieval:**
  - Filter at the SQLite level first (project, language, path GLOB) to get the candidate `qdrant_guid` set.
  - Pass those GUIDs as a `has_id` filter to Qdrant — this is the sole project isolation mechanism.
  - Multi-vector (dense/sparse/colbert) alignment: each vector list from the embed response is positionally aligned with the chunk list.

## Slicer
`Slicer` (`slicing/traits.rs`) traverses the tree-sitter AST depth-first and selects **named nodes** whose token span (measured against the HuggingFace tokenizer) falls in the range **128–512 tokens** — the range where BGE-M3 performs best. Token boundaries do not align with AST node boundaries; the tokenizer is context-dependent.

## Language Extensibility
Any language with a tree-sitter grammar can be supported. When adding a new `ProgrammingLanguage`:
1. Add the variant to the `ProgrammingLanguage` enum in `models.rs` and its `ToSql`/`FromSql` impls.
2. Add it to the SQLite `CHECK` constraint in the migration SQL.
3. Add the `tree-sitter-<lang>` crate to `Cargo.toml`.
4. Map the variant to `tree_sitter_<lang>::LANGUAGE` in the `post_index` handler.
5. Add the `cargo-add` dependency to `Cargo.toml`.

## Operational Rules
- **Data Integrity:** SQLite writes involving multiple rows must be inside a `transaction`. If a batch fails, the index state must remain consistent (cascade deletes handle this via FK constraints).
- **Qdrant Safety:** Always call `ensure_collection("chunks", ...)` before any vector operation; always use batch upsert/delete.
- **Migration:** New schema changes go in a new SQL file added to the `MIGRATIONS` slice in `main.rs`. Migrations run inside a transaction on startup.

## When Modifying Code
1. Any new loop touching Qdrant, SQLite, or the model server must check/respect the `CancellationToken`.
2. Any multi-row database write must be inside a `transaction`.
3. New endpoints must be registered in `backend::http3::run` and must respect `RouterState`.
4. Always pass `"chunks"` as the Qdrant collection name — never create or target per-project collections.
