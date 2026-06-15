use std::collections::HashMap;

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
use crate::db::qdrant::ChunkAsVector;
use crate::db::qdrant::SearchHit;
use crate::db::qdrant::collection_name;
use crate::db::qdrant::ensure_project;
use crate::db::qdrant::insert_batch;
use crate::db::qdrant::search;
use crate::db::sqlite3::SQLite3Pool;
use crate::db::sqlite3::SQLite3PoolError;
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
use tree_sitter::Language;
use uuid::Uuid;

pub trait OptionResultExt<T> {
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
            error!("Slicer error: {other}");
            SQLite3PoolError::HTTPStatusCode(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn update_file_status(
    db_pool: &SQLite3Pool,
    project_guid: UUIDv4,
    path: String,
    model_id: String,
    status: &'static str,
    increment_retry: bool,
    token: CancellationToken,
) {
    let _ = db_pool
        .transaction(token, move |tx| {
            if increment_retry {
                tx.execute(
                    "UPDATE project_files
                     SET status = ?1, retry_count = retry_count + 1, status_updated_at = unixepoch()
                     WHERE project_guid = ?2 AND path = ?3 AND model_id = ?4",
                    params![status, project_guid, path, model_id],
                )?;
            } else {
                tx.execute(
                    "UPDATE project_files
                     SET status = ?1, status_updated_at = unixepoch()
                     WHERE project_guid = ?2 AND path = ?3 AND model_id = ?4",
                    params![status, project_guid, path, model_id],
                )?;
            }
            Ok(())
        })
        .await;
}

#[debug_handler]
pub async fn post_index(
    Path(project_guid): Path<UUIDv4>,
    State(s): State<RouterState>,
    Json(payload): Json<IndexRequest>,
) -> Result<Json<IndexResponse>, ErrorResponse> {
    let span = info_span!("indexing", project_guid = ?project_guid);

    async move {
    let guard = http3::CancellationGuard(CancellationToken::new());

    let db_pool = s.db_pool;
    let qdrant = s.qdrant;
    let tokenizer = s.tokenizer;
    let EmbeddingModel::BGEm3 { model_id, client } = s.model;

    let project_guid_simple = project_guid.0.as_simple().to_string();

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
                error!(?err);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    }

    // ── ensure Qdrant collection ──────────────────────────────────────────
    ensure_project(&qdrant, &collection_name(&project_guid_simple))
        .await
        .map_err(|err| {
            error!(?err);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut res = IndexResponse { files: HashMap::new() };
    let mut sha256_hasher = Sha256::default();

    for (pl, files) in payload.files.iter() {
        let pl = *pl;
        res.files.entry(pl).or_default();

        for (path, Code { code }) in files.iter() {
            let path = path.clone();
            let code = code.clone();

            let file_span = info_span!("indexing_file", ?pl, ?path);
            let _file_span_guard = file_span.enter();

            sha256_hasher.update(code.as_bytes());
            let sha256 = hex::encode(sha256_hasher.finalize_fixed_reset());

            // ── hash check ───────────────────────────────────────────────
            {
                let (sha256_c, path_c, model_id_c) =
                    (sha256.clone(), path.clone(), model_id.clone());
                let unchanged = db_pool
                    .transaction(guard.0.child_token(), move |tx| {
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
                    .with_cancellation_token(&guard.0)
                    .await
                    .from_cancelled()
                    .map_err(|err| {
                        error!(?err);
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;

                if unchanged {
                    info!("The source code has not changed: no reindexing is required.");
                    continue;
                }

                info!("The source code has changed: reindexing is required.");
            }

            // ── status = 'indexing' (committed before heavy work) ────────
            {
                let (sha256_u, path_u, model_id_u) =
                    (sha256.clone(), path.clone(), model_id.clone());
                db_pool
                    .transaction(guard.0.child_token(), move |tx| {
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
                    .with_cancellation_token(&guard.0)
                    .await
                    .from_cancelled()
                    .map_err(|err| {
                        error!(?err);
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
            }

            // ── mark old chunks deleted + slice + insert new chunks ───────
            let chunks_to_embed: Vec<(UUIDv4, String)> = {
                let tokenizer = tokenizer.clone();
                let (path_m, model_id_m, code_m) =
                    (path.clone(), model_id.clone(), code.clone());
                let slicer_token = guard.0.clone();

                let result = db_pool
                    .transaction(guard.0.child_token(), move |tx| {
                        tx.execute(
                            "UPDATE project_file_chunks
                             SET status = 'deleted'
                             WHERE project_guid = ?1 AND file_path = ?2 AND model_id = ?3
                               AND status = 'active'",
                            params![project_guid, path_m, model_id_m],
                        )?;

                        let mut slicer = Slicer::new(
                            Language::new(match pl {
                                ProgrammingLanguage::Rust => tree_sitter_rust::LANGUAGE,
                            }),
                            &tokenizer,
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
                    .with_cancellation_token(&guard.0)
                    .await
                    .from_cancelled();

                match result {
                    Ok(v) => v,
                    Err(SQLite3PoolError::Cancelled) => {
                        update_file_status(
                            &db_pool, project_guid, path.clone(), model_id.clone(),
                            "cancelled", false, guard.0.child_token(),
                        )
                        .await;
                        return Err(cancelled_499().into());
                    }
                    Err(SQLite3PoolError::HTTPStatusCode(sc)) => {
                        update_file_status(
                            &db_pool, project_guid, path.clone(), model_id.clone(),
                            "failed", true, guard.0.child_token(),
                        )
                        .await;
                        return Err(sc.into());
                    }
                    Err(err) => {
                        error!(?err);
                        update_file_status(
                            &db_pool, project_guid, path.clone(), model_id.clone(),
                            "failed", true, guard.0.child_token(),
                        )
                        .await;
                        return Err(StatusCode::INTERNAL_SERVER_ERROR.into());
                    }
                }
            };

            // ── embed + Qdrant upsert ─────────────────────────────────────
            let collection = collection_name(&project_guid_simple);
            let mut embed_error: Option<StatusCode> = None;

            'embed: for batch in chunks_to_embed.chunks(64) {
                let texts: Vec<String> = batch.iter().map(|(_, c)| c.clone()).collect();
                let guids: Vec<UUIDv4> = batch.iter().map(|(g, _)| *g).collect();

                info!(batch_len = batch.len(), "Embedding a batch.");

                let BGEm3EmbedResponse {
                    dense_vecs,
                    sparse_vecs,
                    colbert_vecs,
                } = match client.encode(BGEm3EmbedRequest { texts }, guard.0.clone()).await {
                    Ok(val) => val,
                    Err(EncodeError::Cancelled) => {
                        update_file_status(
                            &db_pool, project_guid, path.clone(), model_id.clone(),
                            "cancelled", false, guard.0.child_token(),
                        )
                        .await;
                        return Err(cancelled_499().into());
                    }
                    Err(EncodeError::Request(request_err)) => {
                        error!(?project_guid, ?request_err);
                        embed_error = Some(StatusCode::SERVICE_UNAVAILABLE);
                        break 'embed;
                    }
                };

                let mut vector_batch: Vec<ChunkAsVector> = Vec::with_capacity(guids.len());
                for (i, ((dense, sparse), colbert)) in dense_vecs
                    .iter()
                    .zip(sparse_vecs.iter())
                    .zip(colbert_vecs.iter())
                    .enumerate()
                {
                    let sparse_indices: Vec<u32> = sparse
                        .iter()
                        .filter(|(_, w)| **w > 1e-5)
                        .map(|(k, _)| *k)
                        .collect();
                    let sparse_values: Vec<f32> = sparse
                        .iter()
                        .filter(|(_, w)| **w > 1e-5)
                        .map(|(_, v)| *v)
                        .collect();

                    vector_batch.push(ChunkAsVector {
                        guid: guids[i],
                        dense: dense.clone(),
                        sparse_indices,
                        sparse_values,
                        colbert: colbert.clone(),
                    });
                }

                for points_batch in vector_batch.chunks(256) {
                    if let Err(qdrant_err) =
                        insert_batch(&qdrant, &collection, points_batch.to_vec()).await
                    {
                        error!(?qdrant_err);
                        embed_error = Some(StatusCode::INTERNAL_SERVER_ERROR);
                        break 'embed;
                    }
                }
            }

            if let Some(sc) = embed_error {
                update_file_status(
                    &db_pool, project_guid, path.clone(), model_id.clone(),
                    "failed", true, guard.0.child_token(),
                )
                .await;
                return Err(sc.into());
            }

            // ── status = 'indexed' + record new sha256 ────────────────────
            {
                let (sha256_f, path_f, model_id_f) =
                    (sha256.clone(), path.clone(), model_id.clone());
                db_pool
                    .transaction(guard.0.child_token(), move |tx| {
                        tx.execute(
                            "UPDATE project_files
                             SET status = 'indexed', sha256 = ?1, status_updated_at = unixepoch()
                             WHERE project_guid = ?2 AND path = ?3 AND model_id = ?4",
                            params![sha256_f, project_guid, path_f, model_id_f],
                        )?;
                        Ok(())
                    })
                    .with_cancellation_token(&guard.0)
                    .await
                    .from_cancelled()
                    .map_err(|err| {
                        error!(?err);
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
            }

            *res.files
                .entry(pl)
                .or_default()
                .entry(path.clone())
                .or_insert(0) += chunks_to_embed.len() as u64;

            info!("File indexed successfully.");
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
    let span = info_span!("searching", project_guid = ?project_guid.0);

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
                texts: vec![payload.query],
            },
            guard.0.clone(),
        )
        .await
    {
        Ok(val) => Ok(val),
        Err(EncodeError::Cancelled) => Err(cancelled_499()),
        Err(EncodeError::Request(request_err)) => {
            error!(?project_guid, ?request_err);
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }?;

    let chunks = state
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let mut param_number: usize = 1;

            // c.status = 'active' is always required to exclude soft-deleted chunks.
            let mut meta_where = vec![
                format!("c.project_guid = ?{}", param_number),
                "c.status = 'active'".to_string(),
            ];
            param_number += 1;
            let mut params: Vec<Box<dyn ToSql>> = vec![Box::new(project_guid)];

            if let Some(inc) = &payload.include {
                if let Some(pls) = &inc.programming_languages {
                    meta_where.push(format!(
                        "f.programming_language IN ({})",
                        vec![
                            format!("?{}", {
                                let tmp = param_number;
                                param_number += 1;
                                tmp
                            });
                            pls.len()
                        ]
                        .join(", ")
                    ));

                    for lang in pls {
                        params.push(Box::new(*lang));
                    }
                }

                if let Some(inc) = &inc.paths {
                    let clauses: Vec<String> = inc
                        .iter()
                        .map(|_| {
                            format!("c.file_path GLOB ?{}", {
                                let tmp = param_number;
                                param_number += 1;
                                tmp
                            })
                        })
                        .collect();

                    meta_where.push(clauses.join(" OR "));
                    params.extend(inc.iter().map(|p| Box::new(p.0.as_str()) as Box<dyn ToSql>));
                }
            }

            if let Some(exc) = &payload.exclude {
                if let Some(pls) = &exc.programming_languages {
                    meta_where.push(format!(
                        "f.programming_language NOT IN ({})",
                        vec![
                            format!("?{}", {
                                let tmp = param_number;
                                param_number += 1;
                                tmp
                            });
                            pls.len()
                        ]
                        .join(", ")
                    ));

                    for lang in pls {
                        params.push(Box::new(*lang));
                    }
                }

                if let Some(paths) = &exc.paths {
                    let clauses: Vec<String> = paths
                        .iter()
                        .map(|_| {
                            format!("c.file_path GLOB ?{}", {
                                let tmp = param_number;
                                param_number += 1;
                                tmp
                            })
                        })
                        .collect();

                    meta_where.push(format!("NOT ({})", clauses.join(" OR ")));
                    params.extend(
                        paths
                            .iter()
                            .map(|p| Box::new(p.0.as_str()) as Box<dyn ToSql>),
                    );
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

            let chunks: std::collections::HashMap<UUIDv4, (String, String, i64, i64, i64, i64)> = tx
                .prepare(&sql)?
                .query_map(params_from_iter(params), |row| {
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
                error!(?project_guid, "{}", err);
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

    let project_guid_simple = project_guid.0.as_simple().to_string();

    let search_hits = search(
        &state.qdrant,
        &collection_name(project_guid_simple.as_str()),
        chunks.keys().copied().collect(),
        dense,
        sparse.keys().copied().collect(),
        sparse.values().copied().collect(),
        colbert,
        payload.top_k.unwrap_or(5) as u64,
    )
    .await
    .map_err(|err| {
        error!(?err);
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
                        warn!(?err, "Bad UUIDv4.");
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
