use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use qdrant_client::Qdrant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::db::qdrant::{collection_name, delete_batch};
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};

pub async fn run(db_pool: Arc<SQLite3Pool>, qdrant: Arc<Qdrant>, token: CancellationToken) {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = token.cancelled() => {
                info!("GC worker: shutting down.");
                break;
            }
        }

        info!("GC worker: starting sweep.");
        sweep(&db_pool, &qdrant, &token).await;
        info!("GC worker: sweep complete.");
    }
}

async fn sweep(db_pool: &SQLite3Pool, qdrant: &Qdrant, token: &CancellationToken) {
    loop {
        if token.is_cancelled() {
            break;
        }

        let batch: Vec<(String, String)> = match db_pool
            .transaction(token.clone(), |tx| {
                tx.prepare(
                    "SELECT qdrant_guid, project_guid
                     FROM project_file_chunks
                     WHERE status = 'deleted'
                     LIMIT 256",
                )?
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
            })
            .await
        {
            Ok(b) => b,
            Err(SQLite3PoolError::Cancelled) => break,
            Err(e) => {
                error!(?e, "GC: failed to query deleted chunks");
                break;
            }
        };

        if batch.is_empty() {
            break;
        }

        // Group by project so we issue one delete call per collection.
        let mut by_project: HashMap<String, Vec<String>> = HashMap::new();
        for (guid, project) in &batch {
            by_project
                .entry(project.clone())
                .or_default()
                .push(guid.clone());
        }

        for (project_guid, guids) in &by_project {
            let coll = collection_name(project_guid);
            if let Err(e) = delete_batch(qdrant, &coll, guids.clone()).await {
                error!(?e, project_guid, "GC: Qdrant delete_batch failed");
            }
        }

        // Hard-delete the rows that have been removed from Qdrant.
        let all_guids: Vec<String> = batch.into_iter().map(|(g, _)| g).collect();
        let _ = db_pool
            .transaction(token.clone(), move |tx| {
                let placeholders = (1..=all_guids.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "DELETE FROM project_file_chunks
                     WHERE status = 'deleted' AND qdrant_guid IN ({placeholders})"
                );
                tx.execute(&sql, rusqlite::params_from_iter(all_guids.iter()))?;
                Ok(())
            })
            .await;
    }
}
