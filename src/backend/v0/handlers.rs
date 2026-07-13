use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use super::models::IndexRequest;
use crate::backend::error::ApiError;
use crate::backend::error::ProblemDetails;
use crate::backend::extract::{ApiJson, ApiPath, ApiQuery};
use crate::backend::http3;
use crate::backend::http3::EmbeddingModel;
use crate::backend::http3::RouterState;
use crate::backend::v0::validate;
use crate::backend::v0::models::CancelRequest;
use crate::backend::v0::models::CancelResponse;
use crate::backend::v0::models::ChunkCounts;
use crate::backend::v0::models::Code;
use crate::backend::v0::models::ConfigResponse;
use crate::backend::v0::models::DeleteFilesRequest;
use crate::backend::v0::models::DeleteFilesResponse;
use crate::backend::v0::models::DriftRequest;
use crate::backend::v0::models::DriftResponse;
use crate::backend::v0::models::FileInfo;
use crate::backend::v0::models::FileListQuery;
use crate::backend::v0::models::FileListResponse;
use crate::backend::v0::models::FileStatusCounts;
use crate::backend::v0::models::GcResponse;
use crate::backend::v0::models::RetryRequest;
use crate::backend::v0::models::RetryResponse;
use crate::backend::v0::models::StatusResponse;
use crate::backend::v0::models::HealthChecks;
use crate::backend::v0::models::HealthResponse;
use crate::backend::v0::models::ProjectListResponse;
use crate::backend::v0::models::ProjectSummary;
use crate::backend::v0::models::VersionResponse;
use crate::backend::v0::models::IndexResponse;
use crate::backend::v0::models::ProgrammingLanguage;
use crate::backend::v0::models::ProjectStats;
use crate::backend::v0::models::SearchFilter;
use crate::backend::v0::models::SearchRequest;
use crate::backend::v0::models::SearchResponse;
use crate::backend::v0::models::SearchResult;
use crate::backend::v0::models::UUIDv4;
use crate::db::files::set_file_status;
use crate::db::qdrant::SearchHit;
use crate::db::qdrant::VectorStore;
use crate::db::qdrant::collection_for;
use crate::db::sqlite3::SQLite3Pool;
use crate::db::sqlite3::SQLite3PoolError;
use crate::embed::EmbedUpsertError;
use crate::embed::embed_and_upsert;
use crate::models::bge_m3::BGEm3EmbedRequest;
use crate::models::bge_m3::BGEm3EmbedResponse;
use crate::models::bge_m3::BGEm3Model;
use crate::models::bge_m3::EncodeError;
use crate::slicing::traits::SlicedChunk;
use crate::slicing::traits::Slicer;
use crate::slicing::traits::SlicerError;
use axum::Json;
use axum::debug_handler;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use qdrant_client::qdrant::point_id::PointIdOptions;
use rusqlite::OptionalExtension;
use rusqlite::ToSql;
use rusqlite::params;
use rusqlite::params_from_iter;
use sha2::Sha256;
use sha2::digest::FixedOutputReset;
use sha2::digest::Update;
use tokenizers::Tokenizer;
use tokio_util::future::FutureExt;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::warn;
use tree_sitter::Language;
use uuid::Uuid;

pub trait OptionResultExt<T> {
    // `from_cancelled` takes `self` by value: it consumes the `Option<Result<..>>`
    // produced by `with_cancellation_token` and reinterprets `None` (timeout/cancel)
    // as `Err(Cancelled)`. The `from_*`-takes-no-self convention does not fit a
    // consuming adapter method, so the lint is intentionally allowed here.
    #[allow(clippy::wrong_self_convention)]
    fn from_cancelled(self) -> Result<T, SQLite3PoolError>;
}

impl<T> OptionResultExt<T> for Option<Result<T, SQLite3PoolError>> {
    fn from_cancelled(self) -> Result<T, SQLite3PoolError> {
        match self {
            Some(Ok(x)) => Ok(x),
            Some(Err(e)) => Err(e),
            None => Err(SQLite3PoolError::Cancelled),
        }
    }
}

fn slicer_err_to_pool_err(err: SlicerError) -> SQLite3PoolError {
    match err {
        SlicerError::Cancelled => SQLite3PoolError::Cancelled,
        other => {
            error!(error = %other, "Slicer failed to parse the source into chunks.");
            SQLite3PoolError::HTTPStatusCode(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// One positional bind value for the search query. Owned (so the whole vec is
/// `Send` and can move into the `spawn_blocking` transaction closure) and
/// `PartialEq` (so the query builder can be unit-tested against exact params).
#[derive(Debug, PartialEq)]
enum Bind {
    Guid(UUIDv4),
    Lang(ProgrammingLanguage),
    Path(String),
}

impl ToSql for Bind {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            Bind::Guid(g) => g.to_sql(),
            Bind::Lang(l) => l.to_sql(),
            Bind::Path(p) => p.to_sql(),
        }
    }
}

/// Builds `post_search`'s candidate query: selects **only** `c.qdrant_guid` — the
/// `has_id` set fed to Qdrant — never `code`/metadata, which `post_search` fetches
/// separately for just the top-k winners (loading display columns for every active
/// chunk would read megabytes per query and discard >99%).
///
/// Pure and side-effect-free so the fragile `?N` parameter-numbering — which must
/// stay in lock-step with the bind order — can be unit-tested in isolation. The
/// `WHERE` clause always pins the project and `c.status = 'active'`; the optional
/// `include`/`exclude` filters append language `IN`/`NOT IN` sets and path `GLOB`
/// clauses, numbering placeholders in push order.
fn build_search_query(project_guid: UUIDv4, req: &SearchRequest) -> (String, Vec<Bind>) {
    let mut param_number: usize = 1;

    // c.status = 'active' is always required to exclude soft-deleted chunks.
    let mut meta_where = vec![
        format!("c.project_guid = ?{}", param_number),
        "c.status = 'active'".to_string(),
    ];
    param_number += 1;
    let mut binds: Vec<Bind> = vec![Bind::Guid(project_guid)];

    if let Some(inc) = &req.include {
        if let Some(pls) = &inc.programming_languages {
            let placeholders: Vec<String> = pls
                .iter()
                .map(|_| {
                    let p = format!("?{param_number}");
                    param_number += 1;
                    p
                })
                .collect();
            meta_where.push(format!(
                "f.programming_language IN ({})",
                placeholders.join(", ")
            ));
            binds.extend(pls.iter().map(|l| Bind::Lang(*l)));
        }

        if let Some(paths) = &inc.paths {
            let clauses: Vec<String> = paths
                .iter()
                .map(|_| {
                    let c = format!("c.file_path GLOB ?{param_number}");
                    param_number += 1;
                    c
                })
                .collect();
            meta_where.push(format!("({})", clauses.join(" OR ")));
            binds.extend(paths.iter().map(|p| Bind::Path(p.0.as_str().to_string())));
        }
    }

    if let Some(exc) = &req.exclude {
        if let Some(pls) = &exc.programming_languages {
            let placeholders: Vec<String> = pls
                .iter()
                .map(|_| {
                    let p = format!("?{param_number}");
                    param_number += 1;
                    p
                })
                .collect();
            meta_where.push(format!(
                "f.programming_language NOT IN ({})",
                placeholders.join(", ")
            ));
            binds.extend(pls.iter().map(|l| Bind::Lang(*l)));
        }

        if let Some(paths) = &exc.paths {
            let clauses: Vec<String> = paths
                .iter()
                .map(|_| {
                    let c = format!("c.file_path GLOB ?{param_number}");
                    param_number += 1;
                    c
                })
                .collect();
            meta_where.push(format!("NOT ({})", clauses.join(" OR ")));
            binds.extend(paths.iter().map(|p| Bind::Path(p.0.as_str().to_string())));
        }
    }

    let sql = format!(
        "
    SELECT c.qdrant_guid
    FROM project_file_chunks c
    JOIN project_files f
        ON c.project_guid = f.project_guid
        AND c.model_id = f.model_id
        AND c.file_path = f.path
    WHERE {}",
        meta_where.join(" AND ")
    );

    (sql, binds)
}

/// Maps an API language to its tree-sitter grammar. Pure and total over the enum,
/// so adding a `ProgrammingLanguage` variant forces a new arm here (and is the one
/// spot a missing grammar would surface at compile time).
fn tree_sitter_language(pl: ProgrammingLanguage) -> Language {
    match pl {
        ProgrammingLanguage::Rust => Language::new(tree_sitter_rust::LANGUAGE),
        ProgrammingLanguage::Python => Language::new(tree_sitter_python::LANGUAGE),
        ProgrammingLanguage::JavaScript => Language::new(tree_sitter_javascript::LANGUAGE),
        ProgrammingLanguage::TypeScript => {
            Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
        }
        ProgrammingLanguage::Tsx => Language::new(tree_sitter_typescript::LANGUAGE_TSX),
        ProgrammingLanguage::Go => Language::new(tree_sitter_go::LANGUAGE),
        ProgrammingLanguage::C => Language::new(tree_sitter_c::LANGUAGE),
        ProgrammingLanguage::Cpp => Language::new(tree_sitter_cpp::LANGUAGE),
        ProgrammingLanguage::Java => Language::new(tree_sitter_java::LANGUAGE),
        ProgrammingLanguage::CSharp => Language::new(tree_sitter_c_sharp::LANGUAGE),
        ProgrammingLanguage::Ruby => Language::new(tree_sitter_ruby::LANGUAGE),
        ProgrammingLanguage::Php => Language::new(tree_sitter_php::LANGUAGE_PHP),
        ProgrammingLanguage::Bash => Language::new(tree_sitter_bash::LANGUAGE),
        ProgrammingLanguage::Html => Language::new(tree_sitter_html::LANGUAGE),
        ProgrammingLanguage::Css => Language::new(tree_sitter_css::LANGUAGE),
        ProgrammingLanguage::Json => Language::new(tree_sitter_json::LANGUAGE),
        ProgrammingLanguage::Scala => Language::new(tree_sitter_scala::LANGUAGE),
        ProgrammingLanguage::Haskell => Language::new(tree_sitter_haskell::LANGUAGE),
        ProgrammingLanguage::Ocaml => Language::new(tree_sitter_ocaml::LANGUAGE_OCAML),
        ProgrammingLanguage::Zig => Language::new(tree_sitter_zig::LANGUAGE),
        ProgrammingLanguage::Sql => Language::new(tree_sitter_sequel::LANGUAGE),
    }
}

/// In-process mutual exclusion for indexing a single `(project, model, path)`.
///
/// `post_index`'s pipeline is several independent transactions (hash-check →
/// mark `indexing` → slice+insert → embed → `mark_indexed`), not one atomic unit.
/// Two concurrent `/index` requests for the *same* file would interleave at those
/// boundaries: the second `prepare` marks the first's freshly-inserted chunks
/// `deleted` (so the first embeds orphan vectors), and the second `mark_indexed`
/// hits an illegal `indexed→indexed` transition — possibly leaving `sha256`
/// describing a different chunk set than is `active` (silent staleness on the next
/// hash-skip). This claim serializes the whole per-file pipeline within one process
/// (mindex is single-instance). A multi-instance deployment would need a DB-level
/// CAS claim instead — see TODO.md.
/// The per-file mutual-exclusion key: `{guid_simple}\0{model_id}\0{path}`. The NUL
/// separators can't appear in any component, so the join is unambiguous. Built
/// identically by the indexing handler and the retry worker so a claim taken by a
/// live `/index` is visible to the worker (and vice versa) — they share one lock
/// table. `guid_simple` must be the 32-char hyphen-less form (`Uuid::simple`), which
/// is exactly how the guid is stored in SQLite (see `UUIDv4`'s `ToSql`).
pub(crate) fn indexing_lock_key(guid_simple: &str, model_id: &str, path: &str) -> String {
    format!("{guid_simple}\u{0}{model_id}\u{0}{path}")
}

pub(crate) struct IndexClaim {
    locks: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl IndexClaim {
    /// `Some(claim)` if the slot was free, `None` if another request holds it.
    pub(crate) fn try_acquire(locks: &Arc<Mutex<HashSet<String>>>, key: String) -> Option<Self> {
        // Recover from a poisoned mutex rather than panic: the set is a plain
        // membership table, no invariant is broken by a panicked holder.
        let mut set = locks.lock().unwrap_or_else(|e| e.into_inner());
        if set.insert(key.clone()) {
            Some(IndexClaim { locks: Arc::clone(locks), key })
        } else {
            None
        }
    }
}

impl Drop for IndexClaim {
    fn drop(&mut self) {
        let mut set = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        set.remove(&self.key);
    }
}

/// A file that has been hash-checked, marked `indexing`, sliced, and had its
/// chunks inserted — awaiting the shared embed pass. `chunks` is drained into the
/// cross-file batch before `mark_indexed`. `_claim` holds the per-file lock for the
/// whole pipeline; it releases on drop (end of `post_index`, after `mark_indexed`).
struct Prepared {
    pl: ProgrammingLanguage,
    path: String,
    sha256: String,
    chunks: Vec<(UUIDv4, String)>,
    _claim: IndexClaim,
}

/// Borrowed view of everything indexing needs. `post_index` drives it in two
/// phases — `prepare` every file, then one batched `embed_all` across all of them —
/// so the GPU sees `embed_batch`-sized batches instead of one file's chunks at a time.
struct FileIndexer<'a> {
    db_pool: &'a SQLite3Pool,
    store: &'a dyn VectorStore,
    tokenizer: &'a Arc<Tokenizer>,
    embedder: &'a dyn BGEm3Model,
    model_id: &'a str,
    project_guid: UUIDv4,
    collection: &'a str,
    /// Embed/upsert batch sizing + sparse threshold (from config).
    embed_tuning: crate::embed::EmbedTuning,
    /// Slicer token window (from config).
    min_chunk_tokens: usize,
    max_chunk_tokens: usize,
    /// Request-scoped cancellation token (the handler's `CancellationGuard`).
    token: &'a CancellationToken,
    /// Shared set of `(project, model, path)` keys currently being indexed — the
    /// per-file mutual-exclusion table (see `IndexClaim`).
    indexing_locks: &'a Arc<Mutex<HashSet<String>>>,
}

/// True iff this file is already **successfully indexed** with this exact content:
/// a row exists with `status = 'indexed'` and a matching `sha256`. The stored
/// `sha256` always reflects the content whose chunks are currently in the table —
/// it is (re)written when the file enters `indexing` (the prepare upsert) and
/// confirmed at `indexed`. Only an `indexed` row counts for the skip, because a
/// non-`indexed` row has no (complete) vectors: a file sliced but never embedded
/// (e.g. the embedder was down, leaving it `failed`/`indexing`) carries the right
/// hash without any vectors. Gating the skip on `status = 'indexed'` is what lets a
/// later re-index pick such a file back up instead of treating it as unchanged forever.
fn file_already_indexed(
    tx: &rusqlite::Transaction,
    project_guid: UUIDv4,
    path: &str,
    model_id: &str,
    sha256: &str,
) -> Result<bool, SQLite3PoolError> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT sha256 FROM project_files
             WHERE project_guid = ?1 AND path = ?2 AND model_id = ?3
               AND status = 'indexed'",
            params![project_guid, path, model_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(existing.as_deref() == Some(sha256))
}

/// The prepare-phase upsert that moves a file into `indexing`. Extracted as a const
/// so the sha256-refresh regression test executes the exact production statement
/// (binds: ?1 project_guid, ?2 path, ?3 sha256, ?4 programming_language, ?5 model_id).
const MARK_INDEXING_UPSERT_SQL: &str = "INSERT INTO project_files
         (project_guid, path, sha256, programming_language, model_id,
          status, status_updated_at)
     VALUES (?1, ?2, ?3, ?4, ?5, 'indexing', unixepoch())
     ON CONFLICT (project_guid, model_id, path)
     DO UPDATE SET status = 'indexing', sha256 = excluded.sha256,
                   status_updated_at = unixepoch()";

impl FileIndexer<'_> {
    /// Phase 1 for one file: hash-check → mark `indexing` → mark old chunks deleted,
    /// slice, insert new chunks. Returns `Ok(None)` if unchanged (skipped),
    /// `Ok(Some(Prepared))` with its chunks (possibly empty, for too-short files),
    /// or `Err` — in which case *this* file's status is already recovered.
    async fn prepare(
        &self,
        pl: ProgrammingLanguage,
        path: &str,
        code: &str,
        hasher: &mut Sha256,
    ) -> Result<Option<Prepared>, ApiError> {
        let span = info_span!("indexing_file", ?pl, path);
        async move {
            let project_guid = self.project_guid;

            // ── claim the per-file slot (serialize concurrent same-file indexing) ─
            // Held across the whole pipeline via `Prepared._claim`; released on any
            // early return below (unchanged / error) when this local drops.
            let claim = {
                let key = indexing_lock_key(
                    &project_guid.0.as_simple().to_string(),
                    self.model_id,
                    path,
                );
                match IndexClaim::try_acquire(self.indexing_locks, key) {
                    Some(c) => c,
                    None => {
                        info!(
                            "The file is already being indexed by another in-flight \
                             request; skipping it so the rest of the batch can proceed."
                        );
                        return Err(ApiError::FileInFlight);
                    }
                }
            };

            hasher.update(code.as_bytes());
            let sha256 = hex::encode(hasher.finalize_fixed_reset());

            // ── hash check ───────────────────────────────────────────────
            {
                let (sha256_c, path_c, model_id_c) =
                    (sha256.clone(), path.to_string(), self.model_id.to_string());
                let unchanged = self
                    .db_pool
                    .transaction(self.token.child_token(), move |tx| {
                        file_already_indexed(tx, project_guid, &path_c, &model_id_c, &sha256_c)
                    })
                    .with_cancellation_token(self.token)
                    .await
                    .from_cancelled()
                    .map_err(|err| {
                        error!(error = ?err, "Failed to read the stored file hash from SQLite.");
                        ApiError::from(err)
                    })?;

                if unchanged {
                    info!("The source code has not changed: no reindexing is required.");
                    return Ok(None);
                }

                info!("The source code has changed: reindexing is required.");
            }

            // ── status = 'indexing' (committed before heavy work) ────────
            {
                let (sha256_u, path_u, model_id_u) =
                    (sha256.clone(), path.to_string(), self.model_id.to_string());
                self.db_pool
                    .transaction(self.token.child_token(), move |tx| {
                        tx.execute(
                            MARK_INDEXING_UPSERT_SQL,
                            params![project_guid, path_u, sha256_u, pl, model_id_u],
                        )?;
                        Ok(())
                    })
                    .with_cancellation_token(self.token)
                    .await
                    .from_cancelled()
                    .map_err(|err| {
                        error!(error = ?err, "Failed to mark the file 'indexing' in SQLite.");
                        ApiError::from(err)
                    })?;
            }

            // ── mark old chunks deleted + slice + insert new chunks ───────
            let tokenizer = self.tokenizer.clone();
            let (path_m, model_id_m, code_m) =
                (path.to_string(), self.model_id.to_string(), code.to_string());
            let (min_chunk_tokens, max_chunk_tokens) =
                (self.min_chunk_tokens, self.max_chunk_tokens);
            let slicer_token = self.token.clone();

            let result = self
                .db_pool
                .transaction(self.token.child_token(), move |tx| {
                    tx.execute(
                        "UPDATE project_file_chunks
                         SET status = 'deleted'
                         WHERE project_guid = ?1 AND file_path = ?2 AND model_id = ?3
                           AND status = 'active'",
                        params![project_guid, path_m, model_id_m],
                    )?;

                    let mut slicer = Slicer::new(
                        tree_sitter_language(pl),
                        &*tokenizer,
                        min_chunk_tokens,
                        max_chunk_tokens,
                    )
                    .map_err(slicer_err_to_pool_err)?;

                    let chunks = slicer
                        .parse(&code_m, slicer_token)
                        .map_err(slicer_err_to_pool_err)?;

                    info!(chunks_len = chunks.len(), "Sliced the source code.");

                    let mut out: Vec<(UUIDv4, String)> = Vec::with_capacity(chunks.len());
                    for SlicedChunk {
                        code,
                        start_line,
                        end_line,
                        start_column,
                        end_column,
                        ..
                    } in &chunks
                    {
                        let qdrant_guid = UUIDv4(Uuid::new_v4());
                        tx.execute(
                            "INSERT INTO project_file_chunks
                                 (project_guid, file_path, code, model_id, qdrant_guid,
                                  start_line, end_line, start_column, end_column, status)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active')",
                            params![
                                project_guid,
                                path_m,
                                code,
                                model_id_m,
                                qdrant_guid,
                                *start_line as i64,
                                *end_line as i64,
                                *start_column as i64,
                                *end_column as i64
                            ],
                        )?;
                        out.push((qdrant_guid, code.clone()));
                    }

                    Ok(out)
                })
                .with_cancellation_token(self.token)
                .await
                .from_cancelled();

            let chunks = match result {
                Ok(v) => v,
                Err(SQLite3PoolError::Cancelled) => {
                    self.recover(path, "cancelled", false).await;
                    return Err(ApiError::Cancelled);
                }
                Err(SQLite3PoolError::HTTPStatusCode(_)) => {
                    // Set only by `slicer_err_to_pool_err` (always 500): a slicer failure.
                    self.recover(path, "failed", true).await;
                    return Err(ApiError::Internal);
                }
                Err(err) => {
                    error!(error = ?err, "Slicing / chunk insertion failed; marking file 'failed'.");
                    self.recover(path, "failed", true).await;
                    return Err(ApiError::Internal);
                }
            };

            Ok(Some(Prepared { pl, path: path.to_string(), sha256, chunks, _claim: claim }))
        }
        .instrument(span)
        .await
    }

    /// Phase 2: embed + upsert every chunk across all prepared files in one batched
    /// pass (`embed_batch` chunks per `/encode`).
    async fn embed_all(&self, chunks: &[(UUIDv4, String)]) -> Result<(), EmbedUpsertError> {
        embed_and_upsert(
            self.embedder,
            self.store,
            self.collection,
            chunks,
            self.token,
            self.embed_tuning,
        )
        .await
    }

    /// Phase 3 for one file: mark it `indexed` and record the new sha256. The
    /// `AND status = 'indexing'` guard makes this a no-op (matching 0 rows, so no
    /// trigger fires) if a concurrent `POST /cancel` moved the file to `cancelled`
    /// since it was prepared — without it the raw `cancelled → indexed` UPDATE would
    /// trip the state-machine trigger and error the whole batch, leaving sibling
    /// files stuck in `indexing`.
    async fn mark_indexed(&self, path: &str, sha256: &str) -> Result<(), ApiError> {
        let project_guid = self.project_guid;
        let (sha256_f, path_f, model_id_f) = (
            sha256.to_string(),
            path.to_string(),
            self.model_id.to_string(),
        );
        self.db_pool
            .transaction(self.token.child_token(), move |tx| {
                tx.execute(
                    "UPDATE project_files
                     SET status = 'indexed', sha256 = ?1, retry_count = 0,
                         status_updated_at = unixepoch()
                     WHERE project_guid = ?2 AND path = ?3 AND model_id = ?4
                       AND status = 'indexing'",
                    params![sha256_f, project_guid, path_f, model_id_f],
                )?;
                Ok(())
            })
            .with_cancellation_token(self.token)
            .await
            .from_cancelled()
            .map_err(|err| {
                error!(error = ?err, "Failed to mark the file 'indexed' in SQLite.");
                ApiError::from(err)
            })?;
        Ok(())
    }

    /// Best-effort recovery: move the file to `status` (incrementing `retry_count`
    /// when `increment_retry`) on a cancellation/failure path.
    async fn recover(&self, path: &str, status: &'static str, increment_retry: bool) {
        set_file_status(
            self.db_pool,
            &self.project_guid.0.as_simple().to_string(),
            path,
            self.model_id,
            status,
            increment_retry,
            self.token.child_token(),
        )
        .await;
    }

    /// Recovers every already-prepared file when the batch is aborted (a later
    /// file's prepare failed, or the shared embed failed) — they are still
    /// `indexing` with chunks inserted, so this hands them to the retry worker.
    async fn recover_all(
        &self,
        prepared: &[Prepared],
        status: &'static str,
        increment_retry: bool,
    ) {
        for p in prepared {
            self.recover(&p.path, status, increment_retry).await;
        }
    }

    /// Reconciliation between Phase 1 and Phase 2: drop any prepared file that a
    /// concurrent `POST /cancel` (or `DELETE /files`) flipped out of `indexing`
    /// since it was prepared. `/cancel` deliberately does not take the per-file
    /// claim (so it can interrupt a held one), so the live request must check for
    /// itself before the expensive embed. Cancelled files' just-inserted `active`
    /// chunks are marked `deleted` so GC reclaims them — this also closes the race
    /// where `/cancel` landed after `status='indexing'` but before the chunks
    /// existed (the `/cancel` UPDATE then matched no chunks). Best-effort: a query
    /// failure leaves the set whole (worst case is a wasted embed; the still-
    /// `cancelled` file's `mark_indexed` is rejected and its chunks GC'd anyway).
    async fn drop_cancelled(&self, prepared: Vec<Prepared>) -> Vec<Prepared> {
        if prepared.is_empty() {
            return prepared;
        }
        let project_guid = self.project_guid;
        let paths: Vec<String> = prepared.iter().map(|p| p.path.clone()).collect();
        let model_id = self.model_id.to_string();

        let cancelled: HashSet<String> = self
            .db_pool
            .transaction(self.token.child_token(), move |tx| {
                let placeholders = (3..3 + paths.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut binds: Vec<Bind> = Vec::with_capacity(paths.len() + 2);
                binds.push(Bind::Guid(project_guid));
                binds.push(Bind::Path(model_id));
                binds.extend(paths.into_iter().map(Bind::Path));
                tx.prepare(&format!(
                    "SELECT path FROM project_files
                     WHERE project_guid = ?1 AND model_id = ?2 AND status != 'indexing'
                       AND path IN ({placeholders})"
                ))?
                .query_map(params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?
                .collect::<Result<HashSet<_>, _>>()
                .map_err(SQLite3PoolError::from)
            })
            .with_cancellation_token(self.token)
            .await
            .from_cancelled()
            .unwrap_or_default();

        if cancelled.is_empty() {
            return prepared;
        }

        for path in &cancelled {
            let (pg, p, m) = (project_guid, path.clone(), self.model_id.to_string());
            let _ = self
                .db_pool
                .transaction(self.token.child_token(), move |tx| {
                    tx.execute(
                        "UPDATE project_file_chunks SET status = 'deleted'
                         WHERE project_guid = ?1 AND file_path = ?2 AND model_id = ?3
                           AND status = 'active'",
                        params![pg, p, m],
                    )?;
                    Ok(())
                })
                .with_cancellation_token(self.token)
                .await
                .from_cancelled();
            info!(%path, "Indexing cancelled mid-flight; skipping the embed pass for this file.");
        }

        prepared
            .into_iter()
            .filter(|p| !cancelled.contains(&p.path))
            .collect()
    }
}

/// Index (or reindex) a batch of files for a project.
///
/// Files are grouped by language → path. The pipeline runs in two phases so the GPU
/// sees large batches: every file is hashed, marked `indexing`, sliced into 128–512
/// token chunks and its chunks inserted; then **all** files' chunks are embedded in
/// one batched pass and upserted to Qdrant; finally each file is marked `indexed`.
/// Re-indexing identical content (matching sha256) is skipped server-side and omitted
/// from the response. The project and its Qdrant collection are created on first use.
///
/// Reindex is append-only: old chunks are soft-deleted (reclaimed later by GC), never
/// deleted inline, so indexing latency is decoupled from Qdrant delete latency.
///
/// **Concurrency:** safe. Each `(project, model, path)` is serialized by an in-process
/// claim — a second in-flight request for the *same* file is **skipped** (it is absent
/// from the response, like an unchanged file); different files proceed in parallel.
/// A concurrent `POST /cancel` is reconciled before the embed pass. On any failure
/// the whole batch is recovered to `failed`/`cancelled` and the retry worker re-attempts it.
#[utoipa::path(
    post,
    path = "/v0/{project_guid}/index",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = IndexRequest,
    responses(
        (status = 200, description = "Per-file chunk counts for the files actually (re)indexed.", body = IndexResponse),
        (status = 400, description = "Validation failed (bad path, oversized file, too many files).", body = ProblemDetails),
        (status = 413, description = "The request body exceeded [server].max_body_mib.", body = ProblemDetails),
        (status = 499, description = "Client closed the connection; indexing was cancelled (nginx convention).", body = ProblemDetails),
        (status = 500, description = "SQLite, slicer, or Qdrant upsert failure; the batch was marked `failed` for the retry worker.", body = ProblemDetails),
        (status = 503, description = "The embedder is unreachable or returned persistent backpressure; the batch was marked `failed`.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_index(
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(s): State<RouterState>,
    ApiJson(payload): ApiJson<IndexRequest>,
) -> Result<Json<IndexResponse>, ApiError> {
    let span = info_span!("indexing", project_guid = %project_guid.0);

    async move {
        validate::validate_index_request(&payload, s.max_files_per_request, s.max_code_bytes)?;

        let guard = http3::CancellationGuard(CancellationToken::new());

        let db_pool = s.db_pool;
        let qdrant = s.qdrant;
        let tokenizer = s.tokenizer;
        let indexing_locks = s.indexing_locks;
        let EmbeddingModel::BGEm3 { model_id, client } = s.model;

        let collection = collection_for(project_guid);

        // ── ensure project row ────────────────────────────────────────────────
        {
            let model_id = model_id.clone();
            db_pool
                .transaction(guard.0.child_token(), move |tx| {
                    // Idempotent and concurrency-safe: two parallel first-time /index
                    // calls for the same new project both reach here. A SELECT-then-
                    // INSERT would let both pass the check and the second trip the
                    // (guid, model_id) PK, failing an otherwise-valid request with 500.
                    // ON CONFLICT DO NOTHING makes the loser a no-op instead. This is
                    // *before* the per-file claim, so the claim can't cover it.
                    let inserted = tx.execute(
                        "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)
                         ON CONFLICT (guid, model_id) DO NOTHING",
                        params![project_guid, model_id],
                    )?;
                    if inserted > 0 {
                        info!("Created a new project.");
                    } else {
                        info!("Project already exists.");
                    }
                    Ok(())
                })
                .with_cancellation_token(&guard.0)
                .await
                .from_cancelled()
                .map_err(|err| {
                    error!(error = ?err, "Failed to ensure the project row in SQLite.");
                    ApiError::from(err)
                })?;
        }

        // ── ensure Qdrant collection ──────────────────────────────────────────
        qdrant.ensure_project(&collection).await.map_err(|err| {
            error!(
                error = ?err,
                "Failed to ensure the Qdrant collection. \
                 Check Qdrant is reachable at --qdrant-server and accepting connections."
            );
            ApiError::Internal
        })?;

        let mut res = IndexResponse {
            files: HashMap::new(),
        };
        let mut sha256_hasher = Sha256::default();

        let indexer = FileIndexer {
            db_pool: &db_pool,
            store: &*qdrant,
            tokenizer: &tokenizer,
            embedder: &*client,
            model_id: &model_id,
            project_guid,
            collection: &collection,
            embed_tuning: s.embed_tuning,
            min_chunk_tokens: s.min_chunk_tokens,
            max_chunk_tokens: s.max_chunk_tokens,
            token: &guard.0,
            indexing_locks: &indexing_locks,
        };

        // ── Phase 1: prepare every file (hash-check, mark indexing, slice + insert).
        let mut prepared: Vec<Prepared> = Vec::new();
        for (pl, files) in payload.files.iter() {
            let pl = *pl;
            res.files.entry(pl).or_default();

            for (path, Code { code }) in files.iter() {
                match indexer.prepare(pl, path, code, &mut sha256_hasher).await {
                    Ok(Some(p)) => prepared.push(p),
                    Ok(None) => {} // unchanged — skipped
                    // Another in-flight request holds the claim for this file; skip it
                    // so the rest of the batch proceeds. Innocent co-batched files must
                    // not pay a retry_count penalty for an unrelated file's contention.
                    Err(ApiError::FileInFlight) => {}
                    Err(e) => {
                        // A real prepare failure; recover the ones already prepared
                        // (they're 'indexing' with chunks inserted) before bailing.
                        indexer.recover_all(&prepared, "failed", true).await;
                        return Err(e);
                    }
                }
            }
        }

        // ── Reconcile against concurrent cancellation before the expensive embed pass:
        //    drop any file a `POST /cancel` flipped to 'cancelled' since it was prepared.
        let mut prepared = indexer.drop_cancelled(prepared).await;

        // ── Phase 2: embed + upsert every chunk across all files in one batched pass.
        let counts: Vec<u64> = prepared.iter().map(|p| p.chunks.len() as u64).collect();
        let all_chunks: Vec<(UUIDv4, String)> = prepared
            .iter_mut()
            .flat_map(|p| std::mem::take(&mut p.chunks))
            .collect();

        info!(
            files = prepared.len(),
            chunks = all_chunks.len(),
            "Embedding request in batches."
        );

        match indexer.embed_all(&all_chunks).await {
            Ok(()) => {}
            Err(EmbedUpsertError::Cancelled) => {
                indexer.recover_all(&prepared, "cancelled", false).await;
                return Err(ApiError::Cancelled);
            }
            Err(EmbedUpsertError::Embed(request_err)) => {
                error!(
                    error = ?request_err,
                    "Embedding request failed; marking batch 'failed'. \
                     Check the model server at --model-server is up and reachable \
                     (from inside the container it must bind 0.0.0.0, not 127.0.0.1)."
                );
                indexer.recover_all(&prepared, "failed", true).await;
                return Err(ApiError::EmbedderUnavailable);
            }
            Err(EmbedUpsertError::Decode(decode_err)) => {
                error!(
                    error = %decode_err,
                    "Embedder response decode failed; marking batch 'failed'. \
                     The embedder and mindex binary wire formats disagree — \
                     redeploy them from the same revision."
                );
                indexer.recover_all(&prepared, "failed", true).await;
                return Err(ApiError::EmbedderUnavailable);
            }
            Err(EmbedUpsertError::Store(qdrant_err)) => {
                error!(
                    error = ?qdrant_err,
                    "Qdrant upsert failed; marking batch 'failed'. \
                     Check Qdrant is reachable at --qdrant-server."
                );
                indexer.recover_all(&prepared, "failed", true).await;
                return Err(ApiError::Internal);
            }
        }

        // ── Phase 3: mark each prepared file 'indexed' and tally the response.
        for (p, count) in prepared.iter().zip(counts) {
            indexer.mark_indexed(&p.path, &p.sha256).await?;
            *res.files
                .entry(p.pl)
                .or_default()
                .entry(p.path.clone())
                .or_insert(0) += count;
        }

        info!("All files processed.");
        Ok(Json(res))
    }
    .instrument(span)
    .await
}

/// Pure drift computation: classify each working-tree path against the server's
/// view. Kept separate from the handler so it is unit-testable without a DB.
///
/// `in_flight` is checked **first**: a file currently being indexed is reported
/// `indexing` and never `stale`/`missing`. Its stored `sha256` is the *incoming*
/// content's hash (written when the file enters `indexing`), but its vectors are
/// not ready yet, so it must be excluded from drift regardless — re-triggering it
/// would race the live batch.
fn compute_drift(
    indexed: &HashMap<String, String>,
    in_flight: &HashSet<String>,
    local: &HashMap<String, String>,
) -> DriftResponse {
    let mut out = DriftResponse::default();

    for (path, local_sha) in local {
        if in_flight.contains(path) {
            out.indexing.push(path.clone());
        } else if let Some(indexed_sha) = indexed.get(path) {
            if indexed_sha != local_sha {
                out.stale.push(path.clone());
            }
        } else {
            out.missing.push(path.clone());
        }
    }

    // Indexed but gone from the working tree — but an in-flight file absent locally
    // is left to settle, not called orphaned.
    for path in indexed.keys() {
        if !local.contains_key(path) && !in_flight.contains(path) {
            out.orphaned.push(path.clone());
        }
    }

    out.stale.sort();
    out.missing.sort();
    out.orphaned.sort();
    out.indexing.sort();
    out
}

/// Read the drift baseline from SQLite: `(indexed path→sha256, in-flight paths)`.
/// `failed`/`deleted` rows are excluded so their paths fall into `missing` (they do
/// need indexing); `indexed` carries a trustworthy hash, everything else is in flight.
async fn read_drift_baseline(
    s: &RouterState,
    token: &CancellationToken,
    project_guid: UUIDv4,
) -> Result<(HashMap<String, String>, HashSet<String>), ApiError> {
    let rows: Vec<(String, String, String)> = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let mut stmt = tx.prepare(
                "SELECT path, sha256, status FROM project_files
                 WHERE project_guid = ?1
                   AND status IN ('indexed', 'indexing', 'just_uploaded')",
            )?;
            let rows = stmt
                .query_map(params![project_guid], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|err| {
            error!(
                error = ?err,
                project_guid = %project_guid.0,
                "Failed to read the project manifest for drift. Check the DB is writable."
            );
            ApiError::from(err)
        })?;

    let mut indexed: HashMap<String, String> = HashMap::new();
    let mut in_flight: HashSet<String> = HashSet::new();
    for (path, sha256, status) in rows {
        if status == "indexed" {
            indexed.insert(path, sha256);
        } else {
            in_flight.insert(path);
        }
    }
    Ok((indexed, in_flight))
}

/// `POST /projects/{guid}/drift` — compare the posted working-tree `path → sha256`
/// map against the index and return the divergence. Filesystem-agnostic: the client
/// walked and hashed; this only reads stored hashes. Unlike `post_search`, an empty
/// project is not a 404 — it just means every posted file is `missing`.
///
/// **Concurrency:** safe — pure read, takes no locks. In-flight files are reported as
/// `indexing` (never `stale`/`missing`) since their stored hash is the previous value.
#[utoipa::path(
    post,
    path = "/projects/{project_guid}/drift",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = DriftRequest,
    responses(
        (status = 200, description = "Working-tree divergence in four buckets (stale/missing/orphaned/indexing).", body = DriftResponse),
        (status = 400, description = "Validation failed (bad path, bad sha256, too many files).", body = ProblemDetails),
        (status = 499, description = "Client closed the connection.", body = ProblemDetails),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_drift(
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(s): State<RouterState>,
    ApiJson(payload): ApiJson<DriftRequest>,
) -> Result<Json<DriftResponse>, ApiError> {
    validate::validate_drift_request(&payload, s.max_drift_files)?;
    let guard = http3::CancellationGuard(CancellationToken::new());
    let (indexed, in_flight) = read_drift_baseline(&s, &guard.0, project_guid).await?;
    Ok(Json(compute_drift(&indexed, &in_flight, &payload.files)))
}

/// Hybrid semantic + lexical code search within one project.
///
/// The query is embedded with BGE-M3 (dense + sparse + ColBERT). Candidate chunks are
/// the project's `active` chunks matching the optional `include`/`exclude` selector
/// (project isolation + soft-delete exclusion happen here, in SQLite). Qdrant then
/// prefetches top-200 dense + top-200 sparse, fuses with RRF, reranks with ColBERT
/// MaxSim, and returns the top-k. Results are sorted by score descending.
///
/// An empty candidate set (nothing indexed, or filtered to nothing) returns **404**
/// immediately without touching Qdrant.
///
/// **Concurrency:** safe — read-only, takes no locks; never blocks or is blocked by
/// indexing/GC. Honors client cancellation (**499**).
#[utoipa::path(
    post,
    path = "/v0/{project_guid}/search",
    tag = "Search",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = SearchRequest,
    responses(
        (status = 200, description = "Ranked matches, sorted by score descending.", body = SearchResponse),
        (status = 400, description = "Validation failed (empty/oversized query, top_k out of range, oversized selector).", body = ProblemDetails),
        (status = 404, description = "No active chunks match (empty project or over-narrow filter).", body = ProblemDetails),
        (status = 499, description = "Client closed the connection.", body = ProblemDetails),
        (status = 500, description = "SQLite failure while building the candidate set or fetching display rows.", body = ProblemDetails),
        (status = 503, description = "The embedder or Qdrant is unreachable.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_search(
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(state): State<RouterState>,
    ApiJson(payload): ApiJson<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let span = info_span!("searching", project_guid = %project_guid.0);

    async move {
    validate::validate_query(&payload.query, state.max_query_bytes)?;
    validate::validate_top_k(payload.top_k, state.max_top_k)?;
    validate::validate_selector(&payload.include, state.max_selector_patterns)?;
    validate::validate_selector(&payload.exclude, state.max_selector_patterns)?;

    let guard = http3::CancellationGuard(CancellationToken::new());

    let EmbeddingModel::BGEm3 { client, .. } = state.model;
    let BGEm3EmbedResponse {
        dense_vecs,
        sparse_vecs,
        colbert_vecs,
    } = match client
        .encode(
            BGEm3EmbedRequest {
                texts: vec![payload.query.clone()],
            },
            guard.0.clone(),
        )
        .await
    {
        Ok(val) => Ok(val),
        Err(EncodeError::Cancelled) => Err(ApiError::Cancelled),
        Err(EncodeError::Request(request_err)) => {
            error!(
                error = ?request_err,
                "Failed to embed the search query. \
                 Check the model server at --model-server is up and reachable."
            );
            Err(ApiError::EmbedderUnavailable)
        }
        Err(EncodeError::Decode(decode_err)) => {
            error!(
                error = %decode_err,
                "Failed to decode the embedder's response for the search query. \
                 The embedder and mindex binary wire formats disagree — \
                 redeploy them from the same revision."
            );
            Err(ApiError::EmbedderUnavailable)
        }
    }?;

    let (sql, binds) = build_search_query(project_guid, &payload);

    // Query 1 (candidate set): only the `qdrant_guid`s feeding Qdrant's `has_id`
    // filter — no `code`/metadata for the (potentially huge) full active set.
    let candidate_ids: Vec<UUIDv4> = state
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            tx.prepare(&sql)?
                .query_map(params_from_iter(binds), |row| row.get::<_, UUIDv4>(0))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|err| {
            if !matches!(err, SQLite3PoolError::Cancelled) {
                error!(error = %err, "Failed to query candidate chunks from SQLite.");
            }
            ApiError::from(err)
        })?;

    if candidate_ids.is_empty() {
        return Err(ApiError::NoMatch);
    }

    // The embedder must return exactly one vector per head for the single query; an
    // empty list is an embedder contract violation, not a client error.
    let dense = dense_vecs
        .into_iter()
        .next()
        .ok_or(ApiError::EmbedderUnavailable)?;
    let sparse = sparse_vecs
        .into_iter()
        .next()
        .ok_or(ApiError::EmbedderUnavailable)?;
    let colbert = colbert_vecs
        .into_iter()
        .next()
        .ok_or(ApiError::EmbedderUnavailable)?;

    let search_hits = state
        .qdrant
        .search(
            &collection_for(project_guid),
            candidate_ids,
            dense,
            sparse.keys().copied().collect(),
            sparse.values().copied().collect(),
            colbert,
            payload.top_k.map(|k| k as u64).unwrap_or(state.default_top_k),
        )
        .await
    .map_err(|err| {
        error!(
            error = ?err,
            "Qdrant query failed. Check Qdrant is reachable at --qdrant-server \
             and the project's collection exists."
        );
        ApiError::QdrantUnavailable
    })?;

    // Winners as (id, score), keeping Qdrant's order (we re-sort after the fetch).
    let scored: Vec<(UUIDv4, f32)> = search_hits
        .iter()
        .filter_map(|SearchHit { id, score }| match &id.point_id_options {
            Some(PointIdOptions::Uuid(uuid)) => match Uuid::parse_str(uuid) {
                Ok(uuid) => Some((UUIDv4(uuid), *score)),
                Err(err) => {
                    warn!(error = ?err, point_id = %uuid, "Qdrant returned a point id that is not a valid UUID; skipping it.");
                    None
                }
            },
            _ => None,
        })
        .collect();

    if scored.is_empty() {
        return Err(ApiError::NoMatch);
    }

    // Query 2 (display): fetch `code`/metadata for *only* the top-k winners.
    let winner_ids: Vec<UUIDv4> = scored.iter().map(|(uuid, _)| *uuid).collect();
    let display = state
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let placeholders = (1..=winner_ids.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT qdrant_guid, file_path, code, start_line, end_line, start_column, end_column
                 FROM project_file_chunks
                 WHERE status = 'active' AND qdrant_guid IN ({placeholders})"
            );
            tx.prepare(&sql)?
                .query_map(params_from_iter(winner_ids.iter()), |row| {
                    Ok((
                        row.get::<_, UUIDv4>(0)?,
                        (
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                        ),
                    ))
                })?
                .collect::<Result<std::collections::HashMap<UUIDv4, (String, String, i64, i64, i64, i64)>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|err| {
            if !matches!(err, SQLite3PoolError::Cancelled) {
                error!(error = %err, "Failed to fetch result rows from SQLite.");
            }
            ApiError::from(err)
        })?;

    let mut results: Vec<SearchResult> = scored
        .iter()
        .filter_map(|(uuid, score)| {
            let (path, code, start_line, end_line, start_column, end_column) =
                display.get(uuid)?;
            Some(SearchResult {
                score: *score,
                path: path.clone(),
                code: code.clone(),
                start_line: *start_line as usize,
                end_line: *end_line as usize,
                start_column: *start_column as usize,
                end_column: *end_column as usize,
            })
        })
        .collect();

    // Guarantee the response is sorted by score (descending), independent of the
    // order Qdrant's fusion/rerank happens to return.
    results.sort_by(|a, b| b.score.total_cmp(&a.score));

    Ok(Json(SearchResponse { results }))
    }
    .instrument(span)
    .await
}

// ─── Management endpoints ───────────────────────────────────────────────────

/// List every known project with a compact summary.
///
/// Returns file count, files currently indexing, and active-chunk count per project.
/// Empty list when nothing has been indexed yet.
///
/// **Concurrency:** safe — read-only, takes no locks.
#[utoipa::path(
    get,
    path = "/projects",
    tag = "Projects",
    responses(
        (status = 200, description = "All projects with summary counts.", body = ProjectListResponse),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_projects(
    State(s): State<RouterState>,
) -> Result<Json<ProjectListResponse>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());

    let projects = s
        .db_pool
        .transaction(guard.0.child_token(), |tx| {
            let mut stmt = tx.prepare(
                "SELECT p.guid,
                        (SELECT COUNT(*) FROM project_files f
                          WHERE f.project_guid = p.guid) AS files,
                        (SELECT COUNT(*) FROM project_files f
                          WHERE f.project_guid = p.guid AND f.status = 'indexing') AS indexing,
                        (SELECT COUNT(*) FROM project_file_chunks c
                          WHERE c.project_guid = p.guid AND c.status = 'active') AS active_chunks
                 FROM projects p
                 GROUP BY p.guid
                 ORDER BY p.guid",
            )?;
            stmt.query_map([], |r| {
                Ok(ProjectSummary {
                    project_guid: r.get::<_, String>(0)?,
                    files: r.get::<_, i64>(1)?,
                    indexing: r.get::<_, i64>(2)?,
                    active_chunks: r.get::<_, i64>(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = %e, "Failed to list projects from SQLite.");
            ApiError::from(e)
        })?;

    Ok(Json(ProjectListResponse { projects }))
}

/// Aggregate statistics for one project.
///
/// `project_files` counted by status, plus chunks per language split into active vs
/// soft-deleted (pending GC).
///
/// **Concurrency:** safe — read-only, takes no locks.
#[utoipa::path(
    get,
    path = "/projects/{project_guid}",
    tag = "Projects",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    responses(
        (status = 200, description = "Per-status file counts and per-language chunk counts.", body = ProjectStats),
        (status = 404, description = "The project has never been seen.", body = ProblemDetails),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_project_stats(
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(s): State<RouterState>,
) -> Result<Json<ProjectStats>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;

    let result = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let exists = tx
                .query_row(
                    "SELECT 1 FROM projects WHERE guid = ?1",
                    params![pg],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Ok(None);
            }

            let mut files = FileStatusCounts::default();
            {
                let mut stmt = tx.prepare(
                    "SELECT status, COUNT(*) FROM project_files
                     WHERE project_guid = ?1 GROUP BY status",
                )?;
                let rows = stmt.query_map(params![pg], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                for row in rows {
                    let (status, n) = row?;
                    files.set(&status, n as u64);
                }
            }

            let mut chunks: HashMap<String, ChunkCounts> = HashMap::new();
            {
                let mut stmt = tx.prepare(
                    "SELECT f.programming_language, c.status, COUNT(*)
                     FROM project_file_chunks c
                     JOIN project_files f
                         ON c.project_guid = f.project_guid
                         AND c.model_id = f.model_id
                         AND c.file_path = f.path
                     WHERE c.project_guid = ?1
                     GROUP BY f.programming_language, c.status",
                )?;
                let rows = stmt.query_map(params![pg], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?;
                for row in rows {
                    let (lang, status, n) = row?;
                    let entry = chunks.entry(lang).or_default();
                    match status.as_str() {
                        "active" => entry.active = n as u64,
                        "deleted" => entry.deleted = n as u64,
                        _ => {}
                    }
                }
            }

            Ok(Some((files, chunks)))
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to query project stats from SQLite.");
            ApiError::from(e)
        })?;

    match result {
        Some((files, chunks)) => Ok(Json(ProjectStats {
            project_guid,
            files,
            chunks,
        })),
        None => Err(ApiError::ProjectNotFound),
    }
}

/// Hard-delete an entire project (immediate, not soft).
///
/// Removes all chunks, files, the project row, its status log, and finally drops the
/// Qdrant collection (last, so a retry re-attempts a failed drop even once the rows
/// are gone). Idempotent: deleting a non-existent project (or re-deleting) is a 204.
///
/// **Concurrency:** safe but destructive — unlike `DELETE /files` this is *not* a soft
/// delete and does not wait for GC. Avoid issuing it against a project with live
/// `/index` requests in flight.
#[utoipa::path(
    delete,
    path = "/projects/{project_guid}",
    tag = "Projects",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    responses(
        (status = 204, description = "Project deleted (or did not exist) — idempotent."),
        (status = 500, description = "SQLite delete or Qdrant collection-drop failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn delete_project(
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(s): State<RouterState>,
) -> Result<StatusCode, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let collection = collection_for(project_guid);
    let pg = project_guid;

    s.db_pool
        .transaction(guard.0.child_token(), move |tx| {
            tx.execute("DELETE FROM project_file_chunks WHERE project_guid = ?1", params![pg])?;
            tx.execute("DELETE FROM project_files WHERE project_guid = ?1", params![pg])?;
            tx.execute("DELETE FROM projects WHERE guid = ?1", params![pg])?;
            tx.execute("DELETE FROM project_file_status_log WHERE project_guid = ?1", params![pg])?;
            Ok(())
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to hard-delete project rows from SQLite.");
            ApiError::from(e)
        })?;

    s.qdrant.delete_collection(&collection).await.map_err(|e| {
        error!(
            error = %e,
            collection = %collection,
            "Failed to delete the Qdrant collection. Check Qdrant is reachable at --qdrant-server."
        );
        ApiError::Internal
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// Builds the `WHERE` body (without the keyword) and ordered binds selecting
/// `project_files` for `DELETE /files`. Mirrors the search filter: language
/// `IN`/`NOT IN` and path `GLOB` (ORed within a parenthesised group so OR cannot
/// leak across the AND-joined clauses), pinned to the project and excluding files
/// already `deleted`.
fn build_file_filter(
    project_guid: UUIDv4,
    include: &Option<SearchFilter>,
    exclude: &Option<SearchFilter>,
) -> (String, Vec<Bind>) {
    let mut n = 1usize;
    let mut parts = vec![
        format!("project_guid = ?{n}"),
        "status != 'deleted'".to_string(),
    ];
    n += 1;
    let mut binds: Vec<Bind> = vec![Bind::Guid(project_guid)];

    if let Some(inc) = include {
        if let Some(pls) = inc.programming_languages.as_ref().filter(|v| !v.is_empty()) {
            let ph: Vec<String> = pls
                .iter()
                .map(|_| {
                    let p = format!("?{n}");
                    n += 1;
                    p
                })
                .collect();
            parts.push(format!("programming_language IN ({})", ph.join(", ")));
            binds.extend(pls.iter().map(|l| Bind::Lang(*l)));
        }
        if let Some(paths) = inc.paths.as_ref().filter(|v| !v.is_empty()) {
            let cl: Vec<String> = paths
                .iter()
                .map(|_| {
                    let c = format!("path GLOB ?{n}");
                    n += 1;
                    c
                })
                .collect();
            parts.push(format!("({})", cl.join(" OR ")));
            binds.extend(paths.iter().map(|p| Bind::Path(p.0.as_str().to_string())));
        }
    }
    if let Some(exc) = exclude {
        if let Some(pls) = exc.programming_languages.as_ref().filter(|v| !v.is_empty()) {
            let ph: Vec<String> = pls
                .iter()
                .map(|_| {
                    let p = format!("?{n}");
                    n += 1;
                    p
                })
                .collect();
            parts.push(format!("programming_language NOT IN ({})", ph.join(", ")));
            binds.extend(pls.iter().map(|l| Bind::Lang(*l)));
        }
        if let Some(paths) = exc.paths.as_ref().filter(|v| !v.is_empty()) {
            let cl: Vec<String> = paths
                .iter()
                .map(|_| {
                    let c = format!("path GLOB ?{n}");
                    n += 1;
                    c
                })
                .collect();
            parts.push(format!("NOT ({})", cl.join(" OR ")));
            binds.extend(paths.iter().map(|p| Bind::Path(p.0.as_str().to_string())));
        }
    }

    (parts.join(" AND "), binds)
}

/// `DELETE /projects/{guid}/files` — soft-deletes files matching the selector:
/// marks their active chunks `deleted` and the files `deleted`; the next GC pass
/// (`POST /gc`, or the hourly worker) physically removes the vectors, the chunk
/// rows, and finally the empty file rows. Returns 204 when nothing matched, else
/// 200 with the count of files moved to `deleted`. A non-empty include/exclude is
/// required so an empty body cannot wipe the whole project.
///
/// **Concurrency:** safe — a soft delete (status flip), so it never races a live
/// `/index`/search the way an inline Qdrant delete would; physical removal is deferred
/// to GC. The empty-selector guard (**400**) prevents an accidental whole-project wipe.
#[utoipa::path(
    delete,
    path = "/projects/{project_guid}/files",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = DeleteFilesRequest,
    responses(
        (status = 200, description = "Files matched and soft-deleted.", body = DeleteFilesResponse),
        (status = 204, description = "The selector matched no files — nothing changed."),
        (status = 400, description = "Empty or oversized selector.", body = ProblemDetails),
        (status = 500, description = "SQLite failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn delete_files(
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(s): State<RouterState>,
    ApiJson(req): ApiJson<DeleteFilesRequest>,
) -> Result<Response, ApiError> {
    validate::require_nonempty_selector(&req.include, &req.exclude)?;
    validate::validate_selector(&req.include, s.max_selector_patterns)?;
    validate::validate_selector(&req.exclude, s.max_selector_patterns)?;

    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;
    let (where_sql, binds) = build_file_filter(pg, &req.include, &req.exclude);

    // 1) Resolve matching file paths (path globs evaluated by SQLite GLOB, as in search).
    let select_sql = format!("SELECT path FROM project_files WHERE {where_sql}");
    let paths: Vec<String> = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let mut stmt = tx.prepare(&select_sql)?;
            let rows = stmt.query_map(params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to select files for deletion.");
            ApiError::from(e)
        })?;

    if paths.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // 2) Soft-delete chunks + files, batched to stay under SQLite's bind-variable limit.
    let mut deleted_files: u64 = 0;
    for batch in paths.chunks(s.path_batch_size) {
        let batch: Vec<String> = batch.to_vec();
        let n = s
            .db_pool
            .transaction(guard.0.child_token(), move |tx| {
                let placeholders = (2..2 + batch.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut bs: Vec<Bind> = Vec::with_capacity(batch.len() + 1);
                bs.push(Bind::Guid(pg));
                bs.extend(batch.into_iter().map(Bind::Path));

                tx.execute(
                    &format!(
                        "UPDATE project_file_chunks SET status = 'deleted'
                         WHERE project_guid = ?1 AND status = 'active' AND file_path IN ({placeholders})"
                    ),
                    params_from_iter(bs.iter()),
                )?;
                let files = tx.execute(
                    &format!(
                        "UPDATE project_files SET status = 'deleted', status_updated_at = unixepoch()
                         WHERE project_guid = ?1 AND status != 'deleted' AND path IN ({placeholders})"
                    ),
                    params_from_iter(bs.iter()),
                )?;
                Ok(files as u64)
            })
            .with_cancellation_token(&guard.0)
            .await
            .from_cancelled()
            .map_err(|e| {
                error!(error = ?e, project_guid = %pg.0, "Failed to soft-delete files.");
                ApiError::from(e)
            })?;
        deleted_files += n;
    }

    if deleted_files == 0 {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((StatusCode::OK, Json(DeleteFilesResponse { deleted_files })).into_response())
    }
}

/// `POST /projects/{guid}/cancel` — best-effort cancel of in-flight indexing for the
/// files matching the selector. Only files in `status = 'indexing'` are touched: each
/// matched file's active chunks are marked `deleted` (the next GC pass removes any
/// vectors a racing embed already upserted) and the file moves `indexing → cancelled`
/// (a legal state-machine transition). Files already `indexed`/`failed`/etc. never
/// match, so their status is preserved — a cancellation that arrives after indexing
/// finished is a no-op. The live `/index` request reconciles against this at its
/// prepare→embed boundary, and the retry worker re-checks status after claiming, so a
/// cancelled file is neither re-embedded nor resurrected. Returns 204 when nothing
/// matched, else 200 with the count of files moved to `cancelled`. A non-empty
/// include/exclude is required so an empty body cannot blanket-cancel the project.
///
/// **Concurrency:** safe and intentionally lock-free — it deliberately does *not* take
/// the per-file indexing claim, so it can interrupt a held one. Correctness against a
/// live `/index` rests on re-reads (the indexer drops cancelled files before embedding;
/// the retry worker re-checks status after claiming), not a lock.
#[utoipa::path(
    post,
    path = "/projects/{project_guid}/cancel",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = CancelRequest,
    responses(
        (status = 200, description = "In-flight indexing cancelled for the matched files.", body = CancelResponse),
        (status = 204, description = "No `indexing` files matched (e.g. already finished) — nothing changed."),
        (status = 400, description = "Empty or oversized selector.", body = ProblemDetails),
        (status = 500, description = "SQLite failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_cancel(
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(s): State<RouterState>,
    ApiJson(req): ApiJson<CancelRequest>,
) -> Result<Response, ApiError> {
    validate::require_nonempty_selector(&req.include, &req.exclude)?;
    validate::validate_selector(&req.include, s.max_selector_patterns)?;
    validate::validate_selector(&req.exclude, s.max_selector_patterns)?;

    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;
    let (where_sql, binds) = build_file_filter(pg, &req.include, &req.exclude);

    // 1) Resolve matching file paths, restricted to those being indexed *right now*.
    //    `build_file_filter` already constrains `status != 'deleted'`; appending a
    //    constant predicate keeps the existing bind numbering intact (no new bind).
    let select_sql =
        format!("SELECT path FROM project_files WHERE {where_sql} AND status = 'indexing'");
    let paths: Vec<String> = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let mut stmt = tx.prepare(&select_sql)?;
            let rows = stmt.query_map(params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to select files to cancel.");
            ApiError::from(e)
        })?;

    if paths.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // 2) Soft-delete the active chunks + move the files 'indexing'→'cancelled', batched
    //    to stay under SQLite's bind-variable limit. Re-asserting status='indexing' in
    //    the file UPDATE makes it a no-op for any row that raced to 'indexed' between
    //    the SELECT and here (the trigger would reject cancelled→… otherwise).
    let mut cancelled_files: u64 = 0;
    for batch in paths.chunks(s.path_batch_size) {
        let batch: Vec<String> = batch.to_vec();
        let n = s
            .db_pool
            .transaction(guard.0.child_token(), move |tx| {
                let placeholders = (2..2 + batch.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut bs: Vec<Bind> = Vec::with_capacity(batch.len() + 1);
                bs.push(Bind::Guid(pg));
                bs.extend(batch.into_iter().map(Bind::Path));

                tx.execute(
                    &format!(
                        "UPDATE project_file_chunks SET status = 'deleted'
                         WHERE project_guid = ?1 AND status = 'active' AND file_path IN ({placeholders})"
                    ),
                    params_from_iter(bs.iter()),
                )?;
                let files = tx.execute(
                    &format!(
                        "UPDATE project_files SET status = 'cancelled', status_updated_at = unixepoch()
                         WHERE project_guid = ?1 AND status = 'indexing' AND path IN ({placeholders})"
                    ),
                    params_from_iter(bs.iter()),
                )?;
                Ok(files as u64)
            })
            .with_cancellation_token(&guard.0)
            .await
            .from_cancelled()
            .map_err(|e| {
                error!(error = ?e, project_guid = %pg.0, "Failed to cancel indexing files.");
                ApiError::from(e)
            })?;
        cancelled_files += n;
    }

    info!(project_guid = %pg.0, cancelled_files, "Cancelled in-flight indexing for matched files.");

    if cancelled_files == 0 {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((StatusCode::OK, Json(CancelResponse { cancelled_files })).into_response())
    }
}

/// `GET /projects/{guid}/files` — lists the project's files with per-file status,
/// language, content hash, active-chunk count, retry count, and last status change.
/// Optional `?status=` and `?language=` query filters narrow the set (e.g.
/// `?status=failed` is the dead-letter view). 404 if the project has never been seen
/// (mirrors `get_project_stats`); an empty file set on a known project is `200` with
/// `files: []`. Pure read — cancellation-safe, takes no locks.
///
/// **Concurrency:** safe — read-only. `?status=failed` is the dead-letter view.
#[utoipa::path(
    get,
    path = "/projects/{project_guid}/files",
    tag = "Projects",
    params(
        ("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form."),
        FileListQuery,
    ),
    responses(
        (status = 200, description = "Per-file listing (status / language / hash / chunk & retry counts).", body = FileListResponse),
        (status = 400, description = "Malformed query parameter (e.g. unknown language).", body = ProblemDetails),
        (status = 404, description = "The project has never been seen.", body = ProblemDetails),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_files(
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(s): State<RouterState>,
    ApiQuery(q): ApiQuery<FileListQuery>,
) -> Result<Json<FileListResponse>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;

    let result = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let exists = tx
                .query_row("SELECT 1 FROM projects WHERE guid = ?1", params![pg], |_| Ok(()))
                .optional()?
                .is_some();
            if !exists {
                return Ok(None);
            }

            // Optional status/language filters, numbered after the pinned project guid.
            let mut where_parts = vec!["f.project_guid = ?1".to_string()];
            let mut binds: Vec<Bind> = vec![Bind::Guid(pg)];
            let mut n = 2usize;
            if let Some(status) = q.status.as_ref() {
                where_parts.push(format!("f.status = ?{n}"));
                binds.push(Bind::Path(status.clone()));
                n += 1;
            }
            if let Some(lang) = q.language {
                where_parts.push(format!("f.programming_language = ?{n}"));
                binds.push(Bind::Lang(lang));
            }

            let sql = format!(
                "SELECT f.path, f.programming_language, f.status, f.sha256,
                        f.retry_count, f.status_updated_at,
                        (SELECT COUNT(*) FROM project_file_chunks c
                          WHERE c.project_guid = f.project_guid
                            AND c.model_id = f.model_id
                            AND c.file_path = f.path
                            AND c.status = 'active') AS chunk_count
                 FROM project_files f
                 WHERE {}
                 ORDER BY f.path",
                where_parts.join(" AND ")
            );
            let files = tx
                .prepare(&sql)?
                .query_map(params_from_iter(binds.iter()), |r| {
                    Ok(FileInfo {
                        path: r.get::<_, String>(0)?,
                        programming_language: r.get::<_, ProgrammingLanguage>(1)?,
                        status: r.get::<_, String>(2)?,
                        sha256: r.get::<_, String>(3)?,
                        retry_count: r.get::<_, i64>(4)?,
                        status_updated_at: r.get::<_, i64>(5)?,
                        chunk_count: r.get::<_, i64>(6)? as u64,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)?;
            Ok(Some(files))
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to list project files from SQLite.");
            ApiError::from(e)
        })?;

    match result {
        Some(files) => Ok(Json(FileListResponse { files })),
        None => Err(ApiError::ProjectNotFound),
    }
}

/// `POST /projects/{guid}/retry` — requeues `failed` files for the retry worker by
/// resetting their retry counter. The `include`/`exclude` selector (same shape as
/// cancel/delete) is **optional**: an empty body requeues *every* `failed` file —
/// retry is non-destructive, so a blanket dead-letter recovery is the useful default.
///
/// This is a **metadata-only** write: `status` stays `failed`, so it never passes
/// through the state-machine triggers (no transition to reject) and never takes the
/// per-file `IndexClaim`. It deliberately leaves `status_updated_at` untouched — the
/// retry worker only picks a `failed` file whose `status_updated_at` is older than
/// 60s, so keeping the old timestamp lets the next sweep (≤60s) re-embed it at once;
/// bumping it would add a needless 60s delay. It races benignly with that worker,
/// which re-checks status under its own claim. Returns 204 when nothing matched, else
/// 200 with the count of files requeued.
///
/// **Concurrency:** safe — a metadata-only write (`retry_count = 0`) that skips the
/// state-machine triggers and takes no claim. It races benignly with the retry worker.
/// An empty body requeues *every* `failed` file (retry is non-destructive).
#[utoipa::path(
    post,
    path = "/projects/{project_guid}/retry",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = RetryRequest,
    responses(
        (status = 200, description = "Matched `failed` files requeued for the retry worker.", body = RetryResponse),
        (status = 204, description = "No `failed` files matched — nothing changed."),
        (status = 400, description = "Oversized selector.", body = ProblemDetails),
        (status = 500, description = "SQLite failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_retry(
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(s): State<RouterState>,
    ApiJson(req): ApiJson<RetryRequest>,
) -> Result<Response, ApiError> {
    // Retry deliberately allows an empty body (= every `failed` file), so no
    // non-empty-selector requirement — only the pattern-count cap applies.
    validate::validate_selector(&req.include, s.max_selector_patterns)?;
    validate::validate_selector(&req.exclude, s.max_selector_patterns)?;

    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;
    let (where_sql, binds) = build_file_filter(pg, &req.include, &req.exclude);

    // `build_file_filter` already pins the project (and excludes 'deleted'); appending
    // a constant `status = 'failed'` keeps the existing bind numbering intact.
    let update_sql =
        format!("UPDATE project_files SET retry_count = 0 WHERE {where_sql} AND status = 'failed'");
    let requeued_files: u64 = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let n = tx.execute(&update_sql, params_from_iter(binds.iter()))?;
            Ok(n as u64)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to requeue failed files.");
            ApiError::from(e)
        })?;

    info!(project_guid = %pg.0, requeued_files, "Requeued failed files for the retry worker.");

    if requeued_files == 0 {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((StatusCode::OK, Json(RetryResponse { requeued_files })).into_response())
    }
}

/// `GET /status` — a live runtime/concurrency snapshot for diagnostics: how many
/// per-file indexing claims are held right now, whether a GC pass is running, SQLite
/// pool headroom, and global `project_files` counts by status. Cheap — one grouped
/// SQLite read plus two in-memory reads. Distinct from `GET /health` (dependency
/// liveness) and `GET /config` (static knobs).
///
/// **Concurrency:** safe — read-only. This is the endpoint to inspect *why* you saw a
/// 409 (`gc_running`) or a 500 (`pool_available` at 0); `indexing_claims` shows how
/// many files are mid-pipeline (same-file collisions are now skipped, not 429).
#[utoipa::path(
    get,
    path = "/status",
    tag = "Observability",
    responses(
        (status = 200, description = "Live runtime/concurrency snapshot.", body = StatusResponse),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_status(State(s): State<RouterState>) -> Result<Json<StatusResponse>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());

    let counts: Vec<(String, i64)> = s
        .db_pool
        .transaction(guard.0.child_token(), |tx| {
            tx.prepare("SELECT status, COUNT(*) FROM project_files GROUP BY status")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = %e, "Failed to read global file-status counts from SQLite.");
            ApiError::from(e)
        })?;

    let mut files_by_status = FileStatusCounts::default();
    for (status, n) in &counts {
        files_by_status.set(status, *n as u64);
    }
    let indexing_files = files_by_status.indexing as i64;

    // In-memory state: the per-file claim table and the GC flag. Recover from a
    // poisoned lock rather than panic (it is a plain membership set — see `IndexClaim`).
    let indexing_claims = s
        .indexing_locks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();
    let gc_running = s.gc_flag.load(std::sync::atomic::Ordering::Acquire);

    Ok(Json(StatusResponse {
        indexing_claims,
        gc_running,
        pool_available: s.db_pool.available().await,
        pool_size: s.db_pool.size(),
        indexing_files,
        files_by_status,
    }))
}

/// `GET /config` — static server capabilities and tuning knobs (the running version,
/// the embedding model, the canonical supported-language list, and the CLI-set
/// concurrency knobs). The `languages` array is the single source of truth clients
/// (e.g. the search frontend) read instead of hardcoding their own copy.
///
/// **Concurrency:** safe — returns static, in-memory values; no I/O, no locks.
#[utoipa::path(
    get,
    path = "/config",
    tag = "Config",
    responses(
        (status = 200, description = "Static capabilities and tuning knobs, incl. the canonical language list.", body = ConfigResponse),
    ),
)]
#[debug_handler]
pub async fn get_config(State(s): State<RouterState>) -> Json<ConfigResponse> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    Json(ConfigResponse {
        version: env!("CARGO_PKG_VERSION"),
        model_id: model_id.clone(),
        languages: ProgrammingLanguage::ALL.iter().map(|l| l.name()).collect(),
        embed_batch: s.embed_tuning.embed_batch,
        db_pool_size: s.db_pool_size,
        stuck_grace_mins: s.stuck_grace_mins,
        max_retries: s.max_retries,
    })
}

/// `POST /gc` — runs a full GC pass synchronously and returns what it removed:
/// hard-deletes soft-deleted chunks (whose vectors are confirmed gone from Qdrant),
/// then the now-empty `deleted` file rows, then prunes the old status log. Blocking
/// by design; the periodic worker runs the same steps hourly. GC is global, so a
/// pass is serialized process-wide by `GcGuard`: a `POST /gc` arriving while one is
/// already running (a concurrent call or the hourly worker's tick) returns 409.
///
/// **Concurrency:** safe but globally serialized — GC is process-wide, so only one pass
/// runs at a time; a concurrent request (or one racing the hourly worker) gets **409**.
/// It only hard-deletes chunks whose Qdrant vectors are confirmed gone, so it never
/// orphans a vector. Synchronous: the response returns when the pass completes.
#[utoipa::path(
    post,
    path = "/gc",
    tag = "Garbage Collection",
    responses(
        (status = 200, description = "GC pass completed; counts of what was physically removed.", body = GcResponse),
        (status = 409, description = "A GC pass is already running (manual or the hourly worker) — retry later.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_gc(State(s): State<RouterState>) -> Result<Json<GcResponse>, ApiError> {
    let Some(_guard) = crate::worker::gc::GcGuard::try_acquire(&s.gc_flag) else {
        info!("POST /gc rejected: a garbage-collection pass is already running.");
        return Err(ApiError::GcRunning);
    };
    let cg = http3::CancellationGuard(CancellationToken::new());
    let (chunks_removed, files_removed, status_log_pruned) =
        crate::worker::gc::collect(&s.db_pool, &*s.qdrant, s.status_log_retention_days, &cg.0).await;
    Ok(Json(GcResponse {
        chunks_removed,
        files_removed,
        status_log_pruned,
    }))
}

/// Running mindex version (also a trivial liveness ping).
///
/// **Concurrency:** safe — constant, no I/O.
#[utoipa::path(
    get,
    path = "/version",
    tag = "Observability",
    responses((status = 200, description = "The running mindex version.", body = VersionResponse)),
)]
#[debug_handler]
pub async fn get_version(State(s): State<RouterState>) -> Json<VersionResponse> {
    Json(VersionResponse { version: env!("CARGO_PKG_VERSION"), db_schema_version: s.db_schema_version })
}

/// `GET /health` — a *smart* readiness check: confirms both stores (SQLite +
/// Qdrant) and the embedder are reachable, and reports how many files are indexing
/// globally. `status` is `"ok"` only if all three checks pass. Each check is
/// best-effort and independent, so one dead dependency is pinpointed rather than
/// collapsing the whole response.
///
/// **Concurrency:** safe — read-only probes. Always returns **200** at the HTTP level;
/// inspect the `status` field (`"ok"` vs `"degraded"`) and per-dependency `checks`.
#[utoipa::path(
    get,
    path = "/health",
    tag = "Observability",
    responses((status = 200, description = "Dependency liveness; `status` is `ok` only if all three checks pass.", body = HealthResponse)),
)]
#[debug_handler]
pub async fn get_health(State(s): State<RouterState>) -> Json<HealthResponse> {
    let guard = http3::CancellationGuard(CancellationToken::new());

    // SQLite: the global indexing-file count doubles as the liveness query.
    let (sqlite, indexing_files) = match s
        .db_pool
        .transaction(guard.0.child_token(), |tx| {
            tx.query_row(
                "SELECT COUNT(*) FROM project_files WHERE status = 'indexing'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
    {
        Ok(n) => ("ok".to_string(), n),
        Err(e) => (format!("error: {e}"), -1),
    };

    let qdrant = match s.qdrant.health().await {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("error: {e}"),
    };

    let EmbeddingModel::BGEm3 { client, .. } = &s.model;
    let embedder = match client.health().await {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("error: {e:?}"),
    };

    let status = if sqlite == "ok" && qdrant == "ok" && embedder == "ok" {
        "ok"
    } else {
        "degraded"
    };

    Json(HealthResponse {
        status,
        version: env!("CARGO_PKG_VERSION"),
        indexing_files,
        checks: HealthChecks { sqlite, qdrant, embedder },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::v0::models::{GlobPattern, SearchFilter};
    use glob::Pattern;
    use uuid::Uuid;

    fn guid() -> UUIDv4 {
        UUIDv4(Uuid::nil())
    }

    fn glob(s: &str) -> GlobPattern {
        GlobPattern(Pattern::new(s).unwrap())
    }

    fn req(include: Option<SearchFilter>, exclude: Option<SearchFilter>) -> SearchRequest {
        SearchRequest {
            query: "q".into(),
            top_k: None,
            include,
            exclude,
        }
    }

    fn langs(v: Vec<ProgrammingLanguage>) -> SearchFilter {
        SearchFilter {
            paths: None,
            programming_languages: Some(v),
        }
    }

    fn paths(v: &[&str]) -> SearchFilter {
        SearchFilter {
            paths: Some(v.iter().map(|s| glob(s)).collect()),
            programming_languages: None,
        }
    }

    // ── hash-skip gating (regression) ───────────────────────────────────
    // The sha256 is written at `indexing` time (the column is NOT NULL), so a
    // file that was sliced but never embedded (embedder down → `failed`) carries
    // the correct hash without any vectors. The unchanged-skip must therefore key
    // on `status = 'indexed'`, not the hash alone — otherwise such a file is
    // skipped on every later re-index and never gets embedded.
    use crate::db::sqlite3::SQLite3Pool;
    use std::path::Path as FsPath;
    use tokio_util::sync::CancellationToken;

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    async fn pool_with_file(status: &'static str, sha: &'static str) -> SQLite3Pool {
        // Pool size 1: the single ":memory:" connection is reused, so the row
        // inserted below is visible to the later hash-check transaction.
        let p = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        p.transaction(CancellationToken::new(), move |tx| {
            tx.execute_batch(
                "CREATE TABLE project_files (
                     project_guid TEXT NOT NULL,
                     path         TEXT NOT NULL,
                     model_id     TEXT NOT NULL,
                     sha256       TEXT NOT NULL,
                     status       TEXT NOT NULL
                 );",
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, path, model_id, sha256, status)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![guid(), "src/a.py", "m", sha, status],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        p
    }

    async fn already_indexed(p: &SQLite3Pool, sha: &'static str) -> bool {
        p.transaction(CancellationToken::new(), move |tx| {
            file_already_indexed(tx, guid(), "src/a.py", "m", sha)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn failed_file_with_matching_hash_is_not_skipped() {
        // A file left `failed` (embedder was down) keeps its content hash but has
        // no vectors — it must be re-indexed, not treated as unchanged.
        let p = pool_with_file("failed", SHA).await;
        assert!(
            !already_indexed(&p, SHA).await,
            "a `failed` file with a matching hash must NOT be skipped"
        );
    }

    #[tokio::test]
    async fn indexing_file_with_matching_hash_is_not_skipped() {
        let p = pool_with_file("indexing", SHA).await;
        assert!(
            !already_indexed(&p, SHA).await,
            "an in-flight `indexing` file must NOT be skipped"
        );
    }

    #[tokio::test]
    async fn indexed_file_with_matching_hash_is_skipped() {
        let p = pool_with_file("indexed", SHA).await;
        assert!(
            already_indexed(&p, SHA).await,
            "a successfully `indexed` file with a matching hash should be skipped"
        );
    }

    #[tokio::test]
    async fn indexed_file_with_changed_hash_is_not_skipped() {
        let p = pool_with_file("indexed", SHA).await;
        let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert!(
            !already_indexed(&p, other).await,
            "changed content must be re-indexed"
        );
    }

    #[test]
    fn no_filters_pins_project_and_active_status() {
        let (sql, binds) = build_search_query(guid(), &req(None, None));
        assert!(sql.contains("c.project_guid = ?1"));
        assert!(sql.contains("c.status = 'active'"));
        // No filter clauses beyond the two mandatory ones.
        assert!(!sql.contains("programming_language"));
        assert!(!sql.contains("GLOB"));
        assert_eq!(binds, vec![Bind::Guid(guid())]);
    }

    #[test]
    fn include_languages_numbered_from_two() {
        let (sql, binds) = build_search_query(
            guid(),
            &req(
                Some(langs(vec![
                    ProgrammingLanguage::Rust,
                    ProgrammingLanguage::Python,
                ])),
                None,
            ),
        );
        assert!(
            sql.contains("f.programming_language IN (?2, ?3)"),
            "sql was: {sql}"
        );
        assert_eq!(
            binds,
            vec![
                Bind::Guid(guid()),
                Bind::Lang(ProgrammingLanguage::Rust),
                Bind::Lang(ProgrammingLanguage::Python),
            ]
        );
    }

    #[test]
    fn include_paths_use_glob_or() {
        let (sql, binds) =
            build_search_query(guid(), &req(Some(paths(&["src/**", "tests/**"])), None));
        assert!(
            sql.contains("c.file_path GLOB ?2 OR c.file_path GLOB ?3"),
            "sql was: {sql}"
        );
        assert_eq!(
            binds,
            vec![
                Bind::Guid(guid()),
                Bind::Path("src/**".into()),
                Bind::Path("tests/**".into()),
            ]
        );
    }

    #[test]
    fn include_langs_and_paths_continue_numbering() {
        // langs take ?2,?3 then paths take ?4,?5 — the bind order must match.
        let inc = SearchFilter {
            paths: Some(vec![glob("a/**"), glob("b/**")]),
            programming_languages: Some(vec![ProgrammingLanguage::Go, ProgrammingLanguage::Sql]),
        };
        let (sql, binds) = build_search_query(guid(), &req(Some(inc), None));
        assert!(
            sql.contains("f.programming_language IN (?2, ?3)"),
            "sql was: {sql}"
        );
        assert!(
            sql.contains("c.file_path GLOB ?4 OR c.file_path GLOB ?5"),
            "sql was: {sql}"
        );
        assert_eq!(
            binds,
            vec![
                Bind::Guid(guid()),
                Bind::Lang(ProgrammingLanguage::Go),
                Bind::Lang(ProgrammingLanguage::Sql),
                Bind::Path("a/**".into()),
                Bind::Path("b/**".into()),
            ]
        );
    }

    #[test]
    fn exclude_languages_use_not_in() {
        let (sql, _) = build_search_query(
            guid(),
            &req(None, Some(langs(vec![ProgrammingLanguage::Json]))),
        );
        assert!(
            sql.contains("f.programming_language NOT IN (?2)"),
            "sql was: {sql}"
        );
    }

    #[test]
    fn exclude_paths_are_negated() {
        let (sql, binds) = build_search_query(guid(), &req(None, Some(paths(&["vendor/**"]))));
        assert!(sql.contains("NOT (c.file_path GLOB ?2)"), "sql was: {sql}");
        assert_eq!(
            binds,
            vec![Bind::Guid(guid()), Bind::Path("vendor/**".into())]
        );
    }

    // ── include-glob OR precedence (regression, red until fixed) ────────────
    // Multiple include globs are ORed; without parentheses the OR leaks past the
    // AND-joined project/status pins: `pin AND pin AND g1 OR g2` parses as
    // `(pin AND pin AND g1) OR g2`, so the second glob matches soft-deleted
    // chunks and other projects' chunks.

    #[test]
    fn include_path_glob_group_is_parenthesized() {
        let (sql, _) =
            build_search_query(guid(), &req(Some(paths(&["src/**", "tests/**"])), None));
        assert!(
            sql.contains("(c.file_path GLOB ?2 OR c.file_path GLOB ?3)"),
            "include glob group must be parenthesized so OR cannot leak past AND: {sql}"
        );
    }

    #[tokio::test]
    async fn include_paths_do_not_leak_foreign_or_deleted_chunks() {
        let p1 = UUIDv4(Uuid::from_u128(1));
        let p2 = UUIDv4(Uuid::from_u128(2));
        let ga = UUIDv4(Uuid::from_u128(0xA)); // P1, active, src/a.rs → expected
        let gt = UUIDv4(Uuid::from_u128(0xB)); // P1, deleted, tests/t.rs → excluded
        let gx = UUIDv4(Uuid::from_u128(0xC)); // P2, active, tests/x.rs → excluded

        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            for (pg, path, chunk_guid, chunk_status) in [
                (p1, "src/a.rs", ga, "active"),
                (p1, "tests/t.rs", gt, "deleted"),
                (p2, "tests/x.rs", gx, "active"),
            ] {
                tx.execute(
                    "INSERT OR IGNORE INTO projects (guid, model_id)
                     VALUES (?1, 'BAAI/bge-m3')",
                    params![pg],
                )?;
                tx.execute(
                    "INSERT INTO project_files
                         (project_guid, model_id, path, sha256, programming_language, status)
                     VALUES (?1, 'BAAI/bge-m3', ?2, ?3, 'rust', 'indexing')",
                    params![pg, path, "0".repeat(64)],
                )?;
                tx.execute(
                    "INSERT INTO project_file_chunks
                         (project_guid, file_path, model_id, code, qdrant_guid,
                          start_line, end_line, start_column, end_column, status)
                     VALUES (?1, ?2, 'BAAI/bge-m3', 'code', ?3, 1, 2, 0, 1, ?4)",
                    params![pg, path, chunk_guid, chunk_status],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

        let (sql, binds) =
            build_search_query(p1, &req(Some(paths(&["src/**", "tests/**"])), None));
        let got: Vec<UUIDv4> = pool
            .transaction(CancellationToken::new(), move |tx| {
                tx.prepare(&sql)?
                    .query_map(params_from_iter(binds), |r| r.get::<_, UUIDv4>(0))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(SQLite3PoolError::from)
            })
            .await
            .unwrap();

        assert_eq!(
            got,
            vec![ga],
            "candidate set must contain only the project's own active chunks \
             matching the include globs — no soft-deleted or foreign-project chunks"
        );
    }

    // ── sha256 refresh at indexing start (regression, red until fixed) ──────
    // The prepare upsert must refresh sha256 on reindex of an existing row.
    // Otherwise a crash/embed-failure recovered by the retry worker (which marks
    // 'indexed' via set_file_status, never touching sha256) leaves the row with
    // the OLD content hash next to the NEW content's chunks — and a later revert
    // of the file to the old content is hash-skipped forever, serving stale chunks.
    #[tokio::test]
    async fn reindex_upsert_refreshes_sha256_at_indexing_start() {
        const SHA_OLD: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        const SHA_NEW: &str = "2222222222222222222222222222222222222222222222222222222222222222";

        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, 'BAAI/bge-m3')",
                params![guid()],
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, 'BAAI/bge-m3', 'src/a.py', ?2, 'python', 'indexing')",
                params![guid(), SHA_OLD],
            )?;
            // indexing → indexed: the state a previously indexed file sits in.
            tx.execute(
                "UPDATE project_files SET status = 'indexed' WHERE project_guid = ?1",
                params![guid()],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        // Reindex with changed content: the exact production prepare upsert.
        pool.transaction(CancellationToken::new(), |tx| {
            tx.execute(
                MARK_INDEXING_UPSERT_SQL,
                params![guid(), "src/a.py", SHA_NEW, ProgrammingLanguage::Python, "BAAI/bge-m3"],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let (status, sha): (String, String) = pool
            .transaction(CancellationToken::new(), |tx| {
                tx.query_row(
                    "SELECT status, sha256 FROM project_files WHERE project_guid = ?1",
                    params![guid()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(SQLite3PoolError::from)
            })
            .await
            .unwrap();

        assert_eq!(status, "indexing");
        assert_eq!(
            sha, SHA_NEW,
            "the prepare upsert must refresh sha256 on conflict — a retry-worker \
             recovery marks 'indexed' without writing sha256, so a stale stored hash \
             would desync from the freshly inserted chunks"
        );
    }

    #[test]
    fn include_and_exclude_share_one_counter() {
        // include langs ?2; exclude paths ?3 — numbering is global across both.
        let (sql, binds) = build_search_query(
            guid(),
            &req(
                Some(langs(vec![ProgrammingLanguage::Rust])),
                Some(paths(&["target/**"])),
            ),
        );
        assert!(
            sql.contains("f.programming_language IN (?2)"),
            "sql was: {sql}"
        );
        assert!(sql.contains("NOT (c.file_path GLOB ?3)"), "sql was: {sql}");
        assert_eq!(
            binds,
            vec![
                Bind::Guid(guid()),
                Bind::Lang(ProgrammingLanguage::Rust),
                Bind::Path("target/**".into()),
            ]
        );
    }

    // ── drift ────────────────────────────────────────────────────────────────

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(p, s)| (p.to_string(), s.to_string())).collect()
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn drift_classifies_all_four_buckets() {
        let indexed = map(&[("same.rs", "h1"), ("changed.rs", "h2"), ("gone.rs", "h3")]);
        let in_flight = set(&["busy.rs"]);
        let local = map(&[
            ("same.rs", "h1"),     // in sync → omitted
            ("changed.rs", "hX"),  // hash differs → stale
            ("new.rs", "h9"),      // not indexed → missing
            ("busy.rs", "hY"),     // in flight → indexing
            // gone.rs absent locally → orphaned
        ]);

        let d = compute_drift(&indexed, &in_flight, &local);

        assert_eq!(d.stale, vec!["changed.rs"]);
        assert_eq!(d.missing, vec!["new.rs"]);
        assert_eq!(d.orphaned, vec!["gone.rs"]);
        assert_eq!(d.indexing, vec!["busy.rs"]);
    }

    #[test]
    fn drift_empty_baseline_makes_everything_missing() {
        let d = compute_drift(&map(&[]), &set(&[]), &map(&[("a.rs", "h"), ("b.rs", "h")]));
        assert_eq!(d.missing, vec!["a.rs", "b.rs"]);
        assert!(d.stale.is_empty() && d.orphaned.is_empty() && d.indexing.is_empty());
    }

    #[test]
    fn drift_in_flight_never_stale_or_missing_even_when_hash_differs() {
        // An indexing row's stored sha256 is the *old* value, so a hash mismatch on
        // an in-flight file must NOT surface as stale/missing — only `indexing`.
        let indexed = map(&[("f.rs", "old_hash")]);
        let in_flight = set(&["f.rs"]);
        let local = map(&[("f.rs", "new_hash")]);

        let d = compute_drift(&indexed, &in_flight, &local);

        assert_eq!(d.indexing, vec!["f.rs"]);
        assert!(d.stale.is_empty());
        assert!(d.missing.is_empty());
        assert!(d.orphaned.is_empty(), "in-flight file must not be called orphaned");
    }

    // ── keyed indexing claim ─────────────────────────────────────────────────

    #[test]
    fn index_claim_is_exclusive_and_releases_on_drop() {
        let locks: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let key = "guid\u{0}model\u{0}path".to_string();

        let first = IndexClaim::try_acquire(&locks, key.clone());
        assert!(first.is_some(), "first claim should succeed");

        // A second claim on the same key is refused while the first is held.
        assert!(
            IndexClaim::try_acquire(&locks, key.clone()).is_none(),
            "concurrent claim on the same key must be refused"
        );

        drop(first); // release

        // After release the key is claimable again.
        assert!(
            IndexClaim::try_acquire(&locks, key).is_some(),
            "key should be claimable again after the holder drops"
        );
    }

    #[test]
    fn claim_lifetime_is_bound_to_prepared_for_the_whole_pipeline() {
        // Regression guard for the "stale recover clobbers a later index" race:
        // the per-file claim is owned by `Prepared._claim`, and `post_index` keeps
        // the `Vec<Prepared>` in scope through embed_all AND mark_indexed/recover_all.
        // So a request that goes on to *fail* still holds the lock while it recovers
        // to `failed`; no second request for the same file can start (the contended
        // file is skipped) until the first fully terminates. This is what makes the interleaving
        // "req1 releases → req3 reindexes → req1's late `failed` lands" impossible:
        // req1 never releases mid-pipeline.
        let locks: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let key = indexing_lock_key("guid", "model", "f.rs");

        let claim = IndexClaim::try_acquire(&locks, key.clone()).expect("first acquires");
        let prepared = Prepared {
            pl: ProgrammingLanguage::Rust,
            path: "f.rs".to_string(),
            sha256: "h".to_string(),
            chunks: Vec::new(),
            _claim: claim,
        };

        // While the first request's `Prepared` is alive — anywhere from slice through
        // embed through recover — every other same-file request is refused.
        assert!(
            IndexClaim::try_acquire(&locks, key.clone()).is_none(),
            "same-file claim must be refused for the whole pipeline, not just until slicing"
        );

        // Only when the pipeline ends (the `Prepared`, hence the claim, drops) does
        // the slot free up — at which point any next request sees the terminal state.
        drop(prepared);
        assert!(
            IndexClaim::try_acquire(&locks, key).is_some(),
            "slot must free up once the holding Prepared drops at end of post_index"
        );
    }

    // ── retry requeue ────────────────────────────────────────────────────────

    /// `post_retry`'s UPDATE resets `retry_count` on a `failed` file and — critically
    /// — leaves `status_updated_at` untouched. The retry worker only picks a `failed`
    /// file whose timestamp is older than 60s, so preserving the old stamp lets the
    /// next sweep re-embed it immediately instead of after another grace window.
    #[tokio::test]
    async fn retry_resets_count_and_preserves_timestamp() {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![guid(), "BAAI/bge-m3"],
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, ?2, 'a.rs', ?3, 'rust', 'indexing')",
                params![guid(), "BAAI/bge-m3", "0".repeat(64)],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        // Reach `failed` legally (indexing → failed), then pin a maxed retry_count and
        // an old status_updated_at directly — both are plain column writes, not status
        // transitions, so no trigger fires.
        let pg = guid().0.as_simple().to_string();
        set_file_status(&pool, &pg, "a.rs", "BAAI/bge-m3", "failed", true, CancellationToken::new()).await;
        pool.transaction(CancellationToken::new(), |tx| {
            tx.execute(
                "UPDATE project_files SET retry_count = 9, status_updated_at = 1000
                 WHERE project_guid = ?1 AND path = 'a.rs'",
                params![guid()],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        // Run exactly what `post_retry` runs: an empty selector (requeue all) plus the
        // constant `status = 'failed'`.
        let (where_sql, binds) = build_file_filter(guid(), &None, &None);
        let update_sql =
            format!("UPDATE project_files SET retry_count = 0 WHERE {where_sql} AND status = 'failed'");
        let n = pool
            .transaction(CancellationToken::new(), move |tx| {
                Ok(tx.execute(&update_sql, params_from_iter(binds.iter()))?)
            })
            .await
            .unwrap();
        assert_eq!(n, 1, "the failed file should be requeued");

        let (status, retry_count, updated): (String, i64, i64) = pool
            .transaction(CancellationToken::new(), |tx| {
                tx.query_row(
                    "SELECT status, retry_count, status_updated_at FROM project_files
                     WHERE project_guid = ?1 AND path = 'a.rs'",
                    params![guid()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(SQLite3PoolError::from)
            })
            .await
            .unwrap();

        assert_eq!(status, "failed", "metadata-only write: status must stay 'failed'");
        assert_eq!(retry_count, 0, "retry_count must be reset so the worker re-picks it");
        assert_eq!(updated, 1000, "status_updated_at must NOT be bumped (else a +60s delay)");
    }

    // ── phase-1 → phase-2 reconciliation (`drop_cancelled`) and batch recovery
    //    (`recover_all`) ─────────────────────────────────────────────────────────
    // These are the correctness core of a concurrent `POST /cancel` against a live
    // `/index`: a file flipped out of `indexing` between prepare and embed must be
    // dropped from the batch (its fresh chunks soft-deleted for GC), and an aborted
    // batch must hand every prepared file to the retry worker — none may stay
    // `indexing` with no one working on it.

    use crate::db::files::set_file_status;
    use crate::db::qdrant::{ChunkAsVector, SearchHit, VectorStoreError};
    use crate::models::bge_m3::{BGEm3EmbedRequest, BGEm3EmbedResponse, EncodeError};
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::Mutex;

    const MODEL: &str = "BAAI/bge-m3";

    /// Neither seam may be touched by `drop_cancelled`/`recover_all` — they are
    /// SQLite-only paths. Any call is a test failure.
    struct NoStore;
    #[async_trait]
    impl VectorStore for NoStore {
        async fn insert_batch(&self, _c: &str, _v: Vec<ChunkAsVector>) -> Result<(), VectorStoreError> {
            unreachable!("drop_cancelled/recover_all must not touch Qdrant")
        }
        async fn ensure_project(&self, _c: &str) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn delete_collection(&self, _c: &str) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn health(&self) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn delete_batch(&self, _c: &str, _g: Vec<String>) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn search(
            &self,
            _c: &str,
            _i: Vec<UUIDv4>,
            _d: Vec<f32>,
            _si: Vec<u32>,
            _sv: Vec<f32>,
            _cb: Vec<Vec<f32>>,
            _k: u64,
        ) -> Result<Vec<SearchHit>, VectorStoreError> {
            unreachable!()
        }
    }

    struct NoEmbedder;
    #[async_trait]
    impl crate::models::bge_m3::BGEm3Model for NoEmbedder {
        async fn encode(
            &self,
            _req: BGEm3EmbedRequest,
            _token: CancellationToken,
        ) -> Result<BGEm3EmbedResponse, EncodeError> {
            unreachable!("drop_cancelled/recover_all must not call the embedder")
        }
        async fn health(&self) -> Result<(), EncodeError> {
            unreachable!()
        }
    }

    /// Migrated pool with the project and `paths` each inserted `indexing` with
    /// `n_chunks` active chunks — the exact state `prepare` leaves behind.
    async fn pool_with_prepared_files(paths: &'static [&'static str], n_chunks: usize) -> SQLite3Pool {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![guid(), MODEL],
            )?;
            for path in paths {
                tx.execute(
                    "INSERT INTO project_files
                         (project_guid, model_id, path, sha256, programming_language, status)
                     VALUES (?1, ?2, ?3, ?4, 'rust', 'indexing')",
                    params![guid(), MODEL, path, "0".repeat(64)],
                )?;
                for _ in 0..n_chunks {
                    tx.execute(
                        "INSERT INTO project_file_chunks
                             (project_guid, file_path, model_id, code, qdrant_guid,
                              start_line, end_line, start_column, end_column, status)
                         VALUES (?1, ?2, ?3, 'code', ?4, 1, 2, 0, 1, 'active')",
                        params![guid(), path, MODEL, Uuid::new_v4().simple().to_string()],
                    )?;
                }
            }
            Ok(())
        })
        .await
        .unwrap();
        pool
    }

    /// Builds a `Prepared` for `path` the way `prepare` would: claim held, chunks
    /// carried. The chunk list content is irrelevant to the paths under test.
    fn prepared_for(locks: &Arc<Mutex<HashSet<String>>>, path: &str) -> Prepared {
        let key = indexing_lock_key(&guid().0.as_simple().to_string(), MODEL, path);
        Prepared {
            pl: ProgrammingLanguage::Rust,
            path: path.to_string(),
            sha256: "0".repeat(64),
            chunks: vec![(UUIDv4(Uuid::new_v4()), "code".to_string())],
            _claim: IndexClaim::try_acquire(locks, key).expect("slot starts free"),
        }
    }

    /// (status, active_chunks, deleted_chunks) for one path.
    async fn file_state(pool: &SQLite3Pool, path: &'static str) -> (String, i64, i64) {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.query_row(
                "SELECT f.status,
                        (SELECT COUNT(*) FROM project_file_chunks c
                         WHERE c.project_guid = f.project_guid AND c.file_path = f.path
                           AND c.model_id = f.model_id AND c.status = 'active'),
                        (SELECT COUNT(*) FROM project_file_chunks c
                         WHERE c.project_guid = f.project_guid AND c.file_path = f.path
                           AND c.model_id = f.model_id AND c.status = 'deleted')
                 FROM project_files f
                 WHERE f.project_guid = ?1 AND f.path = ?2 AND f.model_id = ?3",
                params![guid(), path, MODEL],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    /// The owned pieces a test-local `FileIndexer` borrows (kept alive by the caller).
    struct IndexerFixture {
        tokenizer: Arc<Tokenizer>,
        token: CancellationToken,
    }

    fn fixture() -> IndexerFixture {
        IndexerFixture {
            tokenizer: Arc::new(Tokenizer::new(
                tokenizers::models::wordlevel::WordLevel::default(),
            )),
            token: CancellationToken::new(),
        }
    }

    /// A `FileIndexer` wired to fakes that reject any Qdrant/embedder call (the
    /// paths under test are SQLite-only).
    fn indexer<'a>(
        pool: &'a SQLite3Pool,
        locks: &'a Arc<Mutex<HashSet<String>>>,
        fx: &'a IndexerFixture,
    ) -> FileIndexer<'a> {
        FileIndexer {
            db_pool: pool,
            store: &NoStore,
            tokenizer: &fx.tokenizer,
            embedder: &NoEmbedder,
            model_id: MODEL,
            project_guid: guid(),
            collection: "unused",
            embed_tuning: crate::embed::EmbedTuning {
                embed_batch: 64,
                upsert_batch: 256,
                sparse_min_weight: 1e-5,
            },
            min_chunk_tokens: 128,
            max_chunk_tokens: 512,
            token: &fx.token,
            indexing_locks: locks,
        }
    }

    #[tokio::test]
    async fn drop_cancelled_keeps_files_still_indexing() {
        let pool = pool_with_prepared_files(&["a.rs", "b.rs"], 2).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let prepared = vec![prepared_for(&locks, "a.rs"), prepared_for(&locks, "b.rs")];

        let fx = fixture();
        let kept = indexer(&pool, &locks, &fx).drop_cancelled(prepared).await;
        let mut paths: Vec<_> = kept.iter().map(|p| p.path.clone()).collect();
        paths.sort();
        assert_eq!(paths, vec!["a.rs", "b.rs"], "untouched files must all survive");
        drop(kept); // release the claims before inspecting state

        // No collateral damage: both files still 'indexing', chunks still active.
        assert_eq!(file_state(&pool, "a.rs").await, ("indexing".to_string(), 2, 0));
        assert_eq!(file_state(&pool, "b.rs").await, ("indexing".to_string(), 2, 0));
    }

    #[tokio::test]
    async fn drop_cancelled_drops_flipped_files_and_soft_deletes_their_chunks() {
        let pool = pool_with_prepared_files(&["a.rs", "b.rs"], 2).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let prepared = vec![prepared_for(&locks, "a.rs"), prepared_for(&locks, "b.rs")];

        // A concurrent POST /cancel lands between prepare and embed: indexing → cancelled.
        let pg = guid().0.as_simple().to_string();
        set_file_status(&pool, &pg, "a.rs", MODEL, "cancelled", false, CancellationToken::new())
            .await;

        let fx = fixture();
        let kept = indexer(&pool, &locks, &fx).drop_cancelled(prepared).await;
        let paths: Vec<_> = kept.iter().map(|p| p.path.clone()).collect();
        assert_eq!(paths, vec!["b.rs"], "the cancelled file must be dropped from the batch");

        // The cancelled file's just-inserted chunks are handed to GC; the survivor
        // is untouched.
        assert_eq!(file_state(&pool, "a.rs").await, ("cancelled".to_string(), 0, 2));
        assert_eq!(file_state(&pool, "b.rs").await, ("indexing".to_string(), 2, 0));
    }

    #[tokio::test]
    async fn recover_all_hands_every_prepared_file_to_the_retry_worker() {
        // The shared-embed-failure path: every prepared file goes indexing → failed
        // with its retry budget burned by one.
        let pool = pool_with_prepared_files(&["a.rs", "b.rs"], 1).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let prepared = vec![prepared_for(&locks, "a.rs"), prepared_for(&locks, "b.rs")];

        let fx = fixture();
        indexer(&pool, &locks, &fx).recover_all(&prepared, "failed", true).await;

        for path in ["a.rs", "b.rs"] {
            let (status, _, _) = file_state(&pool, path).await;
            assert_eq!(status, "failed", "{path} must not be left 'indexing' after an aborted batch");
        }

        // The client-cancelled path: cancelled, without burning retry budget.
        let pool = pool_with_prepared_files(&["c.rs"], 1).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let prepared = vec![prepared_for(&locks, "c.rs")];
        let fx = fixture();
        indexer(&pool, &locks, &fx).recover_all(&prepared, "cancelled", false).await;
        assert_eq!(file_state(&pool, "c.rs").await.0, "cancelled");
    }
}
