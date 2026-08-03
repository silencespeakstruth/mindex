use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequestParts as _, MatchedPath, RawPathParams};
use axum::http::{Request, Response};
use axum::middleware::Next;
use axum::routing::{get, post};
use axum_server::tls_rustls::RustlsConfig;
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

use crate::backend::error::ErrorCode;
use crate::backend::metrics::{
    Metrics, ProtoLabels, RequestLabels, RouteLabels, RouteProjectLabels,
};
use crate::backend::openapi::api_doc;
use crate::backend::v0::handlers::{
    delete_files, delete_history, delete_project, delete_research_active, delete_research_run,
    delete_research_runs, get_config, get_files, get_health, get_llms_txt, get_metrics,
    get_project_stats, get_projects, get_research_active, get_research_run, get_research_runs,
    get_research_verification, get_status, get_version, post_cancel, post_drift, post_gc,
    post_history, post_index, post_research, post_research_challenge, post_research_pin,
    post_retry, post_search, post_symbols,
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
    /// Embedder for the **query** path only — `/search` and every `search` a
    /// research run makes. Usually the very same `Arc` as `model`'s client; a
    /// second one when `[model].query_server_url` splits the workloads, because
    /// indexing (batches of hundreds, throughput-bound) and querying (one short
    /// text, latency-bound) want opposite hardware. Kept beside `model` rather
    /// than inside it: the *model* is one model — this is which instance answers.
    pub query_model: Arc<dyn BGEm3Model>,
    /// Embed/upsert batch sizing + sparse threshold (`[indexing]`/`[qdrant]` config).
    pub embed_tuning: EmbedTuning,
    /// Slicer token window (`[slicer]` config).
    pub min_chunk_tokens: usize,
    pub max_chunk_tokens: usize,
    /// Index the lines the AST walk selects nothing for (`[slicer]` config).
    pub fill_gaps: bool,
    /// Cap on a documentation chunk (`[slicer]` config); prose has no minimum.
    pub max_doc_chunk_tokens: usize,
    /// Weight of the semantic-shift term when cutting documentation (`[slicer]`).
    pub doc_semantic_weight: f64,
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
    /// Byte cap for a `/symbols` `name` (`[limits]` config).
    pub max_symbol_name_bytes: usize,
    /// Upper bound a `/symbols` request may set for `limit` (`[limits]` config).
    pub max_symbol_results: usize,
    /// Commit-count cap for one `/history` request (`[limits]` config).
    pub max_history_commits: usize,
    /// Byte cap for one commit's subject + body (`[limits]` config).
    pub max_commit_message_bytes: usize,
    /// Run-id cap for one batch research delete (`[limits]` config).
    pub max_research_delete_ids: usize,
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
    /// Applied `PRAGMA user_version` after startup migrations. Reported by `GET /version`.
    pub db_schema_version: i32,
    /// Handle of the dedicated research runtime (small, `[research].worker_threads`).
    pub research_handle: tokio::runtime::Handle,
    /// Research admission: `[research].max_concurrent` permits; empty → 429.
    pub research_semaphore: Arc<tokio::sync::Semaphore>,
    /// `[research].max_concurrent` itself, published by `GET /config` and
    /// `GET /health`: without it a caller learns the limit only by being refused,
    /// which is what makes planning a queue guesswork.
    pub research_max_concurrent: usize,
    /// Which runs are live right now, and the tokens that stop them. The permit
    /// count alone says a slot is busy; this says *what* is holding it.
    pub research_registry: crate::backend::inflight::ResearchRegistry,
    /// What a run at each `(model, effort)` has actually cost lately, refreshed by
    /// `worker::research_stats` and published by `GET /config`. The ladder says what
    /// a level *grants*; this is the only thing that says what it *takes*.
    pub research_stats: crate::worker::research_stats::SharedRunStats,
    /// The Ollama chat client driving research loops.
    pub research_ollama: Arc<dyn crate::models::ollama::OllamaModel>,
    /// Model used when a research request names none ("" = none configured).
    pub research_default_model: String,
    /// Compiled `[research].allowed_models` globs; empty = any model. Checked
    /// against every resolved research model before a slot is taken, and used to
    /// filter the catalog `GET /config` publishes.
    pub research_allowed_models: crate::config::AllowedModels,
    /// What each `effort` level buys (`[research].effort.*`).
    pub research_effort: crate::config::EffortBudgets,
    /// Ceilings on a request's `budget` override (`[research].max_request_*`;
    /// `max_request_steps` also caps the `checkpoint_every_steps` override).
    pub research_max_request_seconds: u64,
    pub research_max_request_tokens: u64,
    pub research_max_request_steps: usize,
    pub research_max_request_report_sections: usize,
    pub research_max_request_report_words: usize,
    pub research_max_evidence_width: u64,
    /// How long the report phase gets after the investigation deadline
    /// (`[research].report_timeout_ms`). Not a budget axis and not
    /// request-overridable — an operator's bound on the tail of a run.
    pub research_report_timeout_ms: u64,
    pub research_checkpoint_every_steps: usize,
    /// Thinking characters after which one turn is abandoned
    /// (`[research].max_turn_thinking_chars`, `0` = off). Not published by
    /// `GET /config`: it changes nothing a caller renders, waits for or may set.
    pub research_max_turn_thinking_chars: usize,
    /// Abandon one turn once it has streamed this many seconds; `0` = off
    /// (`[research].max_turn_seconds`). The one per-turn guard that stops something.
    pub research_max_turn_seconds: u64,
    /// How long a finished run is kept before `/gc` reaps it
    /// (`[research].retention_days`). Stamped onto the row at insert as an absolute
    /// `expires_at`, so this governs new runs only.
    pub research_retention_days: u64,
    /// How many earlier runs one request may name in `context_run_ids`
    /// (`[research].max_context_runs`; `0` switches the feature off).
    pub research_max_context_runs: usize,
    /// Total characters of prior reports injected into one run
    /// (`[research].max_context_chars`). A real budget axis: the transcript is
    /// resent every turn, so this is paid per turn.
    pub research_max_context_chars: usize,
    /// Page-size ceiling for the stored-research list (`[research].list_page_limit`).
    pub research_list_page_limit: usize,
    /// Server-configured sampling (`[research].temperature`/`top_p`/`seed`). A
    /// request's `seed` is applied over it per run.
    pub research_sampling: crate::models::ollama::Sampling,
    /// The local Ollama model registry, refreshed on an interval by
    /// `worker::ollama_catalog` and published by `GET /config` so a client can offer
    /// a closed list instead of a free-text field. Behind a lock because it is the
    /// one thing in this struct that changes after startup.
    pub research_models: crate::worker::ollama_catalog::SharedCatalog,
    /// Everything this process measures about itself. Always present and always
    /// written into — `[metrics].enabled` gates the endpoint, not the recording.
    pub metrics: Arc<Metrics>,
}

impl RouterState {
    /// The effort level's preset budgets.
    pub fn research_effort_budget(
        &self,
        effort: crate::research::Effort,
    ) -> &crate::config::EffortBudget {
        match effort {
            crate::research::Effort::Low => &self.research_effort.low,
            crate::research::Effort::Medium => &self.research_effort.medium,
            crate::research::Effort::High => &self.research_effort.high,
        }
    }

    /// The budget for one request: the effort preset with the request's overrides
    /// applied (`Budget::resolve`, which is where the merge rules and their tests
    /// live).
    pub fn research_budget(
        &self,
        effort: crate::research::Effort,
        over: Option<crate::backend::v0::models::ResearchBudgetOverride>,
    ) -> crate::research::Budget {
        crate::research::Budget::resolve(self.research_effort_budget(effort), over)
    }

    /// Sampling for one request: the server's configured values with the request's
    /// `seed` applied over `[research].seed`. Only the seed is per-request —
    /// temperature and top_p are a quality decision the operator owns.
    pub fn research_sampling_for(&self, seed: Option<i64>) -> crate::models::ollama::Sampling {
        crate::models::ollama::Sampling {
            seed: seed.or(self.research_sampling.seed),
            ..self.research_sampling
        }
    }
}

pub struct CancellationGuard(pub CancellationToken);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

// ─── Request metrics ─────────────────────────────────────────────────────────

/// Decrements the in-flight gauge on drop, and records the request if the
/// response never came.
///
/// This has to be a guard rather than an `inc()`/`dec()` pair for the same
/// reason [`CancellationGuard`] exists: when a client disconnects, axum *drops*
/// the future — the code after `next.run(req).await` never executes. A research
/// SSE stream lives for minutes and dies **only** by disconnect, so a plain pair
/// would ratchet the gauge upward until restart. Recording the abandonment as a
/// 499 here is also what keeps `requests_total` reconcilable with `in_flight`.
struct InFlightGuard {
    metrics: Arc<Metrics>,
    route: RouteLabels,
    /// Set once the response has been observed and counted.
    completed: bool,
}

impl InFlightGuard {
    fn enter(metrics: Arc<Metrics>, route: RouteLabels) -> Self {
        metrics.http.in_flight.get_or_create(&route).inc();
        Self {
            metrics,
            route,
            completed: false,
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.metrics.http.in_flight.get_or_create(&self.route).dec();
        if !self.completed {
            // Abandoned before a response existed: the same 499 the handlers use
            // for a client-cancelled request.
            self.metrics
                .http
                .requests
                .get_or_create(&RequestLabels {
                    route: self.route.route.clone(),
                    method: self.route.method,
                    status: 499,
                    code: "request.cancelled",
                })
                .inc();
        }
    }
}

/// Methods, as `&'static str` so they never allocate and never grow the label
/// space past what the router actually routes.
fn method_label(m: &axum::http::Method) -> &'static str {
    match *m {
        axum::http::Method::GET => "GET",
        axum::http::Method::POST => "POST",
        axum::http::Method::PUT => "PUT",
        axum::http::Method::DELETE => "DELETE",
        axum::http::Method::PATCH => "PATCH",
        axum::http::Method::HEAD => "HEAD",
        axum::http::Method::OPTIONS => "OPTIONS",
        _ => "other",
    }
}

fn proto_label(v: axum::http::Version) -> &'static str {
    match v {
        axum::http::Version::HTTP_09 => "HTTP/0.9",
        axum::http::Version::HTTP_10 => "HTTP/1.0",
        axum::http::Version::HTTP_11 => "HTTP/1.1",
        axum::http::Version::HTTP_2 => "HTTP/2",
        axum::http::Version::HTTP_3 => "HTTP/3",
        _ => "other",
    }
}

/// The request's `project_guid` path parameter, **validated as a UUID**.
///
/// That validation is the only thing standing between a client and unbounded
/// cardinality: this is the one label whose value a caller chooses. A path
/// parameter that is not a UUID would never match a project anyway.
///
/// `RawPathParams` only *reads* the router's path-parameter extension, so
/// consuming it here leaves the handler's own `Path` extractor untouched.
async fn project_label(parts: &mut axum::http::request::Parts) -> Option<String> {
    let params = RawPathParams::from_request_parts(parts, &()).await.ok()?;
    params
        .iter()
        .find(|(k, _)| *k == "project_guid")
        .map(|(_, v)| v.to_owned())
        .filter(|v| uuid::Uuid::parse_str(v).is_ok())
}

/// Record one request. Applied as the **outermost** layer, so it observes the
/// final response — including the `alt-svc` header and any `DefaultBodyLimit`
/// rejection.
///
/// Two things it structurally cannot see, both accepted rather than faked:
/// a request that matched no route never reaches a `Router::layer` (there is no
/// fallback), so unknown-path 404s go uncounted; and the HTTP/3 body-limit
/// short-circuit answers before the router, so it records itself — see
/// `serve_h3_request`.
async fn record_request(
    metrics: Arc<Metrics>,
    per_project: bool,
    req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let (mut parts, body) = req.into_parts();

    // `MatchedPath` is the route *template*, so `/v0/{project_guid}/search` is one
    // series rather than one per project. Present because `Router::layer` runs
    // after routing. Swagger's merged routes show up here too — bounded, and not
    // worth a special case.
    let route = parts
        .extensions
        .get::<MatchedPath>()
        .map_or_else(|| "unknown".to_string(), |m| m.as_str().to_string());
    let method = method_label(&parts.method);
    let proto = proto_label(parts.version);
    let project = if per_project {
        project_label(&mut parts).await
    } else {
        None
    };

    let labels = RouteLabels {
        route: route.clone(),
        method,
    };
    let mut guard = InFlightGuard::enter(Arc::clone(&metrics), labels.clone());

    let started = std::time::Instant::now();
    let resp = next.run(Request::from_parts(parts, body)).await;
    let elapsed = started.elapsed().as_secs_f64();

    // `code()` is `&'static str` and rides in the response extensions rather than
    // in the body, so labelling costs neither a parse nor an allocation.
    let code = resp
        .extensions()
        .get::<ErrorCode>()
        .map_or("", |ErrorCode(c)| *c);

    metrics
        .http
        .requests
        .get_or_create(&RequestLabels {
            route,
            method,
            status: resp.status().as_u16(),
            code,
        })
        .inc();
    metrics
        .http
        .duration
        .get_or_create(&labels)
        .observe(elapsed);
    metrics
        .http
        .by_proto
        .get_or_create(&ProtoLabels { proto })
        .inc();
    if let Some(project_guid) = project {
        metrics
            .http
            .by_project
            .get_or_create(&RouteProjectLabels {
                route: labels.route.clone(),
                project_guid,
            })
            .inc();
    }
    guard.completed = true;

    resp
}

// ─── TLS helpers ─────────────────────────────────────────────────────────────

/// Load cert + key PEMs into a `rustls::ServerConfig` with ALPN = `["h3"]`.
/// Used only for the QUIC endpoint; the TCP path uses its own `RustlsConfig`.
fn load_quic_tls(cert: &Path, key: &Path) -> Result<Arc<TlsConfig>, Box<dyn std::error::Error>> {
    let cert_pem = std::fs::read(cert)?;
    let key_pem = std::fs::read(key)?;

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|c| c.into_owned())
        .collect();

    let private_key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_pem.as_slice())?
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

fn build_router(
    state: RouterState,
    body_limit_bytes: usize,
    h3_port: u16,
    metrics_cfg: MetricsRouting,
) -> Router {
    let alt_svc = format!("h3=\":{h3_port}\"; ma=86400");
    let metrics = Arc::clone(&state.metrics);
    let per_project = metrics_cfg.per_project_http_labels;

    let router = Router::new()
        .route("/v0/{project_guid}/index", post(post_index))
        .route("/v0/{project_guid}/search", post(post_search))
        .route("/v0/{project_guid}/symbols", post(post_symbols))
        .route("/v0/{project_guid}/research", post(post_research))
        // The opponent: a research run whose subject is a stored report. Same
        // loop, same semaphore — the data plane, like the run that produced it.
        .route(
            "/v0/{project_guid}/research/{run_id}/challenge",
            post(post_research_challenge),
        )
        .route(
            "/v0/{project_guid}/history",
            post(post_history).delete(delete_history),
        )
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
        // Browsing stored research is management, not the versioned data plane: it
        // reads server state the same way `/projects/{guid}/files` does. The run that
        // *produces* a report stays at `POST /v0/{guid}/research`.
        .route(
            "/projects/{project_guid}/research",
            get(get_research_runs).delete(delete_research_runs),
        )
        .route(
            "/projects/{project_guid}/research/{run_id}",
            get(get_research_run).delete(delete_research_run),
        )
        .route(
            "/projects/{project_guid}/research/{run_id}/pin",
            post(post_research_pin),
        )
        // The offline re-check: pure SQLite over the journalled spans and
        // baselines, so "does this report still hold" costs a read, not a rerun.
        .route(
            "/projects/{project_guid}/research/{run_id}/verification",
            get(get_research_verification),
        )
        // Live runs, unlike the stored ones above, are not per project: the
        // semaphore they contend for is global, and a caller planning a queue needs
        // to know the slots are gone rather than that none of its own runs hold
        // them. Kept off `/projects/{guid}/research` for a second reason too — that
        // list is keyset-paged by `seq`, which a live run does not have yet.
        .route("/research/active", get(get_research_active))
        .route(
            "/research/active/{run_id}",
            axum::routing::delete(delete_research_active),
        )
        .route("/gc", post(post_gc))
        .route("/status", get(get_status))
        .route("/config", get(get_config))
        // The AI-agent bootstrap document (llms.txt convention). Unconditional,
        // unlike `/metrics`: there is no config gate whose absence it should
        // mirror, and a 404 here would read as "this server has no such doc".
        .route("/llms.txt", get(get_llms_txt))
        .route("/health", get(get_health))
        .route("/version", get(get_version));

    // Prometheus exposition. Off by config = not routed at all, so a disabled
    // deployment 404s rather than serving an empty registry.
    let router = if metrics_cfg.enabled {
        router.route("/metrics", get(get_metrics))
    } else {
        router
    };

    router
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
        // Added last, so it is the OUTERMOST layer and sees the final response.
        // It must also be a `Router::layer` rather than a wrapper around the
        // server: only inside the router does `MatchedPath` exist, and without it
        // every series would carry an empty `route`.
        .layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let metrics = Arc::clone(&metrics);
                async move { record_request(metrics, per_project, req, next).await }
            },
        ))
        .with_state(state)
}

/// How the router should expose and label metrics (`[metrics]` config).
///
/// A struct rather than two bare `bool` arguments, so a call site cannot swap
/// them silently.
#[derive(Clone, Copy)]
pub struct MetricsRouting {
    /// Route `GET /metrics`.
    pub enabled: bool,
    /// Add `project_guid` to the HTTP request counter.
    pub per_project_http_labels: bool,
}

// ─── Entry point ─────────────────────────────────────────────────────────────

pub async fn run(
    addr: SocketAddr,
    pem_files: (&Path, &Path),
    state: RouterState,
    body_limit_bytes: usize,
    http3: bool,
    metrics_cfg: MetricsRouting,
    token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let metrics = Arc::clone(&state.metrics);
    let router = build_router(state, body_limit_bytes, addr.port(), metrics_cfg);

    // HTTP/3 over QUIC (UDP) — same port as TCP, no socket conflict.
    let quic_handle = if http3 {
        let quic_tls = load_quic_tls(pem_files.0, pem_files.1)?;
        let quic = build_quic_endpoint(addr, quic_tls)?;
        let h3_cancel = token.child_token();
        let h3 = tokio::spawn(serve_http3(
            quic.clone(),
            router.clone(),
            body_limit_bytes,
            Arc::clone(&metrics),
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
    let serve_result = axum_server::bind_rustls(
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

    // `None` = cancelled by the shutdown token (clean exit); `Some(Err)` = the TCP
    // server died on its own (bind conflict, TLS handshake config, ...) and must
    // surface — otherwise the process exits 0 having never served.
    if let Some(res) = serve_result {
        res?;
    }

    Ok(())
}

// ─── HTTP/3 acceptor ─────────────────────────────────────────────────────────

async fn serve_http3(
    endpoint: Endpoint,
    router: Router,
    body_limit_bytes: usize,
    metrics: Arc<Metrics>,
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
                let metrics = Arc::clone(&metrics);
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            if let Err(e) =
                                serve_h3_connection(conn, router, body_limit_bytes, metrics, cancel)
                                    .await
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
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut h3_conn = h3::server::builder()
        .build::<_, Bytes>(h3_quinn::Connection::new(conn))
        .await?;

    // Per-connection token: cancelled when this function returns (client closed the
    // connection, connection error, or server shutdown), so in-flight request tasks
    // drop their handler futures — firing each handler's `CancellationGuard` exactly
    // like axum does on the TCP path when the client disconnects.
    let conn_cancel = cancel.child_token();
    let _conn_guard = conn_cancel.clone().drop_guard();

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
                        let cancel = conn_cancel.clone();
                        let metrics = Arc::clone(&metrics);
                        tokio::spawn(async move {
                            match resolver.resolve_request().await {
                                Ok((req, stream)) => {
                                    if let Err(e) = serve_h3_request(
                                        req, stream, router, body_limit_bytes, metrics, cancel,
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
    metrics: Arc<Metrics>,
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
                            use axum::response::IntoResponse;
                            // The one request that answers without reaching the
                            // router, so the metrics layer can never see it — it
                            // records itself. A second such short-circuit needs
                            // the same three lines, or it goes uncounted in
                            // silence.
                            metrics
                                .http
                                .requests
                                .get_or_create(&RequestLabels {
                                    route: "<h3_body_limit>".to_string(),
                                    method: method_label(req.method()),
                                    status: 413,
                                    code: "request.body_too_large",
                                })
                                .inc();
                            return send_axum_response(
                                crate::backend::error::ApiError::BodyTooLarge.into_response(),
                                &mut stream,
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }

    // Forward to the axum router — identical logic to the TCP path. Dropping the
    // future on connection close is what fires the handler's `CancellationGuard`.
    let (parts, _) = req.into_parts();
    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok(()),
        resp = router.oneshot(Request::from_parts(parts, Body::from(body))) => resp?,
    };

    send_axum_response(resp, &mut stream).await
}

/// Send an axum `Response` back over an HTTP/3 bidi stream. Shared by the normal
/// routing path and the over-limit 413 so both emit the same problem+json envelope.
///
/// **Frame by frame, never buffered.** This used to be
/// `axum::body::to_bytes(body, usize::MAX)` before the first `send_data`, which made
/// the two SSE endpoints — `/index?stream=yes` and `/research` — not stream at all
/// over h3: an event was produced, held, and the client saw nothing until the run
/// finished, up to seventy minutes later for a `high` research run. The whole
/// event history was accumulated in memory meanwhile, with no bound. Both endpoints
/// exist *because* their output is worth watching as it arrives, so buffering them
/// does not degrade the feature, it removes it.
async fn send_axum_response(
    resp: Response<Body>,
    stream: &mut RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    pump_response(resp, stream).await
}

/// The three things sending a response over h3 needs, so the frame pump above can be
/// driven by something other than a live QUIC connection.
///
/// A `RequestStream` can only be obtained from an accepted h3 request, which made the
/// one piece of logic on this path — *when* bytes go out — reachable only from an
/// end-to-end test with a real client. That is precisely the piece that regressed:
/// buffering the whole body raised no error and broke no test, it just stopped both
/// SSE endpoints from streaming.
trait ResponseSink {
    async fn send_head(
        &mut self,
        head: Response<()>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    async fn send_body(
        &mut self,
        data: Bytes,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    async fn finish(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

impl ResponseSink for RequestStream<h3_quinn::BidiStream<Bytes>, Bytes> {
    async fn send_head(
        &mut self,
        head: Response<()>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        RequestStream::send_response(self, head).await?;
        Ok(())
    }
    async fn send_body(
        &mut self,
        data: Bytes,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        RequestStream::send_data(self, data).await?;
        Ok(())
    }
    async fn finish(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        RequestStream::finish(self).await?;
        Ok(())
    }
}

/// Head first, then every body frame as it arrives, then finish.
async fn pump_response<S: ResponseSink>(
    resp: Response<Body>,
    sink: &mut S,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use http_body_util::BodyExt as _;

    let (resp_parts, mut resp_body) = resp.into_parts();
    sink.send_head(Response::from_parts(resp_parts, ())).await?;
    // Trailers are dropped, as they were before: nothing this server emits uses them,
    // and h3's trailer API is a different call.
    while let Some(frame) = resp_body.frame().await {
        if let Ok(data) = frame?.into_data()
            && !data.is_empty()
        {
            sink.send_body(data).await?;
        }
    }
    sink.finish().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::error::ApiError;
    use crate::backend::metrics::Metrics;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use std::time::Duration;

    /// A router carrying only the metrics layer, so these tests exercise the
    /// middleware rather than the service.
    fn metered_router(metrics: Arc<Metrics>, per_project: bool) -> Router {
        let layer_metrics = Arc::clone(&metrics);
        Router::new()
            .route("/status", get(|| async { "ok" }))
            .route(
                "/v0/{project_guid}/search",
                post(|| async { ApiError::NoMatch.into_response() }),
            )
            .layer(axum::middleware::from_fn(
                move |req: Request<Body>, next: Next| {
                    let metrics = Arc::clone(&layer_metrics);
                    async move { record_request(metrics, per_project, req, next).await }
                },
            ))
    }

    /// Records what reached the wire and when, in place of a QUIC stream.
    #[derive(Default)]
    struct RecordingSink {
        head: Option<axum::http::StatusCode>,
        frames: Vec<Bytes>,
        finished: bool,
        /// Notified after each body frame, so a test can observe bytes leaving
        /// *before* the producer has finished writing.
        sent: Option<Arc<tokio::sync::Notify>>,
    }

    impl ResponseSink for RecordingSink {
        async fn send_head(
            &mut self,
            head: Response<()>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            assert!(self.head.is_none(), "the head was sent twice");
            assert!(self.frames.is_empty(), "a body frame preceded the head");
            self.head = Some(head.status());
            Ok(())
        }
        async fn send_body(
            &mut self,
            data: Bytes,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            assert!(self.head.is_some(), "a body frame preceded the head");
            assert!(!self.finished, "a body frame followed finish()");
            self.frames.push(data);
            if let Some(n) = &self.sent {
                n.notify_one();
            }
            Ok(())
        }
        async fn finish(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.finished = true;
            Ok(())
        }
    }

    /// The regression that removed streaming over h3 and broke no test: the body was
    /// collected in full before the first `send_data`, so `/index?stream=yes` and
    /// `/research` showed the client nothing until the run ended — up to seventy
    /// minutes for a `high` research run — while the whole event history accumulated
    /// in memory with no bound.
    ///
    /// The test holds the body open: if the pump buffers, this deadlocks at the
    /// timeout instead of seeing the first frame.
    #[tokio::test]
    async fn a_body_frame_reaches_the_wire_before_the_body_is_finished() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
        let body = Body::from_stream(tokio_stream::wrappers::UnboundedReceiverStream::new(rx));
        let resp = Response::builder().status(200).body(body).expect("builds");

        let sent = Arc::new(tokio::sync::Notify::new());
        let sink = Arc::new(tokio::sync::Mutex::new(RecordingSink {
            sent: Some(Arc::clone(&sent)),
            ..Default::default()
        }));

        let pump = {
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                let mut guard = sink.lock().await;
                pump_response(resp, &mut *guard).await
            })
        };

        // One event, and the producer deliberately does not finish.
        tx.send(Ok(Bytes::from_static(b"event: started\n\n")))
            .expect("send");

        tokio::time::timeout(Duration::from_secs(2), sent.notified())
            .await
            .expect(
                "no frame reached the wire while the body was still open — the response \
                 is being buffered, so neither SSE endpoint streams over h3",
            );

        // Now close the body and let the pump finish.
        tx.send(Ok(Bytes::from_static(b"event: done\n\n")))
            .expect("send");
        drop(tx);
        pump.await.expect("joins").expect("pump succeeds");

        let done = sink.lock().await;
        assert_eq!(done.head, Some(axum::http::StatusCode::OK));
        assert_eq!(
            done.frames,
            vec![
                Bytes::from_static(b"event: started\n\n"),
                Bytes::from_static(b"event: done\n\n"),
            ],
            "frames must arrive separately and in order, not concatenated into one"
        );
        assert!(done.finished, "the stream was never finished");
    }

    /// An ordinary buffered response — every non-SSE endpoint — must still send its
    /// head, its bytes and a finish, in that order.
    #[tokio::test]
    async fn a_plain_response_sends_head_then_body_then_finish() {
        let resp = Response::builder()
            .status(404)
            .body(Body::from("not found"))
            .expect("builds");

        let mut sink = RecordingSink::default();
        pump_response(resp, &mut sink).await.expect("pump succeeds");

        assert_eq!(sink.head, Some(axum::http::StatusCode::NOT_FOUND));
        assert_eq!(sink.frames, vec![Bytes::from_static(b"not found")]);
        assert!(sink.finished);
    }

    /// A response with no body at all — a 204, or the shutdown path — must still be
    /// finished, or the client waits for bytes that are never coming.
    #[tokio::test]
    async fn an_empty_response_is_still_finished() {
        let resp = Response::builder()
            .status(204)
            .body(Body::empty())
            .expect("builds");

        let mut sink = RecordingSink::default();
        pump_response(resp, &mut sink).await.expect("pump succeeds");

        assert_eq!(sink.head, Some(axum::http::StatusCode::NO_CONTENT));
        assert!(sink.frames.is_empty(), "an empty body sent bytes");
        assert!(sink.finished, "an empty response was never finished");
    }

    /// A zero-length frame is skipped rather than sent: h3 treats an empty DATA frame
    /// as a protocol oddity, and axum's stream bodies do produce them.
    #[tokio::test]
    async fn empty_frames_are_skipped_but_do_not_end_the_response() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
        for chunk in [b"a".as_slice(), b"", b"b"] {
            tx.send(Ok(Bytes::copy_from_slice(chunk))).expect("send");
        }
        drop(tx);
        let resp = Response::builder()
            .status(200)
            .body(Body::from_stream(
                tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
            ))
            .expect("builds");

        let mut sink = RecordingSink::default();
        pump_response(resp, &mut sink).await.expect("pump succeeds");

        assert_eq!(
            sink.frames,
            vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")],
            "an empty frame either reached the wire or truncated the response"
        );
        assert!(sink.finished);
    }

    /// A body that errors mid-stream — the shape a panicking detached job takes —
    /// must surface as an error rather than a quietly short response the client
    /// reads as complete.
    #[tokio::test]
    async fn a_body_that_fails_midway_is_an_error_not_a_short_response() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
        tx.send(Ok(Bytes::from_static(b"first"))).expect("send");
        tx.send(Err(std::io::Error::other("the job died")))
            .expect("send");
        drop(tx);
        let resp = Response::builder()
            .status(200)
            .body(Body::from_stream(
                tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
            ))
            .expect("builds");

        let mut sink = RecordingSink::default();
        let res = pump_response(resp, &mut sink).await;

        assert!(res.is_err(), "a failed body was reported as a clean send");
        assert_eq!(sink.frames, vec![Bytes::from_static(b"first")]);
        assert!(
            !sink.finished,
            "a truncated response was finished, so the client reads it as complete"
        );
    }

    fn sample(metrics: &Metrics, needle: &str) -> Option<String> {
        let text = metrics.render().expect("renders");
        text.lines()
            .find(|l| l.starts_with(needle))
            .map(str::to_string)
    }

    #[tokio::test]
    async fn a_served_request_is_counted_under_its_route_template() {
        let metrics = Arc::new(Metrics::new());
        let resp = metered_router(Arc::clone(&metrics), false)
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let line =
            sample(&metrics, "mindex_http_requests_total{").expect("the request counter moved");
        assert!(line.contains(r#"route="/status""#), "{line}");
        assert!(line.contains(r#"method="GET""#), "{line}");
        assert!(line.contains(r#"status="200""#), "{line}");
        assert!(
            line.contains(r#"code="""#),
            "success carries an empty code: {line}"
        );
        assert!(line.ends_with(" 1"), "{line}");

        // The template, not the concrete path — otherwise every project would be
        // its own series.
        assert!(sample(&metrics, "mindex_http_request_duration_seconds_count{").is_some());
    }

    /// The `code` label rides on the response extensions rather than in the body.
    /// This is the guard on `ApiError`'s `IntoResponse` continuing to set it.
    #[tokio::test]
    async fn an_error_is_labelled_with_its_stable_code() {
        let metrics = Arc::new(Metrics::new());
        let resp = metered_router(Arc::clone(&metrics), false)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v0/6f1a2b3c4d5e4f60817293a4b5c6d7e8/search")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let line = sample(&metrics, "mindex_http_requests_total{").expect("counted");
        assert!(
            line.contains(r#"route="/v0/{project_guid}/search""#),
            "the template collapses every project into one series: {line}"
        );
        assert!(line.contains(r#"code="search.no_match""#), "{line}");
    }

    #[tokio::test]
    async fn the_project_label_is_opt_in_and_uuid_validated() {
        // Off by default: no by-project series at all.
        let off = Arc::new(Metrics::new());
        let _ = metered_router(Arc::clone(&off), false)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v0/6f1a2b3c4d5e4f60817293a4b5c6d7e8/search")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(sample(&off, "mindex_http_requests_by_project_total{").is_none());

        // On, with a real GUID: labelled.
        let on = Arc::new(Metrics::new());
        let _ = metered_router(Arc::clone(&on), true)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v0/6f1a2b3c4d5e4f60817293a4b5c6d7e8/search")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let line = sample(&on, "mindex_http_requests_by_project_total{").expect("labelled");
        assert!(line.contains("6f1a2b3c4d5e4f60817293a4b5c6d7e8"), "{line}");

        // On, with a non-UUID path parameter: dropped rather than admitted. This
        // is the whole cardinality defence — the label a caller chooses.
        let junk = Arc::new(Metrics::new());
        let _ = metered_router(Arc::clone(&junk), true)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v0/not-a-uuid/search")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            sample(&junk, "mindex_http_requests_by_project_total{").is_none(),
            "a non-UUID path parameter must never become a label"
        );
    }

    /// Without this, `InFlightGuard` silently regresses to an inc/dec pair on the
    /// next refactor and the gauge ratchets upward — research SSE streams die
    /// *only* by disconnect, so it would be wrong within a day.
    #[tokio::test]
    async fn an_abandoned_request_does_not_leak_the_in_flight_gauge() {
        let metrics = Arc::new(Metrics::new());
        let router = Router::new()
            .route(
                "/slow",
                get(|| async {
                    // Never completes; the test drops the future instead.
                    std::future::pending::<&'static str>().await
                }),
            )
            .layer(axum::middleware::from_fn({
                let metrics = Arc::clone(&metrics);
                move |req: Request<Body>, next: Next| {
                    let metrics = Arc::clone(&metrics);
                    async move { record_request(metrics, false, req, next).await }
                }
            }));

        let mut fut =
            Box::pin(router.oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap()));
        // One poll: the request is now in flight.
        std::future::poll_fn(|cx| {
            let _ = fut.as_mut().poll(cx);
            std::task::Poll::Ready(())
        })
        .await;
        let in_flight = sample(&metrics, "mindex_http_requests_in_flight{").expect("gauge exists");
        assert!(in_flight.ends_with(" 1"), "{in_flight}");

        // Abandon it, exactly as axum does when the client disconnects.
        drop(fut);

        let in_flight = sample(&metrics, "mindex_http_requests_in_flight{").expect("gauge exists");
        assert!(in_flight.ends_with(" 0"), "gauge leaked: {in_flight}");
        let counted = sample(&metrics, "mindex_http_requests_total{").expect("recorded");
        assert!(
            counted.contains(r#"status="499""#) && counted.contains(r#"code="request.cancelled""#),
            "an abandoned request must still be reconcilable against in_flight: {counted}"
        );
    }

    /// The cardinality rule's other half. `project_guid` is the label a *caller*
    /// chooses, and it is UUID-validated above; `method` and `proto` come off the
    /// wire too, and both must collapse to a closed set. An unrecognised verb
    /// reaching the label raw is unbounded cardinality from a single attacker — or
    /// from one misconfigured client — and Prometheus does not degrade gracefully
    /// under it.
    #[test]
    fn the_method_label_comes_from_a_closed_set() {
        for (m, want) in [
            ("GET", "GET"),
            ("POST", "POST"),
            ("PUT", "PUT"),
            ("DELETE", "DELETE"),
            ("PATCH", "PATCH"),
            ("HEAD", "HEAD"),
            ("OPTIONS", "OPTIONS"),
        ] {
            let method = axum::http::Method::from_bytes(m.as_bytes()).expect("a method");
            assert_eq!(method_label(&method), want);
        }

        for exotic in ["PROPFIND", "MKCOL", "BREW", "XXXXXXXXXXXXXXXXXXXXXXXX"] {
            let method = axum::http::Method::from_bytes(exotic.as_bytes()).expect("a method");
            assert_eq!(
                method_label(&method),
                "other",
                "{exotic} reached the label raw; the method label is caller-chosen \
                 and must collapse to a closed set"
            );
        }
    }

    /// Same for the protocol. It cannot be attacker-chosen the way a method can, but
    /// the rule is one rule and a new `Version` variant appearing in a dependency
    /// bump must land in `other` rather than as a new series.
    #[test]
    fn the_proto_label_comes_from_a_closed_set() {
        for (v, want) in [
            (axum::http::Version::HTTP_09, "HTTP/0.9"),
            (axum::http::Version::HTTP_10, "HTTP/1.0"),
            (axum::http::Version::HTTP_11, "HTTP/1.1"),
            (axum::http::Version::HTTP_2, "HTTP/2"),
            (axum::http::Version::HTTP_3, "HTTP/3"),
        ] {
            assert_eq!(proto_label(v), want);
        }
    }

    /// Every label a request produces must be one of the closed values, end to end —
    /// this is the check that the middleware actually uses `method_label` rather
    /// than the method itself.
    #[tokio::test]
    async fn an_exotic_method_is_counted_under_other_not_under_itself() {
        let metrics = Arc::new(Metrics::new());
        let app = Router::new()
            .route("/status", axum::routing::any(|| async { "ok" }))
            .layer(axum::middleware::from_fn({
                let m = Arc::clone(&metrics);
                move |req: Request<Body>, next: Next| {
                    let m = Arc::clone(&m);
                    async move { record_request(m, false, req, next).await }
                }
            }));

        let _ = app
            .oneshot(
                Request::builder()
                    .method("PROPFIND")
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let text = metrics.render().expect("renders");
        assert!(
            text.contains(r#"method="other""#),
            "the exotic method was not collapsed: {text}"
        );
        assert!(
            !text.contains("PROPFIND"),
            "a caller-chosen method string became a metric label: {text}"
        );
    }
}
