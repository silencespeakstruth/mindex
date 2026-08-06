# Retrieval v3 — Qwen3-Embedding via vLLM, the model registry, and everything that dies on the way

*Implementation spec, approved 2026-08-05. Supersedes the implementation half of
`retrieval-v2.md` (whose **measurement record stands** and is the evidence base
here); read that file's §1/§6 for the numbers and the open questions. This file
is written to be executed by a mid-tier model without further research: every
phase names its files, signatures, SQL and tests. Execute phases 1–5 as one
series — the tree does not compile between them — then run the full suite.*

*Sanctioned up front (owner's decision, do not relitigate): destructive restart
of the SQLite lineage, full reindex of every project, retraction of prior
releases, deletion of ColBERT **and** sparse, and net shrinkage of the codebase
as a hard requirement.*

---

## 0. The decisions and their grounds

| decision | ground |
|---|---|
| Single dense leg; ColBERT deleted | never measured to help (+0.0065, p=0.30) for 99.6% of storage and 84% of query latency (`bench/FINDINGS.md` §7a, §2.7) |
| Sparse deleted too | worth ~+0.004 over a good dense leg, CI through zero, bounded below δ=0.01; plain RRF scores **below** the single leg it fuses (0.4164 vs 0.4448 django) — `retrieval-v2.md` §1 |
| Model family: **Qwen3-Embedding**, not granite | statistically indistinguishable from granite on both corpora (0.4540 vs 0.4448 django, p=0.20); chosen over granite for multilinguality (the owner's queries are often Russian — `retrieval-v2.md` §6 names this the untested case) and for the size ladder (0.6B/4B/8B, one tokenizer) |
| All three sizes registered day one, operator-selectable | switching sizes must be a re-embed, never a re-slice; nothing anywhere may hard-code one dim |
| Served by **vLLM** (OpenAI-compatible API) | the vendored `embedder/` server existed only because no general server returned three heads (`embedder/README.md` said so); with one head it is deletable, as designed |
| Chunk window default max **364** | +0.0108 nDCG@10, p=0.030 (`bench/FINDINGS.md` §2.5); measured under the BGE tokenizer — carried as best-available default, re-confirmed by the bench re-run |
| Canonical model ids in SQLite with CHECK | owner requirement: future model additions cheap, maximally safe, identity enforced at the database |

Out of scope this iteration: the `/research` Ollama loop (untouched), sparse
re-introduction (per-model named collections leave it a cheap slot), symbols,
git history.

---

## 1. Phase 0 — the model registry (`src/models/registry.rs`, new)

```rust
/// One embedding model mindex can be configured to run. Compiled in, and
/// cross-checked against the `embedding_models` table at startup.
pub struct EmbeddingModelSpec {
    /// Canonical id: what config, SQLite, metrics and GET /config all carry.
    pub id: &'static str,
    /// HF repo the serving side loads; the handshake expects it in /v1/models.
    pub hf_repo: &'static str,
    /// Dense width. Validated against EVERY /v1/embeddings response row.
    pub dim: usize,
    /// Model context limit (32768 for all three).
    pub max_seq: usize,
    /// Prepended to QUERIES only. Documents get nothing.
    pub query_prefix: &'static str,
    /// Qdrant-name-safe short slug for collection names.
    pub collection_slug: &'static str,
    /// Tokenizer the slicer loads. Identical for all three sizes — which is
    /// what makes switching sizes a re-embed, never a re-slice.
    pub tokenizer_hf_id: &'static str,
}

pub const QWEN3_QUERY_PREFIX: &str =
    "Instruct: Given a description of desired functionality, retrieve the source code that implements it\nQuery: ";
// Byte-for-byte the string in bench/baselines/external_embedder.py:85-88.
// The archived 0.4540 was measured WITH it; a drifted prefix degrades silently.

pub const EMBEDDING_MODELS: &[EmbeddingModelSpec] = &[
    EmbeddingModelSpec { id: "qwen3-embedding-0.6b", hf_repo: "Qwen/Qwen3-Embedding-0.6B",
        dim: 1024, max_seq: 32768, query_prefix: QWEN3_QUERY_PREFIX,
        collection_slug: "q3e06b", tokenizer_hf_id: "Qwen/Qwen3-Embedding-0.6B" },
    EmbeddingModelSpec { id: "qwen3-embedding-4b",   hf_repo: "Qwen/Qwen3-Embedding-4B",
        dim: 2560, max_seq: 32768, query_prefix: QWEN3_QUERY_PREFIX,
        collection_slug: "q3e4b",  tokenizer_hf_id: "Qwen/Qwen3-Embedding-0.6B" },
    EmbeddingModelSpec { id: "qwen3-embedding-8b",   hf_repo: "Qwen/Qwen3-Embedding-8B",
        dim: 4096, max_seq: 32768, query_prefix: QWEN3_QUERY_PREFIX,
        collection_slug: "q3e8b",  tokenizer_hf_id: "Qwen/Qwen3-Embedding-0.6B" },
];

pub fn model_by_id(id: &str) -> Option<&'static EmbeddingModelSpec>;
pub fn model_by_slug(slug: &str) -> Option<&'static EmbeddingModelSpec>;
```

Register in `src/models/mod.rs`. In-module tests: ids unique, slugs unique,
slugs match `^[a-z0-9]{1,16}$`, dims are exactly {1024, 2560, 4096}, all three
share `tokenizer_hf_id`, `QWEN3_QUERY_PREFIX` pinned byte-for-byte.

---

## 2. Phase 1 — schema: `v2.0.0_schema.sql`, lineage refusal, `model_id` surgery

**Delete all six `src/db/migrations/v1.*.sql`.** New single baseline
`src/db/migrations/v2.0.0_schema.sql` = the current effective schema with
v1.1–v1.4 folded in (git-history tables, widened language CHECK, research
context/verification columns including `embedder_model_id`, definitions-only
symbols), minus `model_id` everywhere, plus the additions below. Every
statement `IF NOT EXISTS` / `INSERT OR IGNORE` — `every_migration_sql_is_idempotent`
(`src/main.rs:1277`) runs the batch twice and must pass over the new list.

### 2a. New objects

```sql
PRAGMA application_id = 0x4D583033;   -- 'MX03'. Old-lineage databases carry 0.

CREATE TABLE IF NOT EXISTS embedding_models (
    id  TEXT PRIMARY KEY CHECK (id IN
        ('qwen3-embedding-0.6b','qwen3-embedding-4b','qwen3-embedding-8b')),
    dim INTEGER NOT NULL CHECK (dim > 0)
);
INSERT OR IGNORE INTO embedding_models (id, dim) VALUES
    ('qwen3-embedding-0.6b', 1024),
    ('qwen3-embedding-4b',   2560),
    ('qwen3-embedding-8b',   4096);
-- Append-only: rows are facts about what vectors may exist. A future model =
-- a migration widening the CHECK (small-table rebuild, rule 8) + an INSERT +
-- a Rust registry entry, in one commit.
CREATE TRIGGER IF NOT EXISTS embedding_models_no_update
BEFORE UPDATE ON embedding_models
BEGIN SELECT RAISE(ABORT, 'embedding_models is append-only'); END;
CREATE TRIGGER IF NOT EXISTS embedding_models_no_delete
BEFORE DELETE ON embedding_models
BEGIN SELECT RAISE(ABORT, 'embedding_models is append-only'); END;
```

### 2b. Table changes (relative to today's effective schema)

- `projects` → `(guid TEXT PRIMARY KEY CHECK (length(guid) = 32))`. No model_id.
- `project_files`: PK `(project_guid, path)`; FK `project_guid → projects(guid)
  ON DELETE CASCADE`; keeps the status machine, all eight triggers, the path /
  sha256 / language / status CHECKs verbatim (minus model_id columns in trigger
  bodies and the status log). Gains two columns beside `chunks_version`:
  - `chunker_id TEXT` — the tokenizer identity (`spec.tokenizer_hf_id`).
    Deliberately **not** the window: the doctrine at `src/slicing/traits.rs:14`
    stands — the window is config, and folding it in would price every window
    experiment at a corpus-wide re-slice.
  - `embedded_model_id TEXT REFERENCES embedding_models(id)` — whose vectors
    exist for this file. Nullable; NULL never matches (the derivation-version
    self-heal shape).
- `project_file_chunks`: gains `tokens INTEGER NOT NULL CHECK (tokens > 0)`
  (blast-radius queryability — "which chunks exceed model X's window" becomes
  SQL) and `UNIQUE` on `qdrant_guid` (replaces the plain lookup index). FK
  `(project_guid, file_path) → project_files` `ON DELETE RESTRICT` as today.
- `project_file_status_log`, `project_file_symbols`, `project_commits`,
  `project_commit_paths`, all four `research_run_*` tables, `research_runs`:
  identical minus `model_id`. **Keep `research_runs.embedder_model_id`** — it
  now records a canonical registry id and finally means something.
- All indexes lose their `model_id` component.

### 2c. `src/main.rs`: MIGRATIONS restart + lineage refusal + registry check

```rust
pub(crate) const MIGRATIONS: &[(i32, &str)] =
    &[(1, include_str!("db/migrations/v2.0.0_schema.sql"))];
pub(crate) const APPLICATION_ID: i32 = 0x4D58_3033;
```

In the migration transaction, **before** `apply_pending_migrations`:

```rust
let app_id: i32 = tx.pragma_query_value(None, "application_id", |r| r.get(0))?;
let user_version: i32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
if app_id != APPLICATION_ID && user_version > 0 {
    return Err(/* fatal, naming the path: "this database predates the v2
        retrieval schema (application_id {app_id:#x}, schema {user_version})
        and cannot be migrated. Delete it (and its -wal/-shm) plus the old
        Qdrant collections, restart, and reindex every project
        (mindex-index --force)." */);
}
```

A fresh DB has both pragmas 0 and proceeds; the baseline stamps
`application_id`. **After** migrations, same startup path:
`verify_model_registry(tx)` — `SELECT id, dim FROM embedding_models` must
biject with `EMBEDDING_MODELS` with equal dims; any mismatch is fatal, naming
both sides ("the registry says qwen3-embedding-4b is 2560-d, the database says
3000"). This is the guard against a rebuilt binary silently reinterpreting
stored vectors.

### 2d. `model_id` surgery (mechanical, wide)

- `IndexClaim` key drops to `{guid}\0{path}` (`indexing_lock_key` loses the
  model argument).
- `file_already_indexed` (`handlers.rs:~446`) — the currency predicate becomes:

  ```sql
  ... WHERE project_guid = ?1 AND path = ?2 AND status = 'indexed'
        AND chunks_version    = ?3   -- CHUNKS_DERIVATION_VERSION
        AND chunker_id        = ?4   -- spec.tokenizer_hf_id
        AND embedded_model_id = ?5   -- spec.id
        AND symbols_version   = ?6
  ```

  Flipping `[model].id` in config therefore self-heals through the existing
  skip logic, exactly like a derivation-version bump.
- `MARK_INDEXING_UPSERT_SQL` (`handlers.rs:480-488`): drop `model_id`; conflict
  target `(project_guid, path)`; add `chunker_id` and `embedded_model_id` to
  the INSERT columns and the `DO UPDATE SET` list. The full-index path stamps
  both in the prepare transaction (same guarantee as today: a row can never
  claim an identity whose rows were not produced in-tx). The `symbols_only`
  path must not touch either (mirror how it already skips `chunks_version`).
- The chunk INSERT gains `tokens` (from Phase 4's `SlicedChunk.tokens`).
- `worker/retry.rs` (~:280-470): drop the binds; the terminal `indexed` write
  also sets `embedded_model_id` to the active id (the worker re-embeds into the
  active model's collection, so the row must say so).
- `worker/gc.rs`, `worker/stale.rs`, git-history handlers, `/stats`, `/files`
  listings, and every test seed: drop the column.

New tests: `an_old_lineage_database_is_refused`,
`a_dim_mismatch_between_registry_and_db_refuses_startup`,
`embedding_models_rows_cannot_be_updated_or_deleted`.

---

## 3. Phase 2 — the embedder client (`src/models/embedder.rs` replaces `src/models/bge_m3.rs`)

### 3a. Trait and client

```rust
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: Vec<String>, token: CancellationToken)
        -> Result<Vec<Vec<f32>>, EncodeError>;
    /// GET {base}/health — vLLM serves one. Bounded by health_timeout_ms.
    async fn health(&self) -> Result<(), EncodeError>;
    /// GET {base}/v1/models — the served model ids, for the handshake.
    async fn served_models(&self) -> Result<Vec<String>, EncodeError>;
}

pub enum EncodeError { Cancelled, Request(reqwest::Error),
                       Timeout(Duration), Decode(String) }   // variants unchanged

pub struct OpenAiEmbedClient {
    client: reqwest::Client,
    base_url: Url,
    /// The "model" field of every request and what the handshake expects in
    /// /v1/models. Default spec.hf_repo; overridable by [model].served_name
    /// (a vLLM started with --served-model-name).
    served_name: String,
    /// spec.dim — every response row is checked against it.
    expected_dim: usize,
    // tuning: max_429_retries (now also retries 503), backoff_base,
    // health_timeout, encode_timeout; retries_429: Option<Counter>.
}
```

- `embed`: POST `{base}/v1/embeddings`, body
  `{"model": served_name, "input": texts, "encoding_format": "float"}`.
  **Port verbatim** the whole-call-deadline loop from `bge_m3.rs:352-411`
  (single deadline for the call, per-attempt `remaining` as the reqwest
  timeout, backoff `(base * 2^attempt).min(remaining)`, cancellation
  `select!`s at send / sleep / body-read). The retry condition widens from
  `429` to `429 || 503` — vLLM emits both under load.
- Decode: parse `{"data":[{"index":n,"embedding":[...]}]}`; **sort rows by
  `index`** (defensive); `data.len() == texts.len()` else `Decode`; **every**
  `embedding.len() == expected_dim` else `Decode` naming both dims and the
  served model ("row 3 is 768-d, qwen3-embedding-0.6b is 1024-d — the server
  is serving a different model than [model].id names"). `ENCODE_MAGIC`, the
  binary `Reader`, `parse_encode_response`, `checked_capacity` all die (serde
  bounds allocations now).
- `MeteredEmbedder` decorator ports verbatim (`bge_m3.rs:276-334`): same
  labels, same outcome mapping (`decode` now means JSON/shape/dim mismatch),
  same `embedder = "index" | "query"` roles, `with_metrics` builder.

### 3b. `http3.rs`: kill the enum

```rust
// was: pub enum EmbeddingModel { BGEm3 { model_id: String, client: Arc<dyn BGEm3Model> } }
pub struct ActiveModel {
    pub spec: &'static EmbeddingModelSpec,
    pub client: Arc<dyn Embedder>,
}
// RouterState.model: ActiveModel;  RouterState.query_model: Arc<dyn Embedder>
```

~25 destructure sites (grep `EmbeddingModel::BGEm3`; known: `main.rs:803`,
`handlers.rs` 1403, 1988, 2263, 2812, 2969, 3089, 3204, 3450, 3651, 3726,
3800, 4121, 4973, 5232, 6355, 7039, 7233, 7380, 7626, 8227, 8234, plus test
modules). Pattern: `let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;` →
`let model_id = s.model.spec.id;`.

### 3c. `main.rs` wiring + startup handshake

Resolve `let spec = registry::model_by_id(&cfg.model.id)` (config already
validated it). Build index + query clients, wrapped in `MeteredEmbedder`.
Call `served_models()` once per client at startup:

- Unreachable / timeout → `warn!` and **continue** (a down embedder at startup
  is legitimate; files fail and the retry worker recovers — today's story).
- Reachable but the expected name absent → **refuse to start**, naming found
  vs expected. Rationale: the dim check catches a wrong-family model, but a
  wrong model with a coincidental dim would poison every vector silently; a
  server that answered can be interrogated, so answering wrong is
  misconfiguration, not outage.

`GET /health` embedder probes become `health()` **and** the handshake match; a
mismatch reports the component as `error` (shape of `checks.*` unchanged).

### 3d. `tests/mock_embedder/main.py` — OpenAI-format rewrite

```
GET  /health         -> {"status":"ok"}
GET  /v1/models      -> {"object":"list","data":[{"id": MOCK_MODEL,"object":"model"}]}
POST /v1/embeddings  -> {"object":"list","model": MOCK_MODEL,
                         "data":[{"object":"embedding","index":i,"embedding":[...]}]}
POST /config         -> keep encode_delay_secs / fail_next_encodes knobs verbatim
```

Keep `_dense()` (md5-seeded gauss, normalized) **exactly** — integration tests
rely on stable deterministic ranking. `MOCK_DIM` (default 1024) and
`MOCK_MODEL` (default `Qwen/Qwen3-Embedding-0.6B`) from env.
`_sparse`/`_colbert`/`_pack`/`_MAGIC` deleted. `docker-compose.test.yml` sets
the env vars; both test configs get `[model] id = "qwen3-embedding-0.6b"`.

Client tests port to JSON stubs (429-retry-then-success, give-up count,
cancellation, wedged-server timeout, whole-call budget, backoff clamp); add:
`a_503_is_retried_like_a_429`, `a_wrong_dim_row_is_a_decode_error_naming_both_dims`,
`rows_are_reordered_by_index`, `a_short_response_is_a_decode_error`.

---

## 4. Phase 3 — Qdrant: per-model collections, dense-only search

`src/db/qdrant.rs`, `src/db/qdrant_metrics.rs`, `src/worker/stale.rs`.

### 4a. Names and classification

```rust
const COLLECTION_SCHEMA_VERSION: &str = "v3";

/// {guid_simple}_{collection_slug}_{version}, e.g.
/// 2f1c9a704d3e4e9b8b6a1f2e3d4c5b6a_q3e06b_v3
pub fn collection_name(project_guid_simple: &str, slug: &str) -> String;
pub fn collection_for(project_guid: UUIDv4, spec: &EmbeddingModelSpec) -> String;

pub enum CollectionAge {
    Current,     // this guid shape, ACTIVE model's slug, v3
    OtherModel,  // a REGISTERED slug != active, v3. A deliberate per-model
                 // store — never named in any "drop this" message.
    Previous,    // mindex grammar at an old layout: legacy {guid}_v1/_v2,
                 // and {guid}_{registered-slug}_{vN != v3}
    Foreign,     // everything else, incl. unregistered slugs (Qdrant may be shared)
}
pub fn classify_collection(name: &str, active_slug: &str) -> CollectionAge;
```

Parsing (pin with a test matrix): 32 lowercase hex + `_`; remainder either
`v<digits>` alone (legacy two-part grammar → `Previous`) or
`{slug}_{version}` where the slug must be **registered** (else `Foreign`) and
the version `v3` → `Current`/`OtherModel` by slug, else `Previous`.

`worker/stale.rs`: `check_once` builds names with the active spec's slug; the
orphan gauge counts `Previous` only; `OtherModel` collections are listed at
`info!` ("held for a registered model that is not active; switching
`[model].id` back reuses them").

### 4b. Store

```rust
pub struct QdrantStore { client: Qdrant, dim: u64, search_hnsw_ef: u64 }

pub struct ChunkAsVector { pub guid: UUIDv4, pub dense: Vec<f32> }
// -> PointStruct with ONE named vector "dense". Named, not default: the cheap
//    slot a future sparse leg re-enters through without a grammar change.

// VectorStore::search narrows to:
async fn search(&self, collection: &str, chunk_ids: Vec<UUIDv4>,
                dense: Vec<f32>, top_k: u64)
    -> Result<Vec<SearchHit>, VectorStoreError>;
```

- `ensure_project`: one named vector, `VectorParamsBuilder::new(self.dim,
  Distance::Cosine)`, Qdrant-default HNSW. No sparse config, no multivector,
  no fp16/on_disk/m=0 overrides.
- `search`: a single `QueryPointsBuilder::new(collection).query(dense)
  .using("dense").filter(has_id).limit(top_k)
  .params(hnsw_ef = self.search_hnsw_ef).with_payload(false)
  .with_vectors(false)`. The nested prefetch tree, `Fusion::Rrf` and the
  ColBERT outer query all go, along with `VECTOR_DIM`, `COLBERT_DATATYPE`,
  `COLBERT_HNSW_M` and both `#[allow(clippy::too_many_arguments)]`.
- Keep verbatim: `delete_batch`'s missing-collection-is-confirmation logic,
  `ensure_project`'s "already exists" race guard, `count_points` /
  `list_collections` decline semantics.
- **Project delete** (`DELETE /projects/{guid}`): enumerate
  `list_collections()` and drop every name whose 32-hex prefix is this guid
  and whose classification is not `Foreign` (all models, all versions — the
  project is gone). If listing fails/declines: fall back to dropping the
  active collection, `warn!`.
- **GC note (document, don't build):** GC confirms vector deletion against the
  *active* collection only. Chunks deleted while model A is active leave dead
  points in model B's collection; they are unreachable by search (the
  candidate `has_id` set comes from SQLite) and cost disk only; a `--force`
  reindex or a collection drop reclaims them. State this in
  `docs/claude/qdrant.md`.
- **Keep `rank_by_score` + `search_unscorable_winners`** (`handlers.rs:~2623`):
  a split index/query deployment can still disagree about model or precision;
  NaN defense stays, still expected to read zero. Update its doc text (drop
  the XPU-attention sentence; keep "check both instances serve the same model
  at the same precision").

Tests: naming grammar + length, pinned literal `"v3"` (failure message: v3
runbook — reindex, then drop `_v2` leftovers), the classification matrix
(legacy `_v2` → `Previous`, unregistered slug → `Foreign`, other registered
slug → `OtherModel`), and `distinct_project_model_pairs_never_share_a_collection`.

---

## 5. Phase 4 — slicer

`src/slicing/traits.rs`, `src/slicing/markdown.rs`, `src/config.rs`.

- Delete the whole ColBERT ceiling chain: `SPECIAL_TOKENS`,
  `STORABLE_TOKENS_CEILING`, `RETOKENIZATION_SLACK`, `MAX_STORABLE_TOKENS`;
  both constructors stop min-clamping (`max_tokens` taken plainly), and
  `markdown.rs:129`'s truncation ceiling goes with them. The token-boundary
  cutting mechanism itself **stays** — an over-window chunk is still a quality
  defect, and a minified one-liner still needs a cut the tokenizer reported.
- `SlicedChunk` gains `pub tokens: usize` — **persisted now**, not
  `#[cfg(test)]`. AST chunks: the token count after left-extension (recompute
  via `partition_point` over the final `code_start`); gap chunks: the window's
  own accounting; `markdown.rs`'s chunk type gains the same field from its DP
  cost table. The chunk INSERT writes it.
- Tokenizer: `main.rs:800` → `Tokenizer::from_pretrained(spec.tokenizer_hf_id,
  None)`. (Still an HF Hub download at startup — pre-cache on offline hosts.)
- Config: `DEFAULT_MAX_CHUNK_TOKENS = 364` (cite the bench numbers in the
  comment); `DEFAULT_MIN_CHUNK_TOKENS` stays 128. Delete `MODEL_MAX_TOKENS`
  and `MODEL_INPUT_LIMIT_TOKENS`; validation becomes `max_chunk_tokens <=
  spec.max_seq` and `max_doc_chunk_tokens <= spec.max_seq`, with the spec
  resolved from `[model].id` (an unknown id is its own validation error
  listing the registry). `max_doc_chunk_tokens` default 1024, now genuinely
  free of the silent 1020 clamp.
- Tests: `Tokenizer::from_pretrained("Qwen/Qwen3-Embedding-0.6B", ...)` at
  `traits.rs:536` and `markdown.rs:555`; window fixtures 128/364;
  `chunks_satisfy_token_window` keeps `WINDOW_SLACK` and re-encodes with the
  Qwen tokenizer. Coverage fixtures (`INDENTED_FIXTURE`, the gap-fill
  percentages) may shift a few percent under the new tokenizer — adjust
  fixtures, not thresholds, unless coverage genuinely regresses.

---

## 6. Phase 5 — `embed.rs`, the search path, `vectors_only`

### 6a. `embed.rs`

```rust
pub struct EmbedTuning { pub embed_batch: usize, pub upsert_batch: usize }
// sparse_min_weight gone

pub async fn embed_and_upsert(
    embedder: &dyn Embedder, store: &dyn VectorStore, collection: &str,
    chunks: &[(UUIDv4, String)], token: &CancellationToken,
    tuning: EmbedTuning, progress: Option<&(dyn Fn(EmbedProgress) + Send + Sync)>,
) -> Result<(), EmbedUpsertError>
```

One head; **keep** the row-count check (the client checks too, but this
function is also fed by test fakes); `ChunkAsVector { guid, dense }`; upsert
batches and the progress callback unchanged. All fakes narrow to dense; the
seven existing tests survive with smaller fixtures.

### 6b. Search path (`handlers.rs:~2377-2510`)

```rust
let query_text = format!("{}{}", state.model.spec.query_prefix, payload.query);
let rows = state.query_model.embed(vec![query_text], token.clone()).await?;
let dense = rows.into_iter().next().ok_or(ApiError::EmbedderUnavailable)?;
let hits = state.qdrant.search(&collection_for(project_guid, state.model.spec),
                               candidate_ids, dense, top_k).await?;
```

This is the **only** call site that applies `query_prefix`. Documents — the
indexing batches, the markdown slicer's per-block embed, the retry worker —
send raw text. Research inherits the prefix through `search_core`, the single
query-embedding site. Regression test: a recording fake behind `router_state()`
asserts the `/search` query text starts with `QWEN3_QUERY_PREFIX` and that
indexing batches do **not**.

### 6c. `vectors_only` — the cheap model-switch path

Follows the `symbols_only` precedent end to end.

- `IndexRequest` gains `#[serde(default)] pub vectors_only: bool`; combining
  with `symbols_only` is a 400 (new validation code + `codes_are_stable`
  update — the one place the snapshot changes, do it deliberately).
- Handler branch per file, under the same `IndexClaim`:
  1. Row already `indexed` with `embedded_model_id = spec.id` **and**
     `chunker_id = spec.tokenizer_hf_id` → emit `skipped`.
  2. `chunker_id` differs → per-file error event: "stored chunks were sliced
     under a different tokenizer; run a full reindex" (reusing them would be
     wrong, not merely stale).
  3. Otherwise: mark `indexing`, read active `(qdrant_guid, code)` rows (the
     exact `retry.rs:370-391` query minus model_id), `ensure_project` on the
     **active** model's collection, `embed_and_upsert`, then the terminal tx
     sets `status='indexed', embedded_model_id = spec.id` — sha256,
     chunks_version, symbols untouched. No tree walk, no slicing, no symbols:
     a pure GPU pass over stored rows.
- `mindex-index --vectors-only` mirrors `--symbols-only`; composable with
  `--force` (which simply defeats step 1's skip).
- SSE: `started` gains `vectors_only` beside `symbols_only`; update
  `index_event_names_are_stable` + `index_event_data_names_its_fields_on_the_wire`.

**Checkpoint: full `cargo test` green against the rewritten mock.**

---

## 7. Phase 6 — config surface, observability, clients

- `[model]`: `name` → **`id`**, `DEFAULT_MODEL_ID = "qwen3-embedding-0.6b"`,
  validated against the registry (the error lists valid ids); new optional
  `served_name`; `server_url` default `http://localhost:11212`; keep the
  `query_server_url` seam and all timeouts; `max_429_retries` docstring now
  covers 503. CLI `--model` maps to `model.id`.
- `[qdrant]`: delete `dense_prefetch_limit` / `sparse_prefetch_limit` /
  `fusion_limit` and their validation chain (`config.rs:~1740-1786`); keep
  `search_hnsw_ef` with one successor rule: `search_hnsw_ef >= max_top_k`.
- `[indexing]`: delete `sparse_min_weight` + its validation.
- `config.example.toml` rewritten; CHANGELOG lists every removed/renamed key —
  `deny_unknown_fields` makes stale configs fail loudly at startup, which is
  intended.
- Metrics: families unchanged (`metric_names_are_stable` must pass untouched);
  `BuildLabels.model_id` fixtures (`metrics.rs:1701`, `:1916`) →
  `"qwen3-embedding-0.6b"`; `mindex_embed_retries_total` doc: 429/503.
- `GET /config`: `model_id` now carries the canonical id; add `embedding_dim`,
  `min_chunk_tokens`, `max_chunk_tokens` (closes the recorded bench hazard —
  `/config` did not publish the window, so `slicer_sweep.py` had to
  re-tokenize chunks to verify it).
- VS Code: `tools/vscode/src/webview/status.ts:181` caption → model-agnostic
  ("Embedding model — turns code and questions into vectors."); `npm run
  compile` (the extension runs `dist/`). Grep `tools/` for other
  bge/BM3/model_id mentions.
- **Four-clients rule stays untriggered** — verify, don't assume: the file
  set, path spelling and hashed bytes are unchanged; `vectors_only` is an
  additive optional field; `model_id` appears in no request body and no
  `.mindex` key.

---

## 8. Phase 7 — deployment: `deploy/vllm/`, delete `embedder/`

**Delete the entire `embedder/` directory** (server, tests, lock, README,
systemd units).

New `deploy/vllm/`:

- `mindex-vllm@.service` — **system**-unit template, structurally copied from
  `embedder/systemd/mindex-embedder@.service` before deleting it:
  - `EnvironmentFile=` `~/.config/mindex/vllm-%i.env`.
  - `ExecStart=/…/mindex/deploy/vllm/.venv-%i/bin/vllm serve ${VLLM_MODEL}
    --task embed --port ${VLLM_PORT} --host 0.0.0.0 --dtype float16
    --gpu-memory-utilization ${VLLM_GPU_FRACTION}
    --max-model-len ${VLLM_MAXLEN} $VLLM_EXTRA_ARGS`
    (unquoted `$VLLM_EXTRA_ARGS` for word splitting; the `%i` in the first
    token is the same per-instance-venv trick the old unit relies on).
  - **No `Conflicts=` / no mutual `After=`** — instances bind different ports
    and may run concurrently; this is the deliberate difference from the old
    unit and the header says so. (Add `Conflicts=` back only if two instances
    ever share a port.)
  - Loopback confinement verbatim: `IPAddressDeny=any`,
    `IPAddressAllow=localhost`, `RestrictNetworkInterfaces=lo`,
    `SocketBindDeny=any`, `SocketBindAllow=tcp:11212`,
    `SocketBindAllow=tcp:11213` (both listed — env vars do not expand in
    these directives).
  - `Environment=HF_HUB_OFFLINE=1` + `TRANSFORMERS_OFFLINE=1` (first pull
    happens outside the unit); GPU section carried over (`DevicePolicy=closed`
    + the DeviceAllow list, `SupplementaryGroups=render video`, the
    PrivateDevices warning); `ReadWritePaths` gains `~/.cache/vllm`;
    `TimeoutStartSec=600` (engine init is slower than the old server).
- `vllm-egpu.env`:

  ```
  VLLM_MODEL=Qwen/Qwen3-Embedding-0.6B
  VLLM_PORT=11212
  VLLM_GPU_FRACTION=0.20   # embedding models are small; must not starve the
                           # research LLM sharing the eGPU
  VLLM_MAXLEN=8192
  VLLM_EXTRA_ARGS=
  ```

- `vllm-igpu.env`: `VLLM_PORT=11213`, `VLLM_GPU_FRACTION=0.5`, plus a comment
  block: XPU is an **experimental** vLLM backend — build `.venv-igpu` per the
  vLLM XPU instructions; if it cannot serve the embed task, fall back to
  `VLLM_EXTRA_ARGS=--device cpu` (slow but fine for the split query-side
  instance).
- `README.md`: venv install for both backends (ROCm wheel / XPU build), the
  first-pull procedure, and a **verification** section: `curl :PORT/v1/models`;
  embed one probe text on both instances and compare cosine ≈ 1.0; check the
  journal for the pooler config line (last-token pooling + normalize; fallback
  `--override-pooler-config '{"pooling_type":"LAST","normalize":true}'`); a
  mindex config snippet pointing `server_url` / `query_server_url` at the
  instances.
- `deploy/systemd/README.md`: embedder references → `../vllm/`.

---

## 9. Phase 8 — docs

- `.claude/CLAUDE.md`: rewrite the Retrieval pipeline section (dense-only,
  per-model collections, registry, the five-way `file_already_indexed`
  predicate, the prefix rule — queries only), the Layout section (`embedder/`
  gone, `deploy/vllm/`, `src/models/registry.rs`, `src/models/embedder.rs`),
  every `model_id`-in-PK invariant, the 1022-ceiling narrative, migrations
  list (one baseline, lineage refusal), `vectors_only` beside `symbols_only`.
- `docs/claude/retrieval-v2.md`: superseded banner (already prepended).
- `docs/claude/qdrant.md`: new name grammar, `OtherModel`, the GC
  cross-collection note, the v3 runbook (bump still not self-healing —
  `worker::stale` still the cover).
- `README.md`, `CHANGELOG.md` (breaking: config keys, DB lineage, embedder
  deployment), `perf/README.md` (`run.sh` scraped the old embedder's `/stats`,
  which vLLM does not serve — columns degrade to NA or the scrape is dropped).

---

## 10. Phase 9 — verification

**Rust:** `cargo fmt --check`; `cargo clippy --bin mindex` + each `tools/`
crate; `cargo test --bin mindex` + tools crates. Named survivors to watch:
`codes_are_stable` (changes only for the new `vectors_only`+`symbols_only`
validation code), `metric_names_are_stable` (must pass untouched),
`index_event_*` wire pins (+`vectors_only`), migration idempotency/FK tests
over the new one-entry list, the slicer suite under the Qwen tokenizer, the
embed suite, the `/health` split-embedder suite, the stale-worker suite, the
qdrant grammar matrix.

**New tests beyond the per-phase ones:** handshake refusal/warn split;
query-prefix-only-on-queries; the `vectors_only` flow (skip-when-current,
flips `embedded_model_id`, tokenizer-mismatch refusal, exclusion with
`symbols_only`); model-switch self-heal (`file_already_indexed` false after
flipping `[model].id`); `tokens` populated and positive for AST, gap and
markdown chunks; project delete drops all of the project's collections.

**Integration:** `docker-compose.test.yml` with the rewritten mock
(`--exit-code-from test-runner --abort-on-container-exit`). **`down -v`
first** — the schema baseline changed; an old volume is stamped and will 500.
Python lint per directory including `tests/mock_embedder` (ruff, black, mypy).

**Host smoke test (real vLLM):**
1. Build `.venv-egpu`, pull the 0.6B, start `mindex-vllm@egpu`,
   `curl :11212/v1/models`.
2. Fresh DB; start mindex with `[model] id = "qwen3-embedding-0.6b"`,
   `server_url = "http://127.0.0.1:11212"`; confirm the handshake log and the
   registry cross-check.
3. Point mindex at the **old** DB once → confirm the refusal message; delete it.
4. `mindex-index --force` a small project → collection `{guid}_q3e06b_v3`
   exists, 1024-d; `/search` returns sane hits; `search_unscorable_winners`
   stays 0.
5. If the 4B is pulled: flip `[model].id`, restart, `mindex-index
   --vectors-only` → new collection `{guid}_q3e4b_v3`, `embedded_model_id`
   flipped, old collection classified `OtherModel` (info, not orphaned). Flip
   back → instant reuse.
6. Drop the old `_v1`/`_v2` collections the stale worker names (and the stale
   bench collections in `~/.local/share/qdrant` — it is at ~61 GB).

**Bench acceptance:**
- `bench/bench-config.toml` was **left on the 256-window arm** (FINDINGS §1) —
  restore it, updated for the new config keys, window 364.
- Rebuild release, reindex django, `run.py` → `score.py` → `stats.py`
  django-short against the archived runs.
- **Expected ≈ 0.4540 nDCG@10** (the archived Qwen3-0.6B external arm) against
  0.3549 deployed. Materially below → check, in order: the query prefix, the
  pooler config, normalization.
- Hazards: django ships a deliberately non-UTF-8 fixture, so `mindex-index`
  exits 1 forever while being otherwise fine — judge completion by
  stderr/`/drift`, not exit code.

---

## 11. Deletion audit (the net-shrink contract)

`embedder/` (entire directory); `src/models/bge_m3.rs` (binary protocol,
`ENCODE_MAGIC`, `Reader`, the triple-head response struct); six v1 migration
files; `VECTOR_DIM` / `COLBERT_DATATYPE` / `COLBERT_HNSW_M`; the RRF prefetch
tree; the `STORABLE_TOKENS_CEILING` chain; `MODEL_MAX_TOKENS` /
`MODEL_INPUT_LIMIT_TOKENS`; config keys `dense_prefetch_limit` /
`sparse_prefetch_limit` / `fusion_limit` / `sparse_min_weight` and five
validation rules; `model_id` from seven tables and every query that bound it;
the sparse/colbert halves of the mock; the `EmbeddingModel` enum.

---

## 12. Risks and open questions — surface during execution, never resolve silently

1. **vLLM on XPU** is the weakest link (experimental backend). eGPU is
   primary; the igpu env file documents the CPU fallback. Do not block the
   migration on igpu.
2. **Pooler config**: verify on the host that vLLM applies last-token pooling
   + normalization for Qwen3-Embedding (journal line + the cosine sanity
   check). A wrong pooler is a clean-looking null result — exactly the failure
   class the bench guard exists for.
3. **`--task embed` spelling** varies across vLLM versions (`--runner
   pooling` in newer ones) — pin to what the installed version accepts and
   record it in the deploy README.
4. **The 364 window under the Qwen tokenizer** is best-available, not
   re-measured; the bench re-run (and a later 320/400 sweep) confirms or
   moves it.
5. **Large `/v1/embeddings` bodies** (`embed_batch_chunks` 256 × 364-token
   chunks): if vLLM rejects them, lower the default rather than adding
   client-side splitting — `embed_and_upsert` already batches.
6. **Tokenizer download at startup** now targets Qwen — pre-cache the HF
   tokenizer on offline hosts (the Rust side has no `HF_HUB_OFFLINE`
   equivalent).
7. **GC cross-collection dead points** — accepted disk-only cost, documented
   in `docs/claude/qdrant.md`.
