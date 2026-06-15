use std::sync::Arc;
use std::time::Duration;

use qdrant_client::Qdrant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::backend::v0::models::UUIDv4;
use crate::db::qdrant::{ChunkAsVector, collection_name, insert_batch};
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};
use crate::models::bge_m3::{BGEm3EmbedRequest, BGEm3EmbedResponse, BGEm3Model, BGEm3HttpClient, EncodeError};

const MAX_RETRIES: i64 = 3;

pub async fn run(
    db_pool: Arc<SQLite3Pool>,
    qdrant: Arc<Qdrant>,
    model_client: Arc<BGEm3HttpClient>,
    model_id: String,
    token: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = token.cancelled() => {
                info!("Retry worker: shutting down.");
                break;
            }
        }

        let stuck_files: Vec<(String, String, String)> = db_pool
            .transaction(token.clone(), {
                let model_id = model_id.clone();
                move |tx| {
                tx.prepare(
                    "SELECT project_guid, path, model_id
                     FROM project_files
                     WHERE model_id = ?2
                       AND ((status IN ('just_uploaded', 'indexing')
                             AND status_updated_at < unixepoch() - 300)
                         OR (status = 'failed'
                             AND retry_count < ?1
                             AND status_updated_at < unixepoch() - 60))",
                )?
                .query_map(rusqlite::params![MAX_RETRIES, model_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
            }})
            .await
            .unwrap_or_default();

        for (project_guid, path, file_model_id) in stuck_files {
            if token.is_cancelled() {
                break;
            }

            info!(?project_guid, ?path, "Retry worker: retrying stuck file.");

            let chunks: Vec<(String, String)> = db_pool
                .transaction(token.clone(), {
                    let (pg, p, m) = (project_guid.clone(), path.clone(), file_model_id.clone());
                    move |tx| {
                        tx.prepare(
                            "SELECT qdrant_guid, code
                             FROM project_file_chunks
                             WHERE project_guid = ?1 AND file_path = ?2 AND model_id = ?3
                               AND status = 'active'",
                        )?
                        .query_map(rusqlite::params![pg, p, m], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(SQLite3PoolError::from)
                    }
                })
                .await
                .unwrap_or_default();

            if chunks.is_empty() {
                warn!(?project_guid, ?path, "Retry: no active chunks found, marking failed.");
                set_status(&db_pool, &project_guid, &path, &file_model_id, "failed", true, token.clone()).await;
                continue;
            }

            let collection = collection_name(&project_guid);
            let mut success = true;

            'embed: for batch in chunks.chunks(64) {
                let texts: Vec<String> = batch.iter().map(|(_, c)| c.clone()).collect();
                let guids: Vec<UUIDv4> = batch
                    .iter()
                    .map(|(g, _)| UUIDv4(Uuid::parse_str(g).unwrap_or_default()))
                    .collect();

                let BGEm3EmbedResponse {
                    dense_vecs,
                    sparse_vecs,
                    colbert_vecs,
                } = match model_client.encode(BGEm3EmbedRequest { texts }, token.clone()).await {
                    Ok(r) => r,
                    Err(EncodeError::Cancelled) => {
                        success = false;
                        break 'embed;
                    }
                    Err(EncodeError::Request(e)) => {
                        error!(?e, "Retry: embed request failed");
                        success = false;
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
                    let sparse_indices = sparse
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
                    if let Err(e) = insert_batch(&qdrant, &collection, points_batch.to_vec()).await {
                        error!(?e, "Retry: Qdrant upsert failed");
                        success = false;
                        break 'embed;
                    }
                }
            }

            set_status(
                &db_pool,
                &project_guid,
                &path,
                &file_model_id,
                if success { "indexed" } else { "failed" },
                !success,
                token.clone(),
            )
            .await;
        }
    }
}

async fn set_status(
    db_pool: &SQLite3Pool,
    project_guid: &str,
    path: &str,
    model_id: &str,
    status: &'static str,
    increment_retry: bool,
    token: CancellationToken,
) {
    let (pg, p, m) = (project_guid.to_string(), path.to_string(), model_id.to_string());
    let _ = db_pool
        .transaction(token, move |tx| {
            if increment_retry {
                tx.execute(
                    "UPDATE project_files
                     SET status = ?1, retry_count = retry_count + 1, status_updated_at = unixepoch()
                     WHERE project_guid = ?2 AND path = ?3 AND model_id = ?4",
                    rusqlite::params![status, pg, p, m],
                )?;
            } else {
                tx.execute(
                    "UPDATE project_files
                     SET status = ?1, status_updated_at = unixepoch()
                     WHERE project_guid = ?2 AND path = ?3 AND model_id = ?4",
                    rusqlite::params![status, pg, p, m],
                )?;
            }
            Ok(())
        })
        .await;
}
