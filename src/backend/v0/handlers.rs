use std::collections::HashMap;
use std::sync::Arc;

use super::models::IndexRequest;
use crate::backend::http3;
use crate::backend::http3::EmbeddingModel;
use crate::backend::http3::RouterState;
use crate::backend::http3::cancelled_499;
use crate::backend::v0::models::Code;
use crate::backend::v0::models::IndexResponse;
use crate::backend::v0::models::ProgrammingLanguage;
use crate::backend::v0::models::SearchRequest;
use crate::backend::v0::models::SearchResponse;
use crate::backend::v0::models::SearchResult;
use crate::backend::v0::models::UUIDv4;
use crate::db::qdrant::SearchHit;
use crate::db::qdrant::VectorStore;
use crate::db::qdrant::collection_for;
use crate::db::files::set_file_status;
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
use qdrant_client::qdrant::point_id::PointIdOptions;
use rusqlite::OptionalExtension;
use rusqlite::ToSql;
use rusqlite::params;
use rusqlite::params_from_iter;
use sha2::Sha256;
use sha2::digest::FixedOutputReset;
use sha2::digest::Update;
use tokio_util::future::FutureExt;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::warn;
use tokenizers::Tokenizer;
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

/// Builds the SQL and ordered bind values for `post_search`'s candidate query.
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
    SELECT c.qdrant_guid, c.file_path, c.code,
           c.start_line, c.end_line, c.start_column, c.end_column
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
        ProgrammingLanguage::TypeScript => Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
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

/// Borrowed view of everything one file's indexing needs. Lets the per-file
/// pipeline live in `index_one` — independently readable and the natural unit to
/// test — instead of inline in `post_index`'s nested loops.
struct FileIndexer<'a> {
    db_pool: &'a SQLite3Pool,
    store: &'a dyn VectorStore,
    tokenizer: &'a Arc<Tokenizer>,
    embedder: &'a dyn BGEm3Model,
    model_id: &'a str,
    project_guid: UUIDv4,
    collection: &'a str,
    /// Request-scoped cancellation token (the handler's `CancellationGuard`).
    token: &'a CancellationToken,
}

impl FileIndexer<'_> {
    /// Indexes one file end to end. Returns `Ok(None)` if the content is unchanged
    /// (hash match, skipped), `Ok(Some(n))` if (re)indexed into `n` chunks, or
    /// `Err` on failure — in which case the file's status has already been recovered
    /// to `cancelled`/`failed` and the error carries the HTTP status to return.
    async fn index_one(
        &self,
        pl: ProgrammingLanguage,
        path: &str,
        code: &str,
        hasher: &mut Sha256,
    ) -> Result<Option<u64>, ErrorResponse> {
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
            let chunks_to_embed: Vec<(UUIDv4, String)> = {
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

                match result {
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
                }
            };

            // ── embed + Qdrant upsert ─────────────────────────────────────
            match embed_and_upsert(
                self.embedder,
                self.store,
                self.collection,
                &chunks_to_embed,
                self.token,
            )
            .await
            {
                Ok(()) => {}
                Err(EmbedUpsertError::Cancelled) => {
                    self.recover(path, "cancelled", false).await;
                    return Err(cancelled_499().into());
                }
                Err(EmbedUpsertError::Embed(request_err)) => {
                    error!(
                        error = ?request_err,
                        "Embedding request failed; marking file 'failed'. \
                         Check the model server at --model-server is up and reachable \
                         (from inside the container it must bind 0.0.0.0, not 127.0.0.1)."
                    );
                    self.recover(path, "failed", true).await;
                    return Err(StatusCode::SERVICE_UNAVAILABLE.into());
                }
                Err(EmbedUpsertError::Store(qdrant_err)) => {
                    error!(
                        error = ?qdrant_err,
                        "Qdrant upsert failed; marking file 'failed'. \
                         Check Qdrant is reachable at --qdrant-server."
                    );
                    self.recover(path, "failed", true).await;
                    return Err(StatusCode::INTERNAL_SERVER_ERROR.into());
                }
            }

            // ── status = 'indexed' + record new sha256 ────────────────────
            {
                let (sha256_f, path_f, model_id_f) =
                    (sha256.clone(), path.to_string(), self.model_id.to_string());
                self.db_pool
                    .transaction(self.token.child_token(), move |tx| {
                        tx.execute(
                            "UPDATE project_files
                             SET status = 'indexed', sha256 = ?1, status_updated_at = unixepoch()
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
            }

            info!("File indexed successfully.");
            Ok(Some(chunks_to_embed.len() as u64))
        }
        .instrument(span)
        .await
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
    qdrant
        .ensure_project(&collection)
        .await
        .map_err(|err| {
            error!(
                error = ?err,
                "Failed to ensure the Qdrant collection. \
                 Check Qdrant is reachable at --qdrant-server and accepting connections."
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut res = IndexResponse { files: HashMap::new() };
    let mut sha256_hasher = Sha256::default();

    let indexer = FileIndexer {
        db_pool: &db_pool,
        store: &*qdrant,
        tokenizer: &tokenizer,
        embedder: &*client,
        model_id: &model_id,
        project_guid,
        collection: &collection,
        token: &guard.0,
    };

    for (pl, files) in payload.files.iter() {
        let pl = *pl;
        res.files.entry(pl).or_default();

        for (path, Code { code }) in files.iter() {
            if let Some(count) = indexer.index_one(pl, path, code, &mut sha256_hasher).await? {
                *res.files.entry(pl).or_default().entry(path.clone()).or_insert(0) += count;
            }
        }
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

    let chunks = state
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let chunks: std::collections::HashMap<UUIDv4, (String, String, i64, i64, i64, i64)> = tx
                .prepare(&sql)?
                .query_map(params_from_iter(binds), |row| {
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
                .collect::<Result<std::collections::HashMap<_, _>, _>>()?;

            Ok(chunks)
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

    if chunks.is_empty() {
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
            chunks.keys().copied().collect(),
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

    if search_hits.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(Json(SearchResponse {
        results: search_hits
            .iter()
            .filter_map(|SearchHit { id, score }| {
                let uuid = match &id.point_id_options {
                    Some(PointIdOptions::Uuid(uuid)) => uuid,
                    _ => return None,
                };

                let uuid = match Uuid::parse_str(uuid) {
                    Ok(uuid) => UUIDv4(uuid),
                    Err(err) => {
                        warn!(error = ?err, point_id = %uuid, "Qdrant returned a point id that is not a valid UUID; skipping it.");
                        return None;
                    }
                };

                let (path, code, start_line, end_line, start_column, end_column) =
                    chunks.get(&uuid)?;

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
            .collect(),
    }))
    }
    .instrument(span)
    .await
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
        SearchRequest { query: "q".into(), top_k: None, include, exclude }
    }

    fn langs(v: Vec<ProgrammingLanguage>) -> SearchFilter {
        SearchFilter { paths: None, programming_languages: Some(v) }
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
            &req(Some(langs(vec![ProgrammingLanguage::Rust, ProgrammingLanguage::Python])), None),
        );
        assert!(sql.contains("f.programming_language IN (?2, ?3)"), "sql was: {sql}");
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
        let (sql, binds) = build_search_query(guid(), &req(Some(paths(&["src/**", "tests/**"])), None));
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
        assert!(sql.contains("f.programming_language IN (?2, ?3)"), "sql was: {sql}");
        assert!(sql.contains("c.file_path GLOB ?4 OR c.file_path GLOB ?5"), "sql was: {sql}");
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
        let (sql, _) =
            build_search_query(guid(), &req(None, Some(langs(vec![ProgrammingLanguage::Json]))));
        assert!(sql.contains("f.programming_language NOT IN (?2)"), "sql was: {sql}");
    }

    #[test]
    fn exclude_paths_are_negated() {
        let (sql, binds) = build_search_query(guid(), &req(None, Some(paths(&["vendor/**"]))));
        assert!(sql.contains("NOT (c.file_path GLOB ?2)"), "sql was: {sql}");
        assert_eq!(binds, vec![Bind::Guid(guid()), Bind::Path("vendor/**".into())]);
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
        assert!(sql.contains("f.programming_language IN (?2)"), "sql was: {sql}");
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
