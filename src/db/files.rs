//! Shared `project_files.status` transition, used by the indexing handler's
//! recovery paths and by the retry worker — previously duplicated in both.

use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::db::sqlite3::SQLite3Pool;

/// Sets a file's `status`, stamping `status_updated_at`. `retry_count` is reset to
/// 0 on reaching `'indexed'` (a clean success clears prior failures), bumped when
/// `increment_retry` (a failure), and left untouched otherwise.
///
/// Best-effort: callers invoke this on recovery/retry paths where there is nothing
/// better to do on failure — but a rejected transition (the state-machine triggers
/// raise `SQLITE_CONSTRAINT_TRIGGER`) is a real bug, so it is logged rather than
/// silently swallowed.
pub async fn set_file_status(
    db_pool: &SQLite3Pool,
    project_guid: &str,
    path: &str,
    model_id: &str,
    status: &'static str,
    increment_retry: bool,
    token: CancellationToken,
) {
    let (pg, p, m) = (
        project_guid.to_string(),
        path.to_string(),
        model_id.to_string(),
    );
    // A reindex/retry that reaches 'indexed' clears the failure counter; a failure
    // bumps it; anything else (e.g. moving to 'indexing') leaves it as-is.
    let retry_expr = if status == "indexed" {
        "0"
    } else if increment_retry {
        "retry_count + 1"
    } else {
        "retry_count"
    };
    let sql = format!(
        "UPDATE project_files
         SET status = ?1, retry_count = {retry_expr}, status_updated_at = unixepoch()
         WHERE project_guid = ?2 AND path = ?3 AND model_id = ?4"
    );

    let result = db_pool
        .transaction(token, move |tx| {
            tx.execute(&sql, rusqlite::params![status, pg, p, m])?;
            Ok(())
        })
        .await;

    if let Err(e) = result {
        warn!(
            error = %e,
            project_guid,
            path,
            new_status = status,
            "Failed to set file status (rejected state transition or DB error)."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite3::SQLite3PoolError;
    use rusqlite::params;
    use std::path::Path;

    const PG: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const MODEL: &str = "BAAI/bge-m3";
    const PATH: &str = "a.rs";

    async fn migrated_pool() -> SQLite3Pool {
        let pool = SQLite3Pool::new(Path::new(":memory:"), 1);
        pool.transaction(CancellationToken::new(), |tx| {
            for m in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![PG, MODEL],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        pool
    }

    /// Pool with one project + one file inserted at `initial` (must be a legal
    /// entry status). Returns the pool.
    async fn pool_with_file(initial: &'static str) -> SQLite3Pool {
        let pool = migrated_pool().await;
        insert_file(&pool, initial).await.expect("legal initial insert");
        pool
    }

    async fn insert_file(pool: &SQLite3Pool, status: &'static str) -> Result<(), SQLite3PoolError> {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, ?2, ?3, ?4, 'rust', ?5)",
                params![PG, MODEL, PATH, "0".repeat(64), status],
            )?;
            Ok(())
        })
        .await
    }

    /// Raw status UPDATE (bypasses set_file_status) so the trigger is what's tested.
    async fn transition(pool: &SQLite3Pool, new: &'static str) -> Result<(), SQLite3PoolError> {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "UPDATE project_files SET status = ?1, status_updated_at = unixepoch()
                 WHERE project_guid = ?2 AND model_id = ?3 AND path = ?4",
                params![new, PG, MODEL, PATH],
            )?;
            Ok(())
        })
        .await
    }

    fn is_trigger_rejection(res: &Result<(), SQLite3PoolError>) -> bool {
        matches!(res, Err(SQLite3PoolError::Sql(e)) if e.to_string().contains("illegal"))
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

    async fn log(pool: &SQLite3Pool) -> Vec<(Option<String>, String)> {
        pool.transaction(CancellationToken::new(), |tx| {
            tx.prepare(
                "SELECT old_status, new_status FROM project_file_status_log ORDER BY id",
            )?
            .query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn indexing_reaches_each_terminal() {
        for terminal in ["indexed", "cancelled", "failed"] {
            let pool = pool_with_file("indexing").await;
            assert!(transition(&pool, terminal).await.is_ok(), "indexing→{terminal} must be legal");
        }
    }

    #[tokio::test]
    async fn any_state_can_restart_indexing() {
        // indexed→indexing (reindex)
        let pool = pool_with_file("indexing").await;
        transition(&pool, "indexed").await.unwrap();
        assert!(transition(&pool, "indexing").await.is_ok(), "indexed→indexing must be legal");

        // failed→indexing (retry)
        let pool = pool_with_file("indexing").await;
        transition(&pool, "failed").await.unwrap();
        assert!(transition(&pool, "indexing").await.is_ok(), "failed→indexing must be legal");

        // cancelled→indexing (re-push)
        let pool = pool_with_file("indexing").await;
        transition(&pool, "cancelled").await.unwrap();
        assert!(transition(&pool, "indexing").await.is_ok(), "cancelled→indexing must be legal");

        // idempotent indexing→indexing (concurrent upserts)
        let pool = pool_with_file("indexing").await;
        assert!(transition(&pool, "indexing").await.is_ok(), "indexing→indexing must be legal");
    }

    #[tokio::test]
    async fn illegal_transitions_are_rejected() {
        // (from_state, to_state) pairs the triggers must forbid.
        let cases = [
            ("indexed", "failed"),
            ("indexed", "cancelled"),
            ("indexed", "indexed"),   // non-indexing self-loop
            ("failed", "indexed"),    // must go via indexing
            ("failed", "failed"),
            ("failed", "cancelled"),
            ("cancelled", "indexed"),
            ("just_uploaded", "indexed"),  // skips the work
            ("just_uploaded", "failed"),
        ];
        for (from, to) in cases {
            // Reach `from` legally from the 'indexing' entry state.
            let pool = pool_with_file("indexing").await;
            if from != "indexing" {
                if from == "just_uploaded" {
                    // can't transition *to* just_uploaded; re-seed instead
                    let pool = pool_with_file("just_uploaded").await;
                    let res = transition(&pool, to).await;
                    assert!(is_trigger_rejection(&res), "{from}→{to} must be rejected, got {res:?}");
                    continue;
                }
                transition(&pool, from).await.unwrap_or_else(|e| panic!("setup {from}: {e:?}"));
            }
            let res = transition(&pool, to).await;
            assert!(is_trigger_rejection(&res), "{from}→{to} must be rejected, got {res:?}");
        }
    }

    #[tokio::test]
    async fn insert_guard_allows_only_entry_states() {
        let pool = migrated_pool().await;
        assert!(insert_file(&pool, "indexing").await.is_ok());

        let pool = migrated_pool().await;
        assert!(insert_file(&pool, "just_uploaded").await.is_ok());

        for terminal in ["indexed", "cancelled", "failed", "deleted"] {
            let pool = migrated_pool().await;
            let res = insert_file(&pool, terminal).await;
            assert!(is_trigger_rejection(&res), "inserting initial {terminal} must be rejected, got {res:?}");
        }
    }

    #[tokio::test]
    async fn deleted_is_reachable_from_any_state_and_terminal() {
        // any → deleted is legal (DELETE /files marks the file for GC).
        let pool = pool_with_file("indexing").await;
        transition(&pool, "indexed").await.unwrap();
        assert!(transition(&pool, "deleted").await.is_ok(), "indexed→deleted must be legal");

        let pool = pool_with_file("indexing").await;
        transition(&pool, "failed").await.unwrap();
        assert!(transition(&pool, "deleted").await.is_ok(), "failed→deleted must be legal");

        // deleted → indexing is legal: re-indexing a path pending deletion resurrects it.
        let pool = pool_with_file("indexing").await;
        transition(&pool, "deleted").await.unwrap();
        assert!(transition(&pool, "indexing").await.is_ok(), "deleted→indexing must be legal");

        // deleted is otherwise terminal: no jump straight to a work-terminal.
        for to in ["indexed", "failed", "cancelled"] {
            let pool = pool_with_file("indexing").await;
            transition(&pool, "deleted").await.unwrap();
            let res = transition(&pool, to).await;
            assert!(is_trigger_rejection(&res), "deleted→{to} must be rejected, got {res:?}");
        }
    }

    #[tokio::test]
    async fn transition_log_records_full_history() {
        let pool = pool_with_file("indexing").await; // insert: (NULL → indexing)
        transition(&pool, "indexed").await.unwrap(); // (indexing → indexed)
        transition(&pool, "indexing").await.unwrap(); // reindex: (indexed → indexing)
        transition(&pool, "failed").await.unwrap(); // (indexing → failed)

        assert_eq!(
            log(&pool).await,
            vec![
                (None, "indexing".to_string()),
                (Some("indexing".to_string()), "indexed".to_string()),
                (Some("indexed".to_string()), "indexing".to_string()),
                (Some("indexing".to_string()), "failed".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn set_file_status_increments_then_resets_retry_count() {
        let pool = pool_with_file("indexing").await;

        // A failure bumps retry_count.
        set_file_status(&pool, PG, PATH, MODEL, "failed", true, CancellationToken::new()).await;
        assert_eq!(current(&pool).await, ("failed".to_string(), 1));

        // Retry: failed→indexing (no change), then a success resets the counter.
        set_file_status(&pool, PG, PATH, MODEL, "indexing", false, CancellationToken::new()).await;
        assert_eq!(current(&pool).await, ("indexing".to_string(), 1));

        set_file_status(&pool, PG, PATH, MODEL, "indexed", false, CancellationToken::new()).await;
        assert_eq!(current(&pool).await, ("indexed".to_string(), 0));
    }
}
