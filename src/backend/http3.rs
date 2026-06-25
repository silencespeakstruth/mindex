use axum::Router;
use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::middleware::Next;
use axum::routing::{get, post};
use axum_server::tls_rustls::RustlsConfig;
use axum::http::{Request, Response};
use bytes::{Buf, Bytes};
use h3::server::RequestStream;
use quinn::Endpoint;
use rustls::ServerConfig as TlsConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use tokenizers::Tokenizer;
use tokio_util::future::FutureExt;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;
use tracing::{info, warn};
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
    /// Per-file indexing mutual-exclusion table.
    pub indexing_locks: Arc<Mutex<HashSet<String>>>,
    /// Process-wide GC flag.
    pub gc_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Minutes a file may sit in `indexing` before the retry worker treats it as
    /// crash-orphaned. Reported by `GET /config`.
    pub stuck_grace_mins: i64,
    /// Connection-pool size. Reported by `GET /config`.
    pub db_pool_size: usize,
}

pub struct CancellationGuard(pub CancellationToken);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

// ─── TLS helpers ─────────────────────────────────────────────────────────────

/// Load cert + key PEMs into a `rustls::ServerConfig` with ALPN = `["h3"]`.
/// Used only for the QUIC endpoint; the TCP path uses its own `RustlsConfig`.
fn load_quic_tls(
    cert: &Path,
    key: &Path,
) -> Result<Arc<TlsConfig>, Box<dyn std::error::Error>> {
    let cert_pem = std::fs::read(cert)?;
    let key_pem = std::fs::read(key)?;

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_slice())
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|c| c.into_owned())
            .collect();

    let private_key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut key_pem.as_slice())?
            .ok_or_else(|| std::io::Error::other("no private key found in TLS key file"))?
            .clone_key();

    let mut config = TlsConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, private_key)?;
    // QUIC mandates ALPN; quinn enforces TLS 1.3 independently.
    config.alpn_protocols = vec![b"h3".to_vec()];

    Ok(Arc::new(config))
}

fn build_quic_endpoint(
    addr: SocketAddr,
    tls: Arc<TlsConfig>,
) -> Result<Endpoint, Box<dyn std::error::Error>> {
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let server_cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    Ok(Endpoint::server(server_cfg, addr)?)
}

// ─── Router ──────────────────────────────────────────────────────────────────

fn build_router(state: RouterState, body_limit_bytes: usize, h3_port: u16) -> Router {
    let alt_svc = format!("h3=\":{h3_port}\"; ma=86400");

    Router::new()
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
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", api_doc()))
        .layer(DefaultBodyLimit::max(body_limit_bytes))
        // Advertise HTTP/3 availability on every TCP/TLS response.
        .layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let alt_svc = alt_svc.clone();
                async move {
                    let mut resp = next.run(req).await;
                    if let Ok(v) = axum::http::HeaderValue::from_str(&alt_svc) {
                        resp.headers_mut().insert("alt-svc", v);
                    }
                    resp
                }
            },
        ))
        .with_state(state)
}

// ─── Entry point ─────────────────────────────────────────────────────────────

pub async fn run(
    addr: SocketAddr,
    pem_files: (&Path, &Path),
    state: RouterState,
    body_limit_bytes: usize,
    http3: bool,
    token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let router = build_router(state, body_limit_bytes, addr.port());

    // HTTP/3 over QUIC (UDP) — same port as TCP, no socket conflict.
    let quic_handle = if http3 {
        let quic_tls = load_quic_tls(pem_files.0, pem_files.1)?;
        let quic = build_quic_endpoint(addr, quic_tls)?;
        let h3_cancel = token.child_token();
        let h3 = tokio::spawn(serve_http3(
            quic.clone(),
            router.clone(),
            body_limit_bytes,
            h3_cancel.clone(),
        ));
        info!(?addr, "HTTP/3 QUIC endpoint listening.");
        Some((quic, h3_cancel, h3))
    } else {
        None
    };

    info!(
        ?addr,
        body_limit_bytes,
        http3,
        "HTTP server ready (HTTP/1.1+2 over TCP{}). Swagger UI at /swagger-ui.",
        if http3 { ", HTTP/3 over QUIC" } else { "" },
    );

    // HTTP/1.1+2 over TLS+TCP — runs until the cancellation token fires.
    axum_server::bind_rustls(
        addr,
        RustlsConfig::from_pem_file(pem_files.0, pem_files.1).await?,
    )
    .serve(router.into_make_service())
    .with_cancellation_token(&token)
    .await;

    if let Some((quic, h3_cancel, h3_handle)) = quic_handle {
        h3_cancel.cancel();
        quic.close(0u32.into(), b"server shutdown");
        let _ = h3_handle.await;
    }

    Ok(())
}

// ─── HTTP/3 acceptor ─────────────────────────────────────────────────────────

async fn serve_http3(
    endpoint: Endpoint,
    router: Router,
    body_limit_bytes: usize,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let router = router.clone();
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            if let Err(e) =
                                serve_h3_connection(conn, router, body_limit_bytes, cancel).await
                            {
                                warn!(error = %e, "HTTP/3 connection error");
                            }
                        }
                        Err(e) => warn!(error = %e, "QUIC handshake failed"),
                    }
                });
            }
        }
    }
}

async fn serve_h3_connection(
    conn: quinn::Connection,
    router: Router,
    body_limit_bytes: usize,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut h3_conn = h3::server::builder()
        .build::<_, Bytes>(h3_quinn::Connection::new(conn))
        .await?;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            accepted = h3_conn.accept() => {
                match accepted {
                    Ok(None) => break,
                    Err(e) => { warn!(error = %e, "HTTP/3 connection accept error"); break; }
                    Ok(Some(resolver)) => {
                        let router = router.clone();
                        let cancel = cancel.clone();
                        tokio::spawn(async move {
                            match resolver.resolve_request().await {
                                Ok((req, stream)) => {
                                    if let Err(e) = serve_h3_request(
                                        req, stream, router, body_limit_bytes, cancel,
                                    )
                                    .await
                                    {
                                        warn!(error = %e, "HTTP/3 request error");
                                    }
                                }
                                Err(e) => warn!(error = %e, "HTTP/3 request resolve error"),
                            }
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

// ─── HTTP/3 request handler ──────────────────────────────────────────────────

async fn serve_h3_request(
    req: Request<()>,
    mut stream: RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    router: Router,
    body_limit_bytes: usize,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Collect the request body, honouring the same size cap as the TCP path.
    let mut body: Vec<u8> = Vec::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            chunk = stream.recv_data() => {
                match chunk? {
                    None => break,
                    Some(mut data) => {
                        // Drain the Buf (h3 returns an opaque impl Buf).
                        while data.has_remaining() {
                            let chunk = data.chunk();
                            body.extend_from_slice(chunk);
                            let n = chunk.len();
                            data.advance(n);
                        }
                        if body.len() > body_limit_bytes {
                            let _ = stream
                                .send_response(Response::builder().status(413).body(()).unwrap())
                                .await;
                            let _ = stream.finish().await;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    // Forward to the axum router — identical logic to the TCP path.
    let (parts, _) = req.into_parts();
    let resp = router
        .oneshot(Request::from_parts(parts, Body::from(body)))
        .await?;

    let (resp_parts, resp_body) = resp.into_parts();
    stream
        .send_response(Response::from_parts(resp_parts, ()))
        .await?;

    let data = axum::body::to_bytes(resp_body, usize::MAX).await?;
    if !data.is_empty() {
        stream.send_data(data).await?;
    }
    stream.finish().await?;

    Ok(())
}
