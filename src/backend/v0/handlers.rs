use std::collections::HashMap;
use std::vec;

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
use crate::db::qdrant::delete_batch;
use crate::db::qdrant::ensure_project;
use crate::db::qdrant::insert_batch;
use crate::db::qdrant::search;
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

fn handle_slicer_error(err: SlicerError) -> StatusCode {
    match err {
        SlicerError::Cancelled => cancelled_499(),
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[debug_handler]
pub async fn post_index(
    Path(project_guid): Path<UUIDv4>,
    State(s): State<RouterState>,
    Json(payload): Json<IndexRequest>,
) -> Result<Json<IndexResponse>, ErrorResponse> {
    let span = info_span!("indexing", project_guid = ?project_guid);

    async move {
    let guard: http3::CancellationGuard = http3::CancellationGuard(CancellationToken::new());

    let EmbeddingModel::BGEm3 { model_id, client } = s.model;

    let token = guard.0.clone();
    let res = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let mut res = IndexResponse {
                files: HashMap::new(),
            };

            let project_exists = tx
                .query_row(
                    "
    SELECT 1 FROM projects WHERE guid = ?1 AND model_id = ?2",
                    params![project_guid, model_id],
                    |_| Ok(true),
                )
                .optional()
                .map_err(SQLite3PoolError::from)?
                .is_some();

            if !project_exists {
                info!("Project does not exist. Creating a new one.");

                tx.execute(
                    "
    INSERT INTO projects (guid, model_id) VALUES (?, ?)",
                    params![project_guid, model_id],
                )?;
            } else {
                info!("Project already exists.");
            }

            let project_guid_simple = project_guid.0.as_simple().to_string();
            let _ = match tokio::runtime::Handle::current()
                .block_on(ensure_project(&s.qdrant, &collection_name(project_guid_simple.as_str())))
            {
                Ok(_) => Ok(()),
                Err(qdrant_err) => {
                    error!(?qdrant_err);
                    Err(SQLite3PoolError::HTTPStatusCode(
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ))
                }
            }?;

            let mut sha256_hasher = Sha256::default();

            for (pl, files) in payload.files.iter() {
                let mut slicer = Slicer::new(
                    Language::new(match pl {
                        ProgrammingLanguage::Rust => tree_sitter_rust::LANGUAGE,
                    }),
                    &s.tokenizer,
                )
                .map_err(handle_slicer_error)?;

                res.files.insert(*(pl), HashMap::new());

                for (path, Code { code }) in files.iter() {
                    let file_span = info_span!("indexing_file", ?pl, ?path);
                    let _ = file_span.enter();

                    sha256_hasher.update(code.as_bytes());
                    let sha256 = hex::encode(sha256_hasher.finalize_fixed_reset());

                    let actual_sha256: Option<String> = tx
                        .query_row(
                            "
    SELECT sha256 FROM project_files
    WHERE project_guid = ?1
        AND path = ?2
        AND programming_language = ?3
        AND model_id = ?4",
                            params![project_guid, path, pl, model_id],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(SQLite3PoolError::from)?;

                    if let Some(ref actual_sha256) = actual_sha256 {
                        if *actual_sha256 == sha256 {
                            info!(
                                actual_sha256 = *actual_sha256,
                                ?sha256,
                                "The source code has not changed: no reindexing is required."
                            );

                            continue;
                        }

                        info!("The source code has changed: a reindexing is required.");

                        let qdrant_guids: Vec<String> = tx
                            .prepare(
                                "
    SELECT qdrant_guid
    FROM project_file_chunks
        WHERE project_guid = ?1
            AND file_path = ?2
            AND model_id = ?3
                            ",
                            )?
                            .query_map(params![project_guid, path, model_id], |row| row.get(0))?
                            .collect::<Result<Vec<_>, _>>()?;

                        for qdrant_guids_batch in qdrant_guids.chunks(256) {
                            let _ = match tokio::runtime::Handle::current().block_on(delete_batch(
                                &s.qdrant,
                                &collection_name(project_guid_simple.as_str()),
                                qdrant_guids_batch.to_vec(),
                            )) {
                                Ok(_) => Ok(()),
                                Err(qdrant_err) => {
                                    error!(?qdrant_err);
                                    Err(SQLite3PoolError::HTTPStatusCode(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                    ))
                                }
                            }?;
                        }

                        let deleted_files = tx
                            .execute(
                                "
    DELETE FROM project_files
    WHERE project_guid = ?1
        AND path = ?2
        AND programming_language = ?3
        AND model_id = ?4",
                                params![project_guid, path, pl, model_id],
                            )
                            .map_err(SQLite3PoolError::from)?;

                        info!(
                            ?deleted_files,
                            deleted_qdrant_points = qdrant_guids.len(),
                            "Pruned old chunks."
                        );
                    }

                    tx.execute(
                        "
    INSERT INTO project_files (project_guid, path, sha256, programming_language, model_id)
    VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![project_guid, path, sha256, pl, model_id],
                    )
                    .map_err(SQLite3PoolError::from)?;

                    info!("Processing the source code.");

                    let chunks = slicer
                        .parse(code, token.clone())
                        .map_err(handle_slicer_error)?;

                    info!(chunks_len = ?chunks.len(), "Sliced the source code.");

                    let counter = match res.files.get_mut(pl) {
                        Some(map) => map,
                        None => panic!("BUG: language key missing from response map"),
                    }
                    .entry(path.clone())
                    .or_insert(0);

                    for chunks_batch in chunks.chunks(64) {
                        let mut req = BGEm3EmbedRequest {
                            texts: Vec::with_capacity(chunks_batch.len()),
                        };

                        let mut qdrant_guids = Vec::with_capacity(chunks_batch.len());

                        for SlicedChunk { code, start_line, end_line, start_column, end_column, .. } in chunks_batch {
                            *counter += 1;

                            req.texts.push(code.to_string());

                            let qdrant_guid = UUIDv4(Uuid::new_v4());

                            tx.execute(
                                "
    INSERT INTO project_file_chunks
        (project_guid, file_path, code, model_id, qdrant_guid,
         start_line, end_line, start_column, end_column)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                                params![
                                    project_guid, path, code, model_id, qdrant_guid,
                                    *start_line as i64, *end_line as i64,
                                    *start_column as i64, *end_column as i64
                                ],
                            )
                            .map_err(SQLite3PoolError::from)?;

                            qdrant_guids.push(qdrant_guid);
                        }

                        info!(batch_len = chunks_batch.len(), "Embedding a batch.");

                        let BGEm3EmbedResponse {
                            dense_vecs,
                            sparse_vecs,
                            colbert_vecs,
                        } = match tokio::runtime::Handle::current()
                            .block_on(client.encode(req, token.clone()))
                        {
                            Ok(val) => Ok(val),
                            Err(EncodeError::Cancelled) => Err(SQLite3PoolError::Cancelled),
                            Err(EncodeError::Request(request_err)) => {
                                error!(?project_guid, ?request_err);
                                Err(SQLite3PoolError::HTTPStatusCode(
                                    StatusCode::SERVICE_UNAVAILABLE,
                                ))
                            }
                        }?;

                        let mut chunks: Vec<ChunkAsVector> = Vec::with_capacity(dense_vecs.len());

                        for (i, ((dense_vec, sparse_vec), colbert_vec)) in dense_vecs
                            .iter()
                            .zip(sparse_vecs.iter())
                            .zip(colbert_vecs.iter())
                            .enumerate()
                        {
                            let qdrant_guid = qdrant_guids[i];

                            let sparse_indices: Vec<u32> = sparse_vec
                                .iter()
                                .filter(|(_, w)| **w > 1e-5)
                                .map(|(k, _)| *k)
                                .collect();

                            let sparse_values: Vec<f32> = sparse_vec
                                .iter()
                                .filter(|(_, w)| **w > 1e-5)
                                .map(|(_, v)| v)
                                .copied()
                                .collect();

                            chunks.push(ChunkAsVector {
                                guid: qdrant_guid,
                                dense: dense_vec.clone(),
                                sparse_indices,
                                sparse_values,
                                colbert: colbert_vec.clone(),
                            });
                        }

                        for chunks_batch in chunks.chunks(256) {
                            let _ = match tokio::runtime::Handle::current().block_on(insert_batch(
                                &s.qdrant,
                                &collection_name(project_guid_simple.as_str()),
                                chunks_batch.to_vec(),
                            )) {
                                Ok(_) => Ok(()),
                                Err(qdrant_err) => {
                                    error!(?qdrant_err);
                                    Err(SQLite3PoolError::HTTPStatusCode(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                    ))
                                }
                            }?;
                        }
                    }
                }
            }

            info!("All good.");

            Ok(res)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled();

    match res {
        Ok(res) => Ok(Json(res)),
        Err(SQLite3PoolError::Cancelled) => Err(cancelled_499().into()),
        Err(SQLite3PoolError::HTTPStatusCode(status_code)) => Err(status_code.into()),
        Err(err) => {
            error!(?project_guid, "{}", err);

            Err(StatusCode::INTERNAL_SERVER_ERROR.into())
        }
    }
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

            let mut meta_where = vec![format!("c.project_guid = ?{}", param_number).to_string()];
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
                            .to_string()
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
                            .to_string()
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
