use crate::backend::http3::{EmbeddingModel, RouterState};
use crate::config::Cli;
use crate::db::qdrant::{QdrantStore, VectorStore};
use crate::db::sqlite3::SQLite3Pool;
use crate::embed::EmbedTuning;
use crate::models::bge_m3::{BGEm3HttpClient, BGEm3Model, BGEm3Tuning};
use crate::worker::retry::RetryTuning;
use clap::Parser;
use qdrant_client::Qdrant;
use std::error::Error;
use std::sync::Arc;
use tokenizers::Tokenizer;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

mod backend;
mod config;
mod db;
mod embed;
mod models;
mod slicing;
mod worker;

type BoxError = Box<dyn Error + Send + Sync>;

// Applied in order on startup: only migrations whose version exceeds the
// current `PRAGMA user_version` are run, inside one transaction. `pub(crate)`
// so test modules build a schema-identical `:memory:` pool from the same source.
pub(crate) const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("db/migrations/v0.1.0_schema.sql")),
    (2, include_str!("db/migrations/v0.2.0_status_machine.sql")),
    (
        3,
        include_str!("db/migrations/v0.3.0_validation_checks.sql"),
    ),
];

/// Applies every migration whose version exceeds the DB's `PRAGMA user_version`,
/// then stamps `user_version` to the highest applied version. Returns the resulting
/// schema version and whether anything was applied. Extracted from the startup
/// transaction so the versioning logic is unit-testable.
pub(crate) fn apply_pending_migrations(
    tx: &rusqlite::Transaction,
) -> Result<(i32, bool), db::sqlite3::SQLite3PoolError> {
    let current: i32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let pending: Vec<_> = MIGRATIONS.iter().filter(|(v, _)| *v > current).collect();
    for (_, sql) in &pending {
        tx.execute_batch(sql)?;
    }
    if let Some((max_v, _)) = pending.last() {
        tx.pragma_update(None, "user_version", max_v)?;
    }
    let applied = !pending.is_empty();
    Ok((pending.last().map_or(current, |(v, _)| *v), applied))
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(
            fmt::layer()
                .json()
                .with_file(true)
                .with_line_number(true)
                .with_current_span(true)
                .with_span_list(true)
                .flatten_event(true)
                .with_ansi(std::env::var("RUST_ENV") == Ok("dev".into()))
                .pretty(),
        )
        .init();

    let token = CancellationToken::new();

    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate()).unwrap();
    let sigterm_token = token.child_token();

    let provider = rustls::crypto::ring::default_provider();
    let _ = provider.install_default();

    let cli = Cli::parse();

    // Two-level config: TOML file (XDG-resolved) → CLI overrides → built-in defaults.
    // `resolve` logs where it looked, what it loaded, and every flag override; on a
    // fatal config / validation error it returns the already-formatted message and we
    // refuse to start.
    let (cfg, config_source) = match config::resolve(&cli) {
        Ok(v) => v,
        Err(e) => {
            // Log the (already-formatted, multi-line) message and exit non-zero
            // directly — returning `Err` would make the runtime *also* dump the
            // error via Debug, double-printing it with escaped newlines.
            error!(error = %e, "Invalid configuration; refusing to start.");
            std::process::exit(1);
        }
    };
    info!(source = %config_source, "Configuration resolved.");

    let db_pool = Arc::new(SQLite3Pool::new(
        cfg.database.path.as_path(),
        cfg.database.pool_size,
        cfg.database.page_size_bytes,
        &cfg.sqlite_synchronous(),
    ));

    let db_schema_version = match db_pool.transaction(token, apply_pending_migrations).await {
        Ok((v, true)) => {
            info!(db_path = ?cfg.database.path, schema_version = v, "Schema migration completed.");
            v
        }
        Ok((v, false)) => {
            info!(db_path = ?cfg.database.path, schema_version = v, "Schema already up to date; no migrations run.");
            v
        }
        Err(err) => {
            error!(
                error = ?err,
                db_path = ?cfg.database.path,
                "Schema migration failed; cannot start. \
                 Check the DB file is writable and not from an incompatible older schema \
                 (no upgrade path is maintained — drop and recreate if so)."
            );
            return Err(err.into());
        }
    };

    let model_id = cfg.model.name.as_str(); // For now, only one model is supported.

    // Embed/upsert tuning shared by the indexing handler and the retry worker.
    let embed_tuning = EmbedTuning {
        embed_batch: cfg.indexing.embed_batch_chunks,
        upsert_batch: cfg.qdrant.upsert_batch_points,
        sparse_min_weight: cfg.indexing.sparse_min_weight,
    };

    // Surface files that have exhausted their retries — the retry worker stops
    // touching them, so without this they are silently stuck in 'failed'.
    worker::retry::warn_permanently_failed(
        &db_pool,
        cfg.workers.max_retries,
        sigterm_token.child_token(),
    )
    .await;

    // Surface files left mid-indexing by a previous run (crash / unclean shutdown).
    // They are not force-failed — the retry worker re-embeds them back to 'indexed'.
    worker::retry::warn_orphaned_indexing(&db_pool, sigterm_token.child_token()).await;

    // The per-file indexing claim table, shared by the HTTP handlers (in `RouterState`)
    // and the retry worker so a file held by a live `/index` is never raced by a sweep.
    let indexing_locks = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    // Process-wide GC flag, shared by the GC worker and the `POST /gc` handler so a
    // manual sweep and the hourly tick never run concurrently (GC is global).
    let gc_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let qdrant_client: Arc<dyn VectorStore> = Arc::new(QdrantStore::new(
        Qdrant::from_url(cfg.qdrant.server_url.as_str()).build()?,
        cfg.qdrant.dense_prefetch_limit,
        cfg.qdrant.sparse_prefetch_limit,
        cfg.qdrant.fusion_limit,
    ));

    // One embedding client, shared (as a trait object) by the retry worker and the
    // HTTP handlers — built once rather than per consumer.
    let embed_client: Arc<dyn BGEm3Model> = Arc::new(BGEm3HttpClient::new(
        cfg.model.server_url.clone(),
        BGEm3Tuning {
            max_429_retries: cfg.model.max_429_retries,
            backoff_base_ms: cfg.model.backoff_base_ms,
            health_timeout_ms: cfg.model.health_timeout_ms,
            encode_timeout_ms: cfg.model.encode_timeout_ms,
        },
    ));

    let gc_token = sigterm_token.child_token();
    let retry_token = sigterm_token.child_token();

    tokio::spawn(worker::gc::run(
        db_pool.clone(),
        qdrant_client.clone(),
        gc_flag.clone(),
        cfg.workers.gc_interval_seconds,
        cfg.workers.status_log_retention_days,
        gc_token,
    ));

    tokio::spawn(worker::retry::run(
        db_pool.clone(),
        qdrant_client.clone(),
        embed_client.clone(),
        model_id.to_string(),
        RetryTuning {
            embed: embed_tuning,
            retry_interval_seconds: cfg.workers.retry_interval_seconds,
            failed_warn_interval_seconds: cfg.workers.failed_warn_interval_seconds,
            max_retries: cfg.workers.max_retries,
            stuck_grace_secs: cfg.indexing.stuck_grace_minutes * 60,
        },
        indexing_locks.clone(),
        retry_token,
    ));

    // Whichever arm fires first wins and we proceed to shutdown — there is no
    // looping (a server exit, SIGINT, or SIGTERM all end the process).
    tokio::select! {
        res = backend::http3::run(
            cfg.server.bind,
            (cfg.server.cert_path.as_path(), cfg.server.key_path.as_path()),
            RouterState {
                tokenizer: Arc::new(Tokenizer::from_pretrained(model_id, None)?),
                db_pool: db_pool.clone(),
                qdrant: qdrant_client.clone(),
                model: EmbeddingModel::BGEm3 {
                    model_id: model_id.to_string(),
                    client: embed_client.clone(),
                },
                embed_tuning,
                min_chunk_tokens: cfg.slicer.min_chunk_tokens,
                max_chunk_tokens: cfg.slicer.max_chunk_tokens,
                default_top_k: cfg.search.default_top_k,
                max_top_k: cfg.search.max_top_k,
                max_query_bytes: cfg.search.max_query_bytes,
                max_code_bytes: cfg.limits.max_code_bytes,
                max_files_per_request: cfg.limits.max_files_per_request,
                max_drift_files: cfg.limits.max_drift_files,
                max_selector_patterns: cfg.limits.max_selector_patterns,
                path_batch_size: cfg.indexing.path_batch_size,
                status_log_retention_days: cfg.workers.status_log_retention_days,
                max_retries: cfg.workers.max_retries,
                indexing_locks: indexing_locks.clone(),
                gc_flag: gc_flag.clone(),
                stuck_grace_mins: cfg.indexing.stuck_grace_minutes,
                db_pool_size: cfg.database.pool_size,
                db_schema_version,
            },
            cfg.server.max_body_mib * 1024 * 1024,
            cfg.server.http3,
            sigterm_token.child_token()) => {
            if let Err(err) = res {
                error!(
                    error = ?err,
                    bind = %cfg.server.bind,
                    "HTTP server exited with an error. \
                     Check the bind address is free and the TLS cert/key paths are valid."
                );
            }
        }
        _ = signal::ctrl_c() => {
            info!("Received SIGINT. Shutting down...");
            sigterm_token.cancel();
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM. Shutting down...");
            sigterm_token.cancel();
        }
    }

    info!("Shutdown complete.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite3::SQLite3PoolError;
    use std::path::Path;

    fn pool() -> SQLite3Pool {
        SQLite3Pool::new(Path::new(":memory:"), 1, 16384, "NORMAL")
    }

    async fn user_version(pool: &SQLite3Pool) -> i32 {
        pool.transaction(CancellationToken::new(), |tx| {
            tx.pragma_query_value(None, "user_version", |r| r.get(0))
                .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    async fn object_exists(pool: &SQLite3Pool, kind: &'static str, name: &'static str) -> bool {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                rusqlite::params![kind, name],
                |r| r.get::<_, i64>(0),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
            > 0
    }

    #[tokio::test]
    async fn fresh_db_applies_all_migrations_and_stamps_user_version() {
        let p = pool();
        let (v, applied) = p
            .transaction(CancellationToken::new(), apply_pending_migrations)
            .await
            .unwrap();

        let (max_v, _) = MIGRATIONS.last().unwrap();
        assert_eq!((v, applied), (*max_v, true));
        assert_eq!(user_version(&p).await, *max_v);
        // One representative object per migration proves each batch actually ran.
        assert!(
            object_exists(&p, "table", "project_files").await,
            "v0.1.0 schema missing"
        );
        assert!(
            object_exists(&p, "trigger", "project_files_status_update_guard").await,
            "v0.2.0 status machine missing"
        );
        assert!(
            object_exists(&p, "trigger", "project_files_sha256_insert_guard").await,
            "v0.3.0 validation checks missing"
        );
    }

    #[tokio::test]
    async fn second_run_is_a_noop() {
        let p = pool();
        p.transaction(CancellationToken::new(), apply_pending_migrations)
            .await
            .unwrap();
        let (v, applied) = p
            .transaction(CancellationToken::new(), apply_pending_migrations)
            .await
            .unwrap();

        let (max_v, _) = MIGRATIONS.last().unwrap();
        assert_eq!(
            (v, applied),
            (*max_v, false),
            "an up-to-date DB must apply nothing"
        );
    }

    #[tokio::test]
    async fn partially_migrated_db_applies_only_newer_migrations() {
        // Simulate a DB created before v0.2.0/v0.3.0 existed: schema only, version 1.
        let p = pool();
        p.transaction(CancellationToken::new(), |tx| {
            tx.execute_batch(MIGRATIONS[0].1)?;
            tx.pragma_update(None, "user_version", 1)?;
            Ok(())
        })
        .await
        .unwrap();

        let (v, applied) = p
            .transaction(CancellationToken::new(), apply_pending_migrations)
            .await
            .unwrap();

        let (max_v, _) = MIGRATIONS.last().unwrap();
        assert_eq!((v, applied), (*max_v, true));
        assert!(object_exists(&p, "trigger", "project_files_status_update_guard").await);
        assert!(object_exists(&p, "trigger", "project_files_sha256_insert_guard").await);
    }

    #[tokio::test]
    async fn db_already_at_max_version_is_trusted_and_untouched() {
        // The filter trusts user_version: a DB stamped at the max version gets no
        // migrations even if (hypothetically) its schema is empty.
        let p = pool();
        let (max_v, _) = *MIGRATIONS.last().unwrap();
        p.transaction(CancellationToken::new(), move |tx| {
            tx.pragma_update(None, "user_version", max_v)?;
            Ok(())
        })
        .await
        .unwrap();

        let (v, applied) = p
            .transaction(CancellationToken::new(), apply_pending_migrations)
            .await
            .unwrap();
        assert_eq!((v, applied), (max_v, false));
        assert!(
            !object_exists(&p, "table", "project_files").await,
            "nothing must be applied"
        );
    }

    #[tokio::test]
    async fn every_migration_sql_is_idempotent() {
        // The cold-start guarantee: re-running any batch on a DB that already has it
        // must be a no-op (all SQL uses IF NOT EXISTS), never an error.
        let p = pool();
        p.transaction(CancellationToken::new(), |tx| {
            for (_, sql) in MIGRATIONS {
                tx.execute_batch(sql)?;
            }
            for (v, sql) in MIGRATIONS {
                tx.execute_batch(sql).unwrap_or_else(|e| {
                    panic!("migration {v} is not idempotent (re-run failed): {e}")
                });
            }
            Ok(())
        })
        .await
        .unwrap();
    }
}
