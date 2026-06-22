use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum_server::tls_rustls::RustlsConfig;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use tokenizers::Tokenizer;
use tokio_util::future::FutureExt;
use tokio_util::sync::CancellationToken;
use tracing::info;
use utoipa_swagger_ui::SwaggerUi;

use crate::backend::openapi::api_doc;
use crate::backend::v0::handlers::{
    delete_files, delete_project, get_config, get_files, get_health, get_project_stats,
    get_projects, get_status, get_version, post_cancel, post_drift, post_gc, post_index,
    post_retry, post_search,
};
use crate::db::qdrant::VectorStore;
use crate::db::sqlite3::SQLite3Pool;
use crate::embed::EmbedTuning;
use crate::models::bge_m3::BGEm3Model;

#[derive(Clone)]
pub enum EmbeddingModel {
    BGEm3 {
        model_id: String,
        client: Arc<dyn BGEm3Model>,
    },
}

#[derive(Clone)]
pub struct RouterState {
    pub tokenizer: Arc<Tokenizer>,
    pub db_pool: Arc<SQLite3Pool>,
    pub qdrant: Arc<dyn VectorStore>,
    pub model: EmbeddingModel,
    /// Embed/upsert batch sizing + sparse threshold (`[indexing]`/`[qdrant]` config).
    pub embed_tuning: EmbedTuning,
    /// Slicer token window (`[slicer]` config).
    pub min_chunk_tokens: usize,
    pub max_chunk_tokens: usize,
    /// `top_k` used when a `/search` request omits it (`[search]` config).
    pub default_top_k: u64,
    /// Upper bound a `/search` request may set for `top_k` (`[search]` config).
    pub max_top_k: u64,
    /// Maximum search-query length in bytes (`[search]` config).
    pub max_query_bytes: usize,
    /// Per-file source size cap for `/index` (`[limits]` config).
    pub max_code_bytes: usize,
    /// File-count cap for one `/index` request (`[limits]` config).
    pub max_files_per_request: usize,
    /// Entry cap for one `/drift` `path → sha256` map (`[limits]` config).
    pub max_drift_files: usize,
    /// Globs + languages cap for one selector (`[limits]` config).
    pub max_selector_patterns: usize,
    /// Paths per batch on soft-delete / cancel (`[indexing]` config).
    pub path_batch_size: usize,
    /// Status-log retention for the synchronous `POST /gc` pass (`[workers]` config).
    pub status_log_retention_days: u64,
    /// `failed` retry budget, reported by `GET /config` (`[workers]` config).
    pub max_retries: i64,
    /// Per-file indexing mutual-exclusion table: the set of
    /// `(project, model, path)` keys currently being indexed. Serializes
    /// concurrent same-file `/index` requests (see `IndexClaim` in handlers).
    pub indexing_locks: Arc<Mutex<HashSet<String>>>,
    /// Process-wide GC flag: `true` while a GC pass is running. GC is global, so
    /// a single bool serializes `POST /gc` against itself and the hourly worker
    /// (see `GcGuard` in `worker::gc`). A concurrent `POST /gc` gets 409.
    pub gc_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Minutes a file may sit in `indexing` before the retry worker treats it as
    /// crash-orphaned (the `--stuck-grace-mins` CLI flag). Reported by `GET /config`.
    pub stuck_grace_mins: i64,
    /// Connection-pool size (the `--db-pool-size` CLI flag). Reported by `GET /config`
    /// (and paired with the live `available()` count in `GET /status`).
    pub db_pool_size: usize,
}

pub struct CancellationGuard(pub CancellationToken);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub async fn run(
    addr: SocketAddr,
    pem_files: (&Path, &Path),
    state: RouterState,
    body_limit_bytes: usize,
    token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    // Indexing posts many files at once, so the body easily exceeds axum's 2 MB
    // default; lift the limit (configurable via --max-body-mib / [server].max_body_mib).
    let router = Router::new()
        .route("/v0/{project_guid}/index", post(post_index))
        .route("/v0/{project_guid}/search", post(post_search))
        .route("/projects", get(get_projects))
        .route(
            "/projects/{project_guid}",
            get(get_project_stats).delete(delete_project),
        )
        .route(
            "/projects/{project_guid}/files",
            get(get_files).delete(delete_files),
        )
        .route("/projects/{project_guid}/cancel", post(post_cancel))
        .route("/projects/{project_guid}/retry", post(post_retry))
        .route("/projects/{project_guid}/drift", post(post_drift))
        .route("/gc", post(post_gc))
        .route("/status", get(get_status))
        .route("/config", get(get_config))
        .route("/health", get(get_health))
        .route("/version", get(get_version))
        // Interactive API docs at /swagger-ui, raw spec at /api-docs/openapi.json.
        // The UI assets are vendored into the binary (no network fetch at runtime),
        // and the route carries no state so it merges cleanly before `.with_state`.
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", api_doc()))
        .layer(DefaultBodyLimit::max(body_limit_bytes))
        .with_state(state);

    info!(?addr, body_limit_bytes, "The HTTP server is ready. Swagger UI at /swagger-ui.");

    let (cert, key) = pem_files;
    axum_server::bind_rustls(addr, RustlsConfig::from_pem_file(cert, key).await?)
        .serve(router.into_make_service())
        .with_cancellation_token(&token)
        .await;

    Ok(())
}
