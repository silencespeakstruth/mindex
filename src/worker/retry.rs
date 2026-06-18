use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::OptionalExtension;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::backend::v0::handlers::{IndexClaim, indexing_lock_key};
use crate::backend::v0::models::UUIDv4;
use crate::db::files::set_file_status;
use crate::db::qdrant::{VectorStore, collection_name};
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};
use crate::embed::{EmbedUpsertError, embed_and_upsert};
use crate::models::bge_m3::BGEm3Model;

pub(crate) const MAX_RETRIES: i64 = 3;

/// How often the worker re-warns about permanently-failed files (in addition to
/// the one-shot warning at startup).
const FAILED_WARN_INTERVAL: Duration = Duration::from_secs(3600);

/// Logs a WARN if any files have exhausted their retries (`status='failed'` with
/// `retry_count >= MAX_RETRIES`). The retry worker stops touching such files, so
/// they are otherwise invisible. Called once at startup and periodically by the
/// worker.
pub(crate) async fn warn_permanently_failed(db_pool: &SQLite3Pool, token: CancellationToken) {
    let count: i64 = db_pool
        .transaction(token, |tx| {
            tx.query_row(
                "SELECT COUNT(*) FROM project_files
                 WHERE status = 'failed' AND retry_count >= ?1",
                rusqlite::params![MAX_RETRIES],
                |r| r.get(0),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap_or(0);

    if count > 0 {
        warn!(
            count,
            max_retries = MAX_RETRIES,
            "Files have exhausted their retries and are stuck in 'failed'; they will not be \
             retried automatically. Re-push them to reindex, and check the model server \
             (--model-server) and Qdrant (--qdrant-server) reachability."
        );
    }
}

/// Logs a WARN if any files were left mid-flight (`status IN ('indexing',
/// 'just_uploaded')`) by a previous run — a crash or unclean shutdown. Called once at
/// startup, where the in-memory claim table is empty and no batch is live, so any such
/// row is definitively orphaned (not a live request). They are **not** force-failed:
/// the retry worker re-embeds their already-inserted chunks back to `indexed` (losing
/// no work), which is strictly better than dropping them to `failed` (that would burn
/// retry budget and risk trapping a 0-chunk file). This warning only surfaces them so
/// the operator knows recovery is pending — it happens within one `stuck_grace_mins`
/// window plus a sweep.
pub(crate) async fn warn_orphaned_indexing(db_pool: &SQLite3Pool, token: CancellationToken) {
    let count: i64 = db_pool
        .transaction(token, |tx| {
            tx.query_row(
                "SELECT COUNT(*) FROM project_files
                 WHERE status IN ('indexing', 'just_uploaded')",
                [],
                |r| r.get(0),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap_or(0);

    if count > 0 {
        warn!(
            count,
            "Files were left mid-indexing by a previous run (crash or unclean shutdown). \
             The retry worker will re-index them automatically (no work is lost); they are \
             not force-failed. If they linger, check the model server (--model-server) and \
             Qdrant (--qdrant-server) are reachable."
        );
    }
}

#[allow(clippy::too_many_arguments)] // irreducible worker deps (db, store, embedder, model_id, batch, grace, locks, token)
pub async fn run(
    db_pool: Arc<SQLite3Pool>,
    store: Arc<dyn VectorStore>,
    model_client: Arc<dyn BGEm3Model>,
    model_id: String,
    embed_batch: usize,
    stuck_grace_secs: i64,
    indexing_locks: Arc<Mutex<HashSet<String>>>,
    token: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Throttle the periodic permanently-failed warning. Start in the past so the
    // first sweep with such files warns immediately.
    let mut last_failed_warn = Instant::now() - FAILED_WARN_INTERVAL;

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = token.cancelled() => {
                info!("Retry worker: shutting down.");
                break;
            }
        }

        if last_failed_warn.elapsed() >= FAILED_WARN_INTERVAL {
            warn_permanently_failed(&db_pool, token.clone()).await;
            last_failed_warn = Instant::now();
        }

        let stuck_files: Vec<(String, String, String)> = db_pool
            .transaction(token.clone(), {
                let model_id = model_id.clone();
                move |tx| {
                // `stuck_grace_secs` only decides *when* a mid-flight row looks
                // abandoned; the actual guard against racing a still-live batch is the
                // per-file claim taken in `retry_file` (a live `/index` holds it for the
                // whole embed pass, so the worker skips). A too-short grace therefore no
                // longer corrupts — it just makes the worker try (and harmlessly skip)
                // sooner. The query still filters by grace to avoid churning fresh rows.
                tx.prepare(
                    "SELECT project_guid, path, model_id
                     FROM project_files
                     WHERE model_id = ?2
                       AND ((status IN ('just_uploaded', 'indexing')
                             AND status_updated_at < unixepoch() - ?3)
                         OR (status = 'failed'
                             AND retry_count < ?1
                             AND status_updated_at < unixepoch() - 60))",
                )?
                .query_map(rusqlite::params![MAX_RETRIES, model_id, stuck_grace_secs], |row| {
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

            retry_file(
                &db_pool, &*store, &*model_client, &project_guid, &path, &file_model_id,
                embed_batch, &indexing_locks, &token,
            )
            .await;
        }
    }
}

/// Re-indexes one stuck/failed file: `*→indexing → {indexed | failed}`. Extracted
/// from the `run` loop so it can be unit-tested with a fake `VectorStore` +
/// `BGEm3Model` (no live Qdrant or model server).
#[allow(clippy::too_many_arguments)] // irreducible per-file deps (db, store, embedder, ids, batch, locks, token)
async fn retry_file(
    db_pool: &SQLite3Pool,
    store: &dyn VectorStore,
    embedder: &dyn BGEm3Model,
    project_guid: &str,
    path: &str,
    model_id: &str,
    embed_batch: usize,
    indexing_locks: &Arc<Mutex<HashSet<String>>>,
    token: &CancellationToken,
) {
    // Claim the per-file slot before touching it. A live `/index` request working this
    // same file holds the claim through its whole pipeline, so `None` means "in flight
    // right now" — skip and let the next sweep try. This makes the worker↔handler race
    // a lock invariant, not merely a consequence of `stuck_grace_secs` exceeding the
    // longest request. Held (as `_claim`) for the whole retry; released on return.
    let key = indexing_lock_key(project_guid, model_id, path);
    let Some(_claim) = IndexClaim::try_acquire(indexing_locks, key) else {
        info!(%project_guid, %path, "Retry worker: file is being indexed live; skipping this sweep.");
        return;
    };

    // Re-check status under the claim: the sweep's SELECT excluded 'cancelled', but a
    // `POST /cancel` could have landed in the window between that SELECT and this claim.
    // `cancelled → indexing` is a legal transition, so without this guard the worker
    // would resurrect a just-cancelled file. Only proceed if it's still in a retryable
    // state ('indexing' stuck, 'just_uploaded', or 'failed'); otherwise leave it be.
    let current_status: Option<String> = db_pool
        .transaction(token.clone(), {
            let (pg, p, m) = (project_guid.to_string(), path.to_string(), model_id.to_string());
            move |tx| {
                tx.query_row(
                    "SELECT status FROM project_files
                     WHERE project_guid = ?1 AND path = ?2 AND model_id = ?3",
                    rusqlite::params![pg, p, m],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(SQLite3PoolError::from)
            }
        })
        .await
        .unwrap_or_default();
    if !matches!(
        current_status.as_deref(),
        Some("indexing" | "just_uploaded" | "failed")
    ) {
        info!(
            %project_guid, %path, status = ?current_status,
            "Retry worker: file is no longer retryable (cancelled/deleted since the sweep); skipping."
        );
        return;
    }

    // Move to 'indexing' first so the whole attempt is a clean
    // {failed,indexing,…}→indexing→{indexed,failed} path through the state machine
    // (the triggers forbid failed→failed / failed→indexed directly).
    set_file_status(db_pool, project_guid, path, model_id, "indexing", false, token.clone()).await;

    let chunks: Vec<(String, String)> = db_pool
        .transaction(token.clone(), {
            let (pg, p, m) = (project_guid.to_string(), path.to_string(), model_id.to_string());
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
        // No active chunks → the file sliced to nothing (too short). The worker can't
        // re-slice, so mark it 'indexed' (0 chunks) rather than looping it to 'failed'
        // forever — 'failed'→'indexed' is illegal, so a wrong 'failed' here would trap it.
        info!(%project_guid, %path, "Retry worker: no active chunks (too short); marking 'indexed'.");
        set_file_status(db_pool, project_guid, path, model_id, "indexed", false, token.clone()).await;
        return;
    }

    let collection = collection_name(project_guid);

    // The chunk rows store qdrant_guid as text; parse back to UUIDv4 for upsert.
    let to_embed: Vec<(UUIDv4, String)> = chunks
        .into_iter()
        .map(|(g, code)| (UUIDv4(Uuid::parse_str(&g).unwrap_or_default()), code))
        .collect();

    let success = match embed_and_upsert(embedder, store, &collection, &to_embed, token, embed_batch)
        .await
    {
        Ok(()) => true,
        Err(EmbedUpsertError::Cancelled) => false,
        Err(EmbedUpsertError::Embed(e)) => {
            error!(
                error = ?e,
                %project_guid,
                %path,
                "Retry worker: embedding request failed; leaving file 'failed'. \
                 Check the model server at --model-server is up."
            );
            false
        }
        Err(EmbedUpsertError::Store(e)) => {
            error!(
                error = ?e,
                %project_guid,
                %path,
                "Retry worker: Qdrant upsert failed; leaving file 'failed'. \
                 Check Qdrant is reachable at --qdrant-server."
            );
            false
        }
    };

    set_file_status(
        db_pool,
        project_guid,
        path,
        model_id,
        if success { "indexed" } else { "failed" },
        !success,
        token.clone(),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rusqlite::params;
    use std::collections::HashMap;
    use std::path::Path;

    use crate::db::qdrant::{ChunkAsVector, SearchHit, VectorStoreError};
    use crate::models::bge_m3::{BGEm3EmbedRequest, BGEm3EmbedResponse, EncodeError};

    const PG: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const MODEL: &str = "BAAI/bge-m3";
    const PATH: &str = "a.rs";

    struct OkEmbedder;
    #[async_trait]
    impl BGEm3Model for OkEmbedder {
        async fn encode(
            &self,
            req: BGEm3EmbedRequest,
            _token: CancellationToken,
        ) -> Result<BGEm3EmbedResponse, EncodeError> {
            let n = req.texts.len();
            Ok(BGEm3EmbedResponse {
                dense_vecs: vec![vec![0.1; 4]; n],
                sparse_vecs: vec![HashMap::from([(1u32, 0.5f32)]); n],
                colbert_vecs: vec![vec![vec![0.1; 4]]; n],
            })
        }
        async fn health(&self) -> Result<(), EncodeError> {
            unreachable!("the retry worker does not call health")
        }
    }

    /// `VectorStore` fake whose `insert_batch` succeeds or fails on demand.
    struct Store {
        fail_upsert: bool,
    }
    #[async_trait]
    impl VectorStore for Store {
        async fn insert_batch(
            &self,
            _collection: &str,
            _chunks: Vec<ChunkAsVector>,
        ) -> Result<(), VectorStoreError> {
            if self.fail_upsert {
                Err(VectorStoreError("forced upsert failure".to_string()))
            } else {
                Ok(())
            }
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

    /// Migrated pool with a project + a file currently in `'failed'` (retry_count=1)
    /// and `n_chunks` active chunks — i.e. a file the retry worker would pick up.
    async fn pool_with_failed_file(n_chunks: usize) -> SQLite3Pool {
        let pool = SQLite3Pool::new(Path::new(":memory:"), 1);
        pool.transaction(CancellationToken::new(), move |tx| {
            for m in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![PG, MODEL],
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, ?2, ?3, ?4, 'rust', 'indexing')",
                params![PG, MODEL, PATH, "0".repeat(64)],
            )?;
            for _ in 0..n_chunks {
                tx.execute(
                    "INSERT INTO project_file_chunks
                         (project_guid, file_path, model_id, code, qdrant_guid,
                          start_line, end_line, start_column, end_column, status)
                     VALUES (?1, ?2, ?3, 'code', ?4, 1, 2, 0, 1, 'active')",
                    params![PG, PATH, MODEL, Uuid::new_v4().simple().to_string()],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
        // indexing → failed (retry_count = 1): the state the worker retries from.
        set_file_status(&pool, PG, PATH, MODEL, "failed", true, CancellationToken::new()).await;
        pool
    }

    async fn current(pool: &SQLite3Pool) -> (String, i64) {
        pool.transaction(CancellationToken::new(), |tx| {
            tx.query_row(
                "SELECT status, retry_count FROM project_files
                 WHERE project_guid = ?1 AND model_id = ?2 AND path = ?3",
                params![PG, MODEL, PATH],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    async fn log_pairs(pool: &SQLite3Pool) -> Vec<(Option<String>, String)> {
        pool.transaction(CancellationToken::new(), |tx| {
            tx.prepare("SELECT old_status, new_status FROM project_file_status_log ORDER BY id")?
                .query_map([], |r| {
                    Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn retry_success_goes_failed_indexing_indexed_and_resets_count() {
        let pool = pool_with_failed_file(2).await;
        let store = Store { fail_upsert: false };

        let locks = Arc::new(Mutex::new(HashSet::new()));
        retry_file(&pool, &store, &OkEmbedder, PG, PATH, MODEL, 64, &locks, &CancellationToken::new()).await;

        assert_eq!(current(&pool).await, ("indexed".to_string(), 0));
        // The transition log proves the path went through 'indexing', never failed→indexed.
        let pairs = log_pairs(&pool).await;
        assert!(pairs.contains(&(Some("failed".into()), "indexing".into())), "{pairs:?}");
        assert!(pairs.contains(&(Some("indexing".into()), "indexed".into())), "{pairs:?}");
    }

    #[tokio::test]
    async fn retry_store_failure_goes_failed_indexing_failed_and_bumps_count() {
        let pool = pool_with_failed_file(2).await;
        let store = Store { fail_upsert: true };

        let locks = Arc::new(Mutex::new(HashSet::new()));
        retry_file(&pool, &store, &OkEmbedder, PG, PATH, MODEL, 64, &locks, &CancellationToken::new()).await;

        // Was failed(1) → indexing → failed(2).
        assert_eq!(current(&pool).await, ("failed".to_string(), 2));
        let pairs = log_pairs(&pool).await;
        assert!(pairs.contains(&(Some("failed".into()), "indexing".into())), "{pairs:?}");
        assert!(pairs.contains(&(Some("indexing".into()), "failed".into())), "{pairs:?}");
    }

    #[tokio::test]
    async fn retry_with_no_active_chunks_marks_indexed() {
        let pool = pool_with_failed_file(0).await;
        let store = Store { fail_upsert: false };

        let locks = Arc::new(Mutex::new(HashSet::new()));
        retry_file(&pool, &store, &OkEmbedder, PG, PATH, MODEL, 64, &locks, &CancellationToken::new()).await;

        // No chunks (too short) → indexing → indexed, retry_count reset. Must NOT be
        // 'failed' (that would trap it: failed→indexed is an illegal transition).
        assert_eq!(current(&pool).await, ("indexed".to_string(), 0));
    }

    /// If a live `/index` request already holds the file's claim, the worker must NOT
    /// touch it — no status change, no transition logged. This is the worker↔handler
    /// race guard (the key here is built exactly as the handler builds it).
    #[tokio::test]
    async fn retry_skips_when_file_claim_is_held() {
        let pool = pool_with_failed_file(2).await;
        let store = Store { fail_upsert: true }; // would fail loudly if it ran

        let locks = Arc::new(Mutex::new(HashSet::new()));
        // Simulate a live `/index` holding the slot.
        let key = indexing_lock_key(PG, MODEL, PATH);
        let _held = IndexClaim::try_acquire(&locks, key).expect("slot starts free");

        let log_before = log_pairs(&pool).await.len();
        retry_file(&pool, &store, &OkEmbedder, PG, PATH, MODEL, 64, &locks, &CancellationToken::new()).await;

        // Untouched: still failed(1), and not a single new transition was logged.
        assert_eq!(current(&pool).await, ("failed".to_string(), 1));
        assert_eq!(log_pairs(&pool).await.len(), log_before, "retry must log nothing when the claim is held");
    }

    /// If a `POST /cancel` flipped the file to `cancelled` in the window between the
    /// sweep's SELECT and the per-file claim, the worker must NOT resurrect it —
    /// `cancelled → indexing` is a legal transition, so only the status re-check
    /// under the claim prevents the worker from re-driving a just-cancelled file.
    #[tokio::test]
    async fn retry_skips_when_file_was_cancelled_since_the_sweep() {
        // Reuse the failed-file fixture, then legally move indexing→...→cancelled.
        let pool = pool_with_failed_file(2).await;
        // failed→indexing (re-push) →cancelled, the state a concurrent /cancel leaves.
        set_file_status(&pool, PG, PATH, MODEL, "indexing", false, CancellationToken::new()).await;
        set_file_status(&pool, PG, PATH, MODEL, "cancelled", false, CancellationToken::new()).await;
        let log_before = log_pairs(&pool).await.len();

        let store = Store { fail_upsert: true }; // would fail loudly if it ran
        let locks = Arc::new(Mutex::new(HashSet::new()));
        retry_file(&pool, &store, &OkEmbedder, PG, PATH, MODEL, 64, &locks, &CancellationToken::new()).await;

        // Untouched: still 'cancelled', no new transition logged (not re-driven).
        assert_eq!(current(&pool).await.0, "cancelled");
        assert_eq!(
            log_pairs(&pool).await.len(),
            log_before,
            "retry must not resurrect a cancelled file"
        );
    }
}
