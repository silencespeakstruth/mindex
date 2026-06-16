//! Shared `project_files.status` transition, used by the indexing handler's
//! recovery paths and by the retry worker — previously duplicated in both.

use tokio_util::sync::CancellationToken;

use crate::db::sqlite3::SQLite3Pool;

/// Sets a file's `status` (bumping `retry_count` when `increment_retry`) and
/// stamps `status_updated_at`. Best-effort: the result is intentionally ignored,
/// since callers invoke this on recovery/cleanup paths where there is nothing
/// better to do on failure.
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
