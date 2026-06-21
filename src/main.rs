use crate::backend::http3::{EmbeddingModel, RouterState};
use crate::db::qdrant::VectorStore;
use crate::db::sqlite3::SQLite3Pool;
use crate::models::bge_m3::{BGEm3HttpClient, BGEm3Model};
use clap::Parser;
use qdrant_client::Qdrant;
use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokenizers::Tokenizer;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

mod backend;
mod db;
mod embed;
mod models;
mod slicing;
mod worker;

type BoxError = Box<dyn Error + Send + Sync>;

// Applied in order on startup, inside one transaction. `pub(crate)` so test
// modules build a schema-identical `:memory:` pool from the same source.
pub(crate) const MIGRATIONS: &[&str] = &[
    include_str!("db/migrations/v0.1.0_schema.sql"),
    include_str!("db/migrations/v0.2.0_status_machine.sql"),
];

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = concat!(
        "mindex is a high-performance semantic search engine built in Rust. ",
        "It leverages the BGE-M3 model for hybrid (dense/sparse) retrieval ",
        "combined with advanced reranking techniques to deliver accurate, ",
        "context-aware search results."
    )
)]
struct Args {
    /// Interface to bind the server (e.g., 127.0.0.1:8080).
    #[arg(short, long, default_value = "127.0.0.1:11111")]
    bind: SocketAddr,

    /// Path to the TLS certificate file (required for https2/3).
    #[arg(long, default_value = "cert.pem")]
    cert_path: PathBuf,

    /// Path to the TLS private key file (required for https2/3).
    #[arg(long, default_value = "key.pem")]
    key_path: PathBuf,

    /// Name of the model to use.
    #[arg(long, default_value = "BAAI/bge-m3")]
    model: String,

    /// Model API server (e.g., https://some.domain:443).
    #[arg(long, default_value = "http://localhost:11211")]
    model_server: Url,

    /// Qdrant server (e.g., https://some.domain:443).
    #[arg(long, default_value = "http://localhost:6334")]
    qdrant_server: Url,

    /// Path to the SQLite database file (use :memory: for in-memory mode).
    #[arg(long, default_value = "mindex.db")]
    db_path: PathBuf,

    /// DB pool size.
    #[arg(long, default_value = "4")]
    db_pool_size: usize,

    /// Chunks sent to the model server per /encode call during indexing. The GPU
    /// batch lever — raise it (and the embedder's --batch) to load the GPU harder.
    #[arg(long, default_value = "256")]
    embed_batch: usize,

    /// Max /index request body size in MiB (indexing posts many files at once;
    /// axum's default is only 2 MB).
    #[arg(long, default_value = "256")]
    max_body_mb: usize,

    /// Minutes a file may sit in 'indexing' before the retry worker treats it as
    /// crash-orphaned. Must exceed the longest legitimate in-flight request, or the
    /// worker races live batches. Raise it for very large --batch-size.
    #[arg(long, default_value = "30")]
    stuck_grace_mins: i64,
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

    let args = Args::parse();

    let db_pool = Arc::new(SQLite3Pool::new(
        args.db_path.as_path(),
        args.db_pool_size,
    ));

    match db_pool
        .transaction(token, |tx| {
            for migration in MIGRATIONS {
                tx.execute_batch(migration)?;
            }

            Ok(())
        })
        .await
    {
        Ok(_) => info!(db_path = ?args.db_path, "Schema migration completed."),
        Err(err) => {
            error!(
                error = ?err,
                db_path = ?args.db_path,
                "Schema migration failed; cannot start. \
                 Check the DB file is writable and not from an incompatible older schema \
                 (no upgrade path is maintained — drop and recreate if so)."
            );
            return Err(err.into());
        }
    }

    let model_id = args.model.as_str(); // For now, only one model is supported.

    // Surface files that have exhausted their retries — the retry worker stops
    // touching them, so without this they are silently stuck in 'failed'.
    worker::retry::warn_permanently_failed(&db_pool, sigterm_token.child_token()).await;

    // Surface files left mid-indexing by a previous run (crash / unclean shutdown).
    // They are not force-failed — the retry worker re-embeds them back to 'indexed'.
    worker::retry::warn_orphaned_indexing(&db_pool, sigterm_token.child_token()).await;

    // The per-file indexing claim table, shared by the HTTP handlers (in `RouterState`)
    // and the retry worker so a file held by a live `/index` is never raced by a sweep.
    let indexing_locks = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    // Process-wide GC flag, shared by the GC worker and the `POST /gc` handler so a
    // manual sweep and the hourly tick never run concurrently (GC is global).
    let gc_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let qdrant_client: Arc<dyn VectorStore> =
        Arc::new(Qdrant::from_url(args.qdrant_server.as_str()).build()?);

    // One embedding client, shared (as a trait object) by the retry worker and the
    // HTTP handlers — built once rather than per consumer.
    let embed_client: Arc<dyn BGEm3Model> =
        Arc::new(BGEm3HttpClient::new(args.model_server.clone()));

    let gc_token = sigterm_token.child_token();
    let retry_token = sigterm_token.child_token();

    tokio::spawn(worker::gc::run(
        db_pool.clone(),
        qdrant_client.clone(),
        gc_flag.clone(),
        gc_token,
    ));

    tokio::spawn(worker::retry::run(
        db_pool.clone(),
        qdrant_client.clone(),
        embed_client.clone(),
        model_id.to_string(),
        args.embed_batch,
        args.stuck_grace_mins * 60,
        indexing_locks.clone(),
        retry_token,
    ));

    // Whichever arm fires first wins and we proceed to shutdown — there is no
    // looping (a server exit, SIGINT, or SIGTERM all end the process).
    tokio::select! {
        res = backend::http3::run(
            args.bind,
            (args.cert_path.as_path(), args.key_path.as_path()),
            RouterState {
                tokenizer: Arc::new(Tokenizer::from_pretrained(model_id, None)?),
                db_pool: db_pool.clone(),
                qdrant: qdrant_client.clone(),
                model: EmbeddingModel::BGEm3 {
                    model_id: model_id.to_string(),
                    client: embed_client.clone(),
                },
                embed_batch: args.embed_batch,
                indexing_locks: indexing_locks.clone(),
                gc_flag: gc_flag.clone(),
            },
            args.max_body_mb * 1024 * 1024,
            sigterm_token.child_token()) => {
            if let Err(err) = res {
                error!(
                    error = ?err,
                    bind = %args.bind,
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
