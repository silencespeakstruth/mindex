use std::collections::HashMap;
use std::sync::Arc;

use super::models::IndexRequest;
use crate::backend::http3;
use crate::backend::http3::EmbeddingModel;
use crate::backend::http3::RouterState;
use crate::backend::http3::cancelled_499;
use crate::backend::v0::models::ChunkCounts;
use crate::backend::v0::models::Code;
use crate::backend::v0::models::DeleteFilesRequest;
use crate::backend::v0::models::DeleteFilesResponse;
use crate::backend::v0::models::FileStatusCounts;
use crate::backend::v0::models::GcResponse;
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
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::ErrorResponse;
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
            meta_where.push(clauses.join(" OR "));
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

/// A file that has been hash-checked, marked `indexing`, sliced, and had its
/// chunks inserted — awaiting the shared embed pass. `chunks` is drained into the
/// cross-file batch before `mark_indexed`.
struct Prepared {
    pl: ProgrammingLanguage,
    path: String,
    sha256: String,
    chunks: Vec<(UUIDv4, String)>,
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
    /// Chunks per `/encode` call (GPU batch lever).
    embed_batch: usize,
    /// Request-scoped cancellation token (the handler's `CancellationGuard`).
    token: &'a CancellationToken,
}

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
    ) -> Result<Option<Prepared>, ErrorResponse> {
        let span = info_span!("indexing_file", ?pl, path);
        async move {
            let project_guid = self.project_guid;

            hasher.update(code.as_bytes());
            let sha256 = hex::encode(hasher.finalize_fixed_reset());

            // ── hash check ───────────────────────────────────────────────
            {
                let (sha256_c, path_c, model_id_c) =
                    (sha256.clone(), path.to_string(), self.model_id.to_string());
                let unchanged = self
                    .db_pool
                    .transaction(self.token.child_token(), move |tx| {
                        let existing: Option<String> = tx
                            .query_row(
                                "SELECT sha256 FROM project_files
                                 WHERE project_guid = ?1 AND path = ?2 AND model_id = ?3",
                                params![project_guid, path_c, model_id_c],
                                |r| r.get(0),
                            )
                            .optional()?;
                        Ok(existing.as_deref() == Some(sha256_c.as_str()))
                    })
                    .with_cancellation_token(self.token)
                    .await
                    .from_cancelled()
                    .map_err(|err| {
                        error!(error = ?err, "Failed to read the stored file hash from SQLite.");
                        StatusCode::INTERNAL_SERVER_ERROR
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
                            "INSERT INTO project_files
                                 (project_guid, path, sha256, programming_language, model_id,
                                  status, status_updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, 'indexing', unixepoch())
                             ON CONFLICT (project_guid, model_id, path)
                             DO UPDATE SET status = 'indexing', status_updated_at = unixepoch()",
                            params![project_guid, path_u, sha256_u, pl, model_id_u],
                        )?;
                        Ok(())
                    })
                    .with_cancellation_token(self.token)
                    .await
                    .from_cancelled()
                    .map_err(|err| {
                        error!(error = ?err, "Failed to mark the file 'indexing' in SQLite.");
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
            }

            // ── mark old chunks deleted + slice + insert new chunks ───────
            let tokenizer = self.tokenizer.clone();
            let (path_m, model_id_m, code_m) =
                (path.to_string(), self.model_id.to_string(), code.to_string());
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

                    let mut slicer = Slicer::new(tree_sitter_language(pl), &*tokenizer)
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
                    return Err(cancelled_499().into());
                }
                Err(SQLite3PoolError::HTTPStatusCode(sc)) => {
                    self.recover(path, "failed", true).await;
                    return Err(sc.into());
                }
                Err(err) => {
                    error!(error = ?err, "Slicing / chunk insertion failed; marking file 'failed'.");
                    self.recover(path, "failed", true).await;
                    return Err(StatusCode::INTERNAL_SERVER_ERROR.into());
                }
            };

            Ok(Some(Prepared { pl, path: path.to_string(), sha256, chunks }))
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
            self.embed_batch,
        )
        .await
    }

    /// Phase 3 for one file: mark it `indexed` and record the new sha256.
    async fn mark_indexed(&self, path: &str, sha256: &str) -> Result<(), ErrorResponse> {
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
                     WHERE project_guid = ?2 AND path = ?3 AND model_id = ?4",
                    params![sha256_f, project_guid, path_f, model_id_f],
                )?;
                Ok(())
            })
            .with_cancellation_token(self.token)
            .await
            .from_cancelled()
            .map_err(|err| {
                error!(error = ?err, "Failed to mark the file 'indexed' in SQLite.");
                StatusCode::INTERNAL_SERVER_ERROR
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
}

#[debug_handler]
pub async fn post_index(
    Path(project_guid): Path<UUIDv4>,
    State(s): State<RouterState>,
    Json(payload): Json<IndexRequest>,
) -> Result<Json<IndexResponse>, ErrorResponse> {
    let span = info_span!("indexing", project_guid = %project_guid.0);

    async move {
        let guard = http3::CancellationGuard(CancellationToken::new());

        let db_pool = s.db_pool;
        let qdrant = s.qdrant;
        let tokenizer = s.tokenizer;
        let EmbeddingModel::BGEm3 { model_id, client } = s.model;

        let collection = collection_for(project_guid);

        // ── ensure project row ────────────────────────────────────────────────
        {
            let model_id = model_id.clone();
            db_pool
                .transaction(guard.0.child_token(), move |tx| {
                    let exists = tx
                        .query_row(
                            "SELECT 1 FROM projects WHERE guid = ?1 AND model_id = ?2",
                            params![project_guid, model_id],
                            |_| Ok(true),
                        )
                        .optional()?
                        .is_some();

                    if !exists {
                        info!("Project does not exist. Creating a new one.");
                        tx.execute(
                            "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                            params![project_guid, model_id],
                        )?;
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
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
        }

        // ── ensure Qdrant collection ──────────────────────────────────────────
        qdrant.ensure_project(&collection).await.map_err(|err| {
            error!(
                error = ?err,
                "Failed to ensure the Qdrant collection. \
                 Check Qdrant is reachable at --qdrant-server and accepting connections."
            );
            StatusCode::INTERNAL_SERVER_ERROR
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
            embed_batch: s.embed_batch,
            token: &guard.0,
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
                    Err(e) => {
                        // A later file failed to prepare; recover the ones already prepared
                        // (they're 'indexing' with chunks inserted) before bailing.
                        indexer.recover_all(&prepared, "failed", true).await;
                        return Err(e);
                    }
                }
            }
        }

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
                return Err(cancelled_499().into());
            }
            Err(EmbedUpsertError::Embed(request_err)) => {
                error!(
                    error = ?request_err,
                    "Embedding request failed; marking batch 'failed'. \
                     Check the model server at --model-server is up and reachable \
                     (from inside the container it must bind 0.0.0.0, not 127.0.0.1)."
                );
                indexer.recover_all(&prepared, "failed", true).await;
                return Err(StatusCode::SERVICE_UNAVAILABLE.into());
            }
            Err(EmbedUpsertError::Store(qdrant_err)) => {
                error!(
                    error = ?qdrant_err,
                    "Qdrant upsert failed; marking batch 'failed'. \
                     Check Qdrant is reachable at --qdrant-server."
                );
                indexer.recover_all(&prepared, "failed", true).await;
                return Err(StatusCode::INTERNAL_SERVER_ERROR.into());
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

#[debug_handler]
pub async fn post_search(
    Path(project_guid): Path<UUIDv4>,
    State(state): State<RouterState>,
    Json(payload): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, StatusCode> {
    let span = info_span!("searching", project_guid = %project_guid.0);

    async move {
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
        Err(EncodeError::Cancelled) => Err(cancelled_499()),
        Err(EncodeError::Request(request_err)) => {
            error!(
                error = ?request_err,
                "Failed to embed the search query. \
                 Check the model server at --model-server is up and reachable."
            );
            Err(StatusCode::SERVICE_UNAVAILABLE)
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
        .map_err(|err| match err {
            SQLite3PoolError::Cancelled => cancelled_499(),
            SQLite3PoolError::HTTPStatusCode(status_code) => status_code,
            err => {
                error!(error = %err, "Failed to query candidate chunks from SQLite.");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;

    if candidate_ids.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }

    let dense = dense_vecs
        .into_iter()
        .next()
        .ok_or(StatusCode::BAD_REQUEST)?;
    let sparse = sparse_vecs
        .into_iter()
        .next()
        .ok_or(StatusCode::BAD_REQUEST)?;
    let colbert = colbert_vecs
        .into_iter()
        .next()
        .ok_or(StatusCode::BAD_REQUEST)?;

    let search_hits = state
        .qdrant
        .search(
            &collection_for(project_guid),
            candidate_ids,
            dense,
            sparse.keys().copied().collect(),
            sparse.values().copied().collect(),
            colbert,
            payload.top_k.unwrap_or(5) as u64,
        )
        .await
    .map_err(|err| {
        error!(
            error = ?err,
            "Qdrant query failed. Check Qdrant is reachable at --qdrant-server \
             and the project's collection exists."
        );
        StatusCode::SERVICE_UNAVAILABLE
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
        return Err(StatusCode::NOT_FOUND);
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
        .map_err(|err| match err {
            SQLite3PoolError::Cancelled => cancelled_499(),
            err => {
                error!(error = %err, "Failed to fetch result rows from SQLite.");
                StatusCode::INTERNAL_SERVER_ERROR
            }
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

/// `GET /projects/{guid}` — aggregate counts: `project_files` by status, and chunks
/// per language split into active vs soft-deleted (pending GC). 404 if the project
/// has never been seen.
#[debug_handler]
pub async fn get_project_stats(
    Path(project_guid): Path<UUIDv4>,
    State(s): State<RouterState>,
) -> Result<Json<ProjectStats>, ErrorResponse> {
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
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    match result {
        Some((files, chunks)) => Ok(Json(ProjectStats {
            project_guid,
            files,
            chunks,
        })),
        None => Err(StatusCode::NOT_FOUND.into()),
    }
}

/// `DELETE /projects/{guid}` — hard-deletes the project: all chunks, files, the
/// project row, its status log, and the Qdrant collection. Idempotent: deleting a
/// non-existent project (or re-deleting) is a 204. Chunks go first (the chunk→file
/// FK is RESTRICT); the collection is dropped last so a retry re-attempts a failed
/// drop even once the rows are gone.
#[debug_handler]
pub async fn delete_project(
    Path(project_guid): Path<UUIDv4>,
    State(s): State<RouterState>,
) -> Result<StatusCode, ErrorResponse> {
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
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    s.qdrant.delete_collection(&collection).await.map_err(|e| {
        error!(
            error = %e,
            collection = %collection,
            "Failed to delete the Qdrant collection. Check Qdrant is reachable at --qdrant-server."
        );
        StatusCode::INTERNAL_SERVER_ERROR
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
#[debug_handler]
pub async fn delete_files(
    Path(project_guid): Path<UUIDv4>,
    State(s): State<RouterState>,
    Json(req): Json<DeleteFilesRequest>,
) -> Result<Response, ErrorResponse> {
    let nonempty = |f: &Option<SearchFilter>| {
        f.as_ref().is_some_and(|x| {
            x.paths.as_ref().is_some_and(|p| !p.is_empty())
                || x.programming_languages
                    .as_ref()
                    .is_some_and(|l| !l.is_empty())
        })
    };
    if !nonempty(&req.include) && !nonempty(&req.exclude) {
        return Err((
            StatusCode::BAD_REQUEST,
            "DELETE /files requires a non-empty include or exclude selector",
        )
            .into());
    }

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
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if paths.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // 2) Soft-delete chunks + files, batched to stay under SQLite's bind-variable limit.
    let mut deleted_files: u64 = 0;
    for batch in paths.chunks(500) {
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
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        deleted_files += n;
    }

    if deleted_files == 0 {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((StatusCode::OK, Json(DeleteFilesResponse { deleted_files })).into_response())
    }
}

/// `POST /gc` — runs a full GC pass synchronously and returns what it removed:
/// hard-deletes soft-deleted chunks (whose vectors are confirmed gone from Qdrant),
/// then the now-empty `deleted` file rows, then prunes the old status log. Blocking
/// by design; the periodic worker runs the same steps hourly.
#[debug_handler]
pub async fn post_gc(State(s): State<RouterState>) -> Json<GcResponse> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let chunks_removed = crate::worker::gc::sweep(&s.db_pool, &*s.qdrant, &guard.0).await;
    let files_removed = crate::worker::gc::prune_deleted_files(&s.db_pool, &guard.0).await;
    let status_log_pruned = crate::worker::gc::prune_status_log(&s.db_pool, &guard.0).await;
    Json(GcResponse {
        chunks_removed,
        files_removed,
        status_log_pruned,
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
}
