use axum::Router;
use axum::http::StatusCode;
use axum::routing::post;
use axum_server::tls_rustls::RustlsConfig;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokenizers::Tokenizer;
use tokio_util::future::FutureExt;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::backend::v0::handlers::{post_index, post_search};
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
    token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let router = Router::new()
        .route("/v0/{project_guid}/index", post(post_index))
        .route("/v0/{project_guid}/search", post(post_search))
        .with_state(state);

    info!(?addr, "The HTTP server is ready.");

    let (cert, key) = pem_files;
    axum_server::bind_rustls(addr, RustlsConfig::from_pem_file(cert, key).await?)
        .serve(router.into_make_service())
        .with_cancellation_token(&token)
        .await;

    Ok(())
}
