use axum::http::StatusCode;
use rusqlite::{Connection, Transaction};
use std::{path::Path, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::backend::metrics::{DbMetrics, Metrics, OutcomeLabels};
use tracing::{error, info, warn};
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

    /// The transaction closure panicked (or its blocking task was aborted). Kept
    /// apart from [`Cancelled`](SQLite3PoolError::Cancelled) because the two are
    /// opposite diagnoses that were once the same value: a panic is a bug in this
    /// process, and reporting it as a cancellation told the client it had closed a
    /// connection it never closed, told the dashboard a disconnect had happened,
    /// and suppressed the call sites' own `error!` (every one of them skips logging
    /// for `Cancelled`). It also costs a pool connection, so it must be loud.
    #[error("transaction task panicked")]
    Panicked,

    #[error("status code: {0}")]
    HTTPStatusCode(StatusCode),
}

impl From<StatusCode> for SQLite3PoolError {
    fn from(code: StatusCode) -> Self {
        SQLite3PoolError::HTTPStatusCode(code)
    }
}

/// Whether a transaction runs under SQLite's foreign-key enforcement.
///
/// Private: `Off` is reachable only through
/// [`migration_transaction`](SQLite3Pool::migration_transaction), which documents
/// the one situation that needs it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ForeignKeys {
    On,
    Off,
}

pub struct SQLite3Pool {
    conns: Arc<Mutex<Vec<Connection>>>,
    /// Total connections the pool was created with (the high-water mark). Stored so
    /// `GET /status` can report saturation as `available()` / `size`.
    size: usize,
    /// Instrumented in place rather than behind a decorator: the pool is
    /// deliberately not a trait (`transaction`'s generic closure is not
    /// object-safe), and `transaction` is the single choke point every database
    /// operation in the process passes through. `None` in the many `:memory:`
    /// test pools, which is why this is a builder rather than a `new` parameter.
    metrics: Option<DbMetrics>,
}

impl SQLite3Pool {
    /// `page_size_bytes` and `synchronous` come from `[database]` config.
    /// `journal_mode = WAL` and `foreign_keys = ON` are **not** configurable: WAL is
    /// required by the concurrency model (readers during writes) and foreign keys are
    /// a correctness invariant (the chunk→file RESTRICT FK).
    pub fn new(db_path: &Path, len: usize, page_size_bytes: u32, synchronous: &str) -> Self {
        let mut conns = Vec::with_capacity(len);

        // `page_size` only takes effect before the DB is first written, so it must be
        // set before any other statement; the rest follow. WAL + foreign_keys are fixed.
        let pragmas = format!(
            "PRAGMA page_size = {page_size_bytes};\n\
             PRAGMA journal_mode = WAL;\n\
             PRAGMA foreign_keys = ON;\n\
             PRAGMA synchronous = {synchronous};\n"
        );

        for _ in 0..len {
            let conn = Connection::open(db_path).unwrap_or_else(|err| {
                panic!(
                    "Failed to open SQLite database at {db_path:?}: {err}. \
                     Check the path exists, is writable by this process, and the disk is not full."
                )
            });

            conn.execute_batch(&pragmas).unwrap_or_else(|err| {
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
            size: len,
            metrics: None,
        }
    }

    /// Record transaction latency, outcomes and pool-acquire failures.
    #[must_use]
    pub fn with_metrics(mut self, metrics: &Metrics) -> Self {
        self.metrics = Some(metrics.db.clone());
        self
    }

    /// Total connections the pool holds when fully idle.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Connections currently free (not checked out). A momentary snapshot — useful
    /// for the `GET /status` saturation report, not for control flow.
    pub async fn available(&self) -> usize {
        self.conns.lock().await.len()
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
        self.run(token, f, ForeignKeys::On).await
    }

    /// [`transaction`](Self::transaction) with foreign-key enforcement **off** for
    /// the duration, restored before the connection goes back to the pool.
    ///
    /// One caller, by design: the startup migration. A migration that rebuilds a
    /// table has to rename the old one out of the way, and SQLite rewrites every
    /// `REFERENCES` clause naming a renamed table whenever `foreign_keys` is ON —
    /// so the child tables would follow the corpse instead of adopting the
    /// replacement. `PRAGMA legacy_alter_table` does not cover that (it spares
    /// trigger and view bodies, never foreign keys), and the pragma is a silent
    /// no-op inside a transaction, so it cannot be flipped by the migration SQL
    /// itself. Turning it off around the whole transaction is what SQLite's own
    /// table-rebuild procedure prescribes.
    ///
    /// Atomicity is unchanged — the transaction still commits or rolls back as one
    /// — and the migration verifies its own result with `PRAGMA foreign_key_check`
    /// before returning, which is the check this pragma suspends.
    pub async fn migration_transaction<F, T>(
        &self,
        token: CancellationToken,
        f: F,
    ) -> Result<T, SQLite3PoolError>
    where
        F: FnOnce(&Transaction) -> Result<T, SQLite3PoolError> + Send + 'static,
        T: Send + 'static,
    {
        self.run(token, f, ForeignKeys::Off).await
    }

    async fn run<F, T>(
        &self,
        token: CancellationToken,
        f: F,
        foreign_keys: ForeignKeys,
    ) -> Result<T, SQLite3PoolError>
    where
        F: FnOnce(&Transaction) -> Result<T, SQLite3PoolError> + Send + 'static,
        T: Send + 'static,
    {
        if token.is_cancelled() {
            return Err(SQLite3PoolError::Cancelled);
        }

        let started = std::time::Instant::now();
        let Some(conn) = self.acquire().await else {
            // Logged as well as counted: a metric says how often, a line says when and
            // what to do, and pool exhaustion used to produce neither from the pool
            // itself — the single most likely production failure left no journal trace
            // at all. `warn!` rather than `error!` because the request is retryable and
            // a burst under load is not yet a defect.
            warn!(
                pool_size = self.size,
                "Every SQLite pool connection is checked out; refusing the transaction. \
                 Sysadmin: if this is not a brief burst, raise [database].pool_size or \
                 find the request holding a connection — GET /status reports saturation."
            );
            if let Some(m) = &self.metrics {
                m.pool_acquire_failures.inc();
                m.transactions
                    .get_or_create(&OutcomeLabels {
                        outcome: "pool_empty",
                    })
                    .inc();
            }
            return Err(SQLite3PoolError::PoolEmpty);
        };

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

                if foreign_keys == ForeignKeys::Off {
                    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
                }

                let tx = conn.transaction()?;
                let val = f(&tx)?;
                tx.commit()?;

                info!("Transaction committed.");

                Ok(val)
            })();

            // Unconditional, and outside the closure above: a connection handed back
            // to the pool with enforcement still off would disable foreign keys for
            // every unrelated caller that later borrowed it, silently and for the
            // life of the process. It must be restored on the error path too.
            if foreign_keys == ForeignKeys::Off
                && let Err(err) = conn.execute_batch("PRAGMA foreign_keys = ON;")
            {
                error!(
                    error = ?err,
                    "Failed to restore PRAGMA foreign_keys on a migration connection; \
                     dropping it rather than returning an unenforced one to the pool."
                );
                return Err(SQLite3PoolError::Sql(err));
            }

            // `blocking_lock` is safe here: we are on a dedicated spawn_blocking thread,
            // not inside the async runtime. The lock is held only for the push.
            conns.blocking_lock().push(conn);

            res
        });

        let joined = handle.await.map_err(|join_err| {
            // The blocking task panicked (a bug in `f`) or was aborted. The connection
            // for a panicked task is dropped rather than returned — acceptable, since a
            // panicking transaction closure is a programmer error, not a runtime condition.
            // It is not free, though: the pool is now one connection smaller for the life
            // of the process, so the hint names the symptom that follows.
            error!(
                %join_err,
                "SQLite transaction task failed to join (closure panicked?). \
                 Sysadmin: this connection is gone for good — after [database].pool_size \
                 such panics every request fails with `pool empty`; restart, and treat the \
                 panic above as the bug to fix."
            );
            SQLite3PoolError::Panicked
        });

        if let Some(m) = &self.metrics {
            m.transaction_duration
                .observe(started.elapsed().as_secs_f64());
            let outcome = match &joined {
                Ok(Ok(_)) => "ok",
                Ok(Err(SQLite3PoolError::Cancelled)) => "cancelled",
                // A panic is its own outcome: bucketed under "cancelled" it read as a
                // client disconnect, which is the one thing a dashboard must not be told.
                Ok(Err(SQLite3PoolError::Panicked)) | Err(_) => "panic",
                Ok(Err(_)) => "error",
            };
            m.transactions
                .get_or_create(&OutcomeLabels { outcome })
                .inc();
        }

        joined?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Each connection to ":memory:" is an independent database, so the pool is
    // sized to 1 wherever shared state across transactions matters.
    fn pool(size: usize) -> SQLite3Pool {
        SQLite3Pool::new(Path::new(":memory:"), size, 16384, "NORMAL")
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

    /// A panic in the closure is a bug in this process, and it used to be reported as
    /// `Cancelled` — i.e. HTTP 499 "the client closed the connection", a diagnosis
    /// pointing at the caller, and one every call site deliberately declines to log.
    /// The two must stay distinguishable: the panic also costs a pool connection, so
    /// it is the one pool error nobody may confuse with routine traffic.
    #[tokio::test]
    async fn a_panicking_closure_is_not_reported_as_a_cancellation() {
        let p = pool(2);
        let res: Result<(), _> = p
            .transaction(CancellationToken::new(), |_tx| {
                panic!("deliberate: a bug inside a transaction closure")
            })
            .await;
        assert!(
            matches!(res, Err(SQLite3PoolError::Panicked)),
            "a closure panic must be its own error, got {res:?}"
        );
        // And it must not read as a client disconnect on the wire either.
        let api = crate::backend::error::ApiError::from(SQLite3PoolError::Panicked);
        assert_eq!(api.code(), "internal.error");
    }

    /// Pool exhaustion is transient: the same request succeeds once a connection frees.
    /// Collapsed into `internal.error` it told the client not to bother retrying and
    /// told the operator nothing about load — the two most likely readings both wrong.
    #[tokio::test]
    async fn an_exhausted_pool_is_a_retryable_503() {
        let api = crate::backend::error::ApiError::from(SQLite3PoolError::PoolEmpty);
        assert_eq!(api.code(), "database.busy");
        assert_eq!(api.status().as_u16(), 503);
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
        assert!(
            timed_out.is_err(),
            "the slow transaction future should have been dropped"
        );

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

    // The pool fast-fails rather than queueing: with every connection checked out, an
    // extra transaction gets PoolEmpty immediately (the API contract behind the 500),
    // and once a holder finishes, the pool serves again.
    #[tokio::test]
    async fn saturated_pool_fast_fails_then_recovers() {
        let p = Arc::new(SQLite3Pool::new(Path::new(":memory:"), 2, 16384, "NORMAL"));

        // Check out both connections and hold them for 300ms.
        let mut holders = Vec::new();
        for _ in 0..2 {
            let p = Arc::clone(&p);
            holders.push(tokio::spawn(async move {
                p.transaction(CancellationToken::new(), |_tx| {
                    std::thread::sleep(Duration::from_millis(300));
                    Ok(())
                })
                .await
            }));
        }
        // Let both blocking tasks actually acquire their connections.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            p.available().await,
            0,
            "both connections should be checked out"
        );

        // The N+1-th caller is rejected immediately, not queued.
        let res = p.transaction(CancellationToken::new(), |_tx| Ok(())).await;
        assert!(
            matches!(res, Err(SQLite3PoolError::PoolEmpty)),
            "a saturated pool must fast-fail with PoolEmpty, got {res:?}"
        );

        // Once the holders return their connections, service resumes.
        for h in holders {
            h.await.unwrap().unwrap();
        }
        let res = p.transaction(CancellationToken::new(), |_tx| Ok(1)).await;
        assert!(
            matches!(res, Ok(1)),
            "pool must recover after connections return: {res:?}"
        );
        assert_eq!(p.available().await, 2);
    }

    // A panicking closure is a programmer error: its connection is deliberately
    // dropped (not returned), the caller gets an error instead of a propagated panic,
    // and the pool keeps serving on the remaining connections.
    #[tokio::test]
    async fn closure_panic_costs_one_connection_but_pool_keeps_serving() {
        let p = pool(2);

        let res = p
            .transaction(
                CancellationToken::new(),
                |_tx| -> Result<(), SQLite3PoolError> { panic!("bug in the closure") },
            )
            .await;
        assert!(
            res.is_err(),
            "a panicked transaction must surface as an error"
        );

        // One connection was sacrificed with the panicked task...
        assert_eq!(
            p.available().await,
            1,
            "the panicked task's connection is dropped"
        );

        // ...but the pool still serves with the other one.
        let res = p.transaction(CancellationToken::new(), |_tx| Ok(5)).await;
        assert!(
            matches!(res, Ok(5)),
            "pool must keep serving after a closure panic: {res:?}"
        );
    }
}
