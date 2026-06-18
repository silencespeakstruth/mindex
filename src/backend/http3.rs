use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
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

use crate::backend::v0::handlers::{
    delete_files, delete_project, get_health, get_project_stats, get_projects, get_version, post_drift,
    post_gc, post_index, post_search,
};
use crate::db::qdrant::VectorStore;
use crate::db::sqlite3::SQLite3Pool;
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
    /// Chunks per `/encode` call during indexing (GPU batch lever).
    pub embed_batch: usize,
    /// Per-file indexing mutual-exclusion table: the set of
    /// `(project, model, path)` keys currently being indexed. Serializes
    /// concurrent same-file `/index` requests (see `IndexClaim` in handlers).
    pub indexing_locks: Arc<Mutex<HashSet<String>>>,
}

pub struct CancellationGuard(pub CancellationToken);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub fn cancelled_499() -> StatusCode {
    StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST)
}

pub async fn run(
    addr: SocketAddr,
    pem_files: (&Path, &Path),
    state: RouterState,
    body_limit_bytes: usize,
    token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    // Indexing posts many files at once, so the body easily exceeds axum's 2 MB
    // default; lift the limit (configurable via --max-body-mb).
    let router = Router::new()
        .route("/v0/{project_guid}/index", post(post_index))
        .route("/v0/{project_guid}/search", post(post_search))
        .route("/projects", get(get_projects))
        .route(
            "/projects/{project_guid}",
            get(get_project_stats).delete(delete_project),
        )
        .route("/projects/{project_guid}/files", delete(delete_files))
        .route("/projects/{project_guid}/drift", post(post_drift))
        .route("/gc", post(post_gc))
        .route("/health", get(get_health))
        .route("/version", get(get_version))
        .layer(DefaultBodyLimit::max(body_limit_bytes))
        .with_state(state);

    info!(?addr, body_limit_bytes, "The HTTP server is ready.");

    let (cert, key) = pem_files;
    axum_server::bind_rustls(addr, RustlsConfig::from_pem_file(cert, key).await?)
        .serve(router.into_make_service())
        .with_cancellation_token(&token)
        .await;

    Ok(())
}
