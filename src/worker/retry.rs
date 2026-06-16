use std::sync::Arc;
use std::time::Duration;

use qdrant_client::Qdrant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::backend::v0::models::UUIDv4;
use crate::db::files::set_file_status;
use crate::db::qdrant::collection_name;
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};
use crate::embed::{EmbedUpsertError, embed_and_upsert};
use crate::models::bge_m3::BGEm3Model;

const MAX_RETRIES: i64 = 3;

pub async fn run(
    db_pool: Arc<SQLite3Pool>,
    qdrant: Arc<Qdrant>,
    model_client: Arc<dyn BGEm3Model>,
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

            info!(%project_guid, %path, "Retry worker: retrying stuck file.");

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
                warn!(%project_guid, %path, "Retry worker: no active chunks found for stuck file; marking 'failed'.");
                set_file_status(&db_pool, &project_guid, &path, &file_model_id, "failed", true, token.clone()).await;
                continue;
            }

            let collection = collection_name(&project_guid);

            // The chunk rows store qdrant_guid as text; parse back to UUIDv4 for upsert.
            let to_embed: Vec<(UUIDv4, String)> = chunks
                .into_iter()
                .map(|(g, code)| (UUIDv4(Uuid::parse_str(&g).unwrap_or_default()), code))
                .collect();

            let success =
                match embed_and_upsert(&*model_client, &qdrant, &collection, &to_embed, &token).await
                {
                    Ok(()) => true,
                    Err(EmbedUpsertError::Cancelled) => false,
                    Err(EmbedUpsertError::Embed(e)) => {
                        error!(
                            error = ?e,
                            project_guid,
                            path,
                            "Retry worker: embedding request failed; leaving file 'failed'. \
                             Check the model server at --model-server is up."
                        );
                        false
                    }
                    Err(EmbedUpsertError::Store(e)) => {
                        error!(
                            error = ?e,
                            project_guid,
                            path,
                            "Retry worker: Qdrant upsert failed; leaving file 'failed'. \
                             Check Qdrant is reachable at --qdrant-server."
                        );
                        false
                    }
                };

            set_file_status(
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
