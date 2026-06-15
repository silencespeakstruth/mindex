use axum::http::StatusCode;
use rusqlite::{Connection, Transaction};
use std::{path::Path, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::info;
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
            let conn = Connection::open(db_path).unwrap();

            conn.execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA foreign_keys = ON;
                PRAGMA synchronous = NORMAL;
                PRAGMA page_size = 16384;
                "#,
            )
            .unwrap();

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

    async fn release(&self, conn: Connection) {
        let mut guard = self.conns.lock().await;
        guard.push(conn);
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

        let handle = tokio::task::spawn_blocking(move || {
            let mut conn = conn;

            let res: Result<T, SQLite3PoolError> = (|| {
                let _guard = span.enter();

                let tx = conn.transaction()?;
                let val = f(&tx)?;
                tx.commit()?;

                info!("Commit.");

                Ok(val)
            })();

            (conn, res)
        });

        let (conn, res) = handle.await.map_err(|_| SQLite3PoolError::Cancelled)?;

        self.release(conn).await;

        res
    }
}
