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
                error!(error = ?e, "GC worker: failed to query deleted chunks from SQLite; aborting this sweep.");
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

        // Only hard-delete SQLite rows whose Qdrant vectors were actually removed.
        // If a collection's delete fails (transient Qdrant error), we keep its rows
        // marked 'deleted' so the next sweep retries them — otherwise the vectors would
        // be orphaned in Qdrant forever, with no SQLite row left to track them.
        let mut confirmed_deleted: Vec<String> = Vec::new();
        for (project_guid, guids) in &by_project {
            let coll = collection_name(project_guid);
            match delete_batch(qdrant, &coll, guids.clone()).await {
                Ok(()) => confirmed_deleted.extend(guids.iter().cloned()),
                Err(e) => error!(
                    error = ?e,
                    project_guid,
                    collection = %coll,
                    chunk_count = guids.len(),
                    "GC: Qdrant delete_batch failed; keeping rows for next sweep. \
                     Check Qdrant reachability and that the collection exists."
                ),
            }
        }

        if confirmed_deleted.is_empty() {
            // Nothing was confirmed removed from Qdrant this iteration (every collection
            // failed). Stop the inner loop to avoid spinning on the same un-deletable
            // batch; the next scheduled sweep will retry.
            break;
        }

        // Hard-delete only the rows whose vectors are confirmed gone from Qdrant.
        let _ = db_pool
            .transaction(token.clone(), move |tx| {
                let placeholders = (1..=confirmed_deleted.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "DELETE FROM project_file_chunks
                     WHERE status = 'deleted' AND qdrant_guid IN ({placeholders})"
                );
                tx.execute(&sql, rusqlite::params_from_iter(confirmed_deleted.iter()))?;
                Ok(())
            })
            .await;
    }
}
