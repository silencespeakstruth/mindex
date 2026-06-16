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
    SQL(#[from] rusqlite::Error),

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
