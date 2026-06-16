use axum::http::StatusCode;
use rusqlite::{Connection, Transaction};
use std::{path::Path, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use uuid::Uuid;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SQLite3PoolError {
    #[error("sql error: {0}")]
    Sql(#[from] rusqlite::Error),

    #[error("pool empty")]
    PoolEmpty,

    #[error("cancelled")]
    Cancelled,

    #[error("status code: {0}")]
    HTTPStatusCode(StatusCode),
}

impl From<StatusCode> for SQLite3PoolError {
    fn from(code: StatusCode) -> Self {
        SQLite3PoolError::HTTPStatusCode(code)
    }
}

pub struct SQLite3Pool {
    conns: Arc<Mutex<Vec<Connection>>>,
}

impl SQLite3Pool {
    pub fn new(db_path: &Path, len: usize) -> Self {
        let mut conns = Vec::with_capacity(len);

        for _ in 0..len {
            let conn = Connection::open(db_path).unwrap_or_else(|err| {
                panic!(
                    "Failed to open SQLite database at {db_path:?}: {err}. \
                     Check the path exists, is writable by this process, and the disk is not full."
                )
            });

            conn.execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA foreign_keys = ON;
                PRAGMA synchronous = NORMAL;
                PRAGMA page_size = 16384;
                "#,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "Failed to apply startup PRAGMAs on {db_path:?}: {err}. \
                     The database file may be corrupt or locked by another process."
                )
            });

            conns.push(conn);
        }

        info!(?len, ?db_path, "Initialized an SQLite3 connection pool.");

        Self {
            conns: Arc::new(Mutex::new(conns)),
        }
    }

    async fn acquire(&self) -> Option<Connection> {
        let mut guard = self.conns.lock().await;
        guard.pop()
    }

    pub async fn transaction<F, T>(
        &self,
        token: CancellationToken,
        f: F,
    ) -> Result<T, SQLite3PoolError>
    where
        F: FnOnce(&Transaction) -> Result<T, SQLite3PoolError> + Send + 'static,
        T: Send + 'static,
    {
        if token.is_cancelled() {
            return Err(SQLite3PoolError::Cancelled);
        }

        let conn = self.acquire().await.ok_or(SQLite3PoolError::PoolEmpty)?;

        let span = tracing::info_span!("sqlite3 transaction", sqlite3_tx_guid = %Uuid::new_v4());

        // The blocking task returns the connection to the pool itself, rather than
        // relying on the awaiting future to do it after `handle.await`. If the caller's
        // future is dropped mid-transaction (client disconnect, cancellation), the
        // spawn_blocking task still runs to completion and re-pushes the connection —
        // otherwise every cancelled request would permanently leak one connection and
        // the pool would be exhausted after `db_pool_size` disconnects.
        let conns = Arc::clone(&self.conns);
        let handle = tokio::task::spawn_blocking(move || {
            let mut conn = conn;

            let res: Result<T, SQLite3PoolError> = (|| {
                let _guard = span.enter();

                let tx = conn.transaction()?;
                let val = f(&tx)?;
                tx.commit()?;

                info!("Transaction committed.");

                Ok(val)
            })();

            // `blocking_lock` is safe here: we are on a dedicated spawn_blocking thread,
            // not inside the async runtime. The lock is held only for the push.
            conns.blocking_lock().push(conn);

            res
        });

        handle.await.map_err(|join_err| {
            // The blocking task panicked (a bug in `f`) or was aborted. The connection
            // for a panicked task is dropped rather than returned — acceptable, since a
            // panicking transaction closure is a programmer error, not a runtime condition.
            error!(%join_err, "SQLite transaction task failed to join (closure panicked?).");
            SQLite3PoolError::Cancelled
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Each connection to ":memory:" is an independent database, so the pool is
    // sized to 1 wherever shared state across transactions matters.
    fn pool(size: usize) -> SQLite3Pool {
        SQLite3Pool::new(Path::new(":memory:"), size)
    }

    #[tokio::test]
    async fn transaction_commits_and_returns_value() {
        let p = pool(1);
        let n = p
            .transaction(CancellationToken::new(), |tx| {
                tx.execute_batch("CREATE TABLE t (x INTEGER);")?;
                tx.execute("INSERT INTO t VALUES (42)", [])?;
                let n: i64 = tx.query_row("SELECT x FROM t", [], |r| r.get(0))?;
                Ok(n)
            })
            .await
            .unwrap();
        assert_eq!(n, 42);
    }

    #[tokio::test]
    async fn precancelled_token_short_circuits() {
        let p = pool(1);
        let token = CancellationToken::new();
        token.cancel();
        let res = p.transaction(token, |_tx| Ok(())).await;
        assert!(matches!(res, Err(SQLite3PoolError::Cancelled)));
    }

    #[tokio::test]
    async fn empty_pool_reports_pool_empty() {
        // Zero connections: every acquire fails.
        let p = pool(0);
        let res = p.transaction(CancellationToken::new(), |_tx| Ok(())).await;
        assert!(matches!(res, Err(SQLite3PoolError::PoolEmpty)));
    }

    // Regression for the connection-leak bug: if the caller's future is dropped while
    // the blocking transaction is still running (client disconnect / cancellation), the
    // connection must still be returned to the pool by the blocking task — otherwise the
    // pool drains and every later transaction returns PoolEmpty.
    #[tokio::test]
    async fn connection_returned_when_future_dropped_midflight() {
        let p = pool(1);

        // A transaction whose closure blocks for 300ms.
        let slow = p.transaction(CancellationToken::new(), |tx| {
            std::thread::sleep(Duration::from_millis(300));
            tx.execute_batch("CREATE TABLE IF NOT EXISTS t (x);")?;
            Ok(())
        });

        // Drop the future after 50ms — it is parked at `handle.await`, so this is exactly
        // the "client went away mid-transaction" case. The spawn_blocking task keeps
        // running to completion regardless.
        let timed_out = tokio::time::timeout(Duration::from_millis(50), slow).await;
        assert!(timed_out.is_err(), "the slow transaction future should have been dropped");

        // Give the orphaned blocking task time to finish and push the connection back.
        tokio::time::sleep(Duration::from_millis(400)).await;

        // The single connection must be back in the pool. Before the fix this was lost
        // and the call below returned PoolEmpty.
        let res = p.transaction(CancellationToken::new(), |_tx| Ok(7)).await;
        assert!(
            matches!(res, Ok(7)),
            "connection leaked on drop — pool exhausted: {res:?}"
        );
    }
}
