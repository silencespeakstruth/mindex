//! The embedding client: OpenAI-compatible `/v1/embeddings`, as served by vLLM.
//!
//! The old client spoke a custom binary protocol to a vendored BGE-M3 server,
//! which existed only because no general model server returned dense, sparse
//! and ColBERT together. Retrieval is dense-only now, so the vendored server is
//! gone and this client speaks the one format every serving stack already
//! offers. Two properties carried over verbatim because they were hard-won:
//! the **whole-call deadline** (retries and backoffs included — per-attempt
//! bounds let a throttled server hold a search open for forty minutes), and
//! the retry loop counting its backoffs from *inside* the client (from outside,
//! three-retries-then-success is indistinguishable from one success).
//!
//! Two properties are new, and both close identity holes the old pipeline had:
//! every response row is checked against the registry's dimension, and
//! `served_models` exposes `GET /v1/models` so startup and `/health` can verify
//! the server actually serves the model `[model].id` names. Nothing checked
//! that before — a wrong embedder behind the right URL indexed silently.

use async_trait::async_trait;
use prometheus_client::metrics::counter::Counter;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::backend::metrics::{EmbedderLabels, EmbedderOutcomeLabels, Metrics};

/// Operational tuning for the embedder HTTP client (all from `[model]` config).
#[derive(Debug, Clone, Copy)]
pub struct EmbedderTuning {
    /// On HTTP 429 or 503 — both are vLLM's "at capacity" spellings — `embed`
    /// retries this many times with exponential backoff before giving up, at
    /// which point the file is marked failed and the retry worker re-attempts
    /// it later.
    pub max_429_retries: u32,
    /// First backoff; doubles each retry (e.g. 200ms → 400ms → 800ms).
    pub backoff_base_ms: u64,
    /// Liveness-ping timeout for the embedder's `/health`. Also bounds the
    /// `/v1/models` handshake — both are cheap control-plane reads.
    pub health_timeout_ms: u64,
    /// Ceiling on one `embed` **call**, retries and backoffs included.
    ///
    /// Per attempt it bounded nothing useful: the worst case was
    /// `(1 + max_429_retries)` full timeouts plus the backoffs between them —
    /// forty minutes at the defaults — while a throttled embedder held a search
    /// open or kept a file's indexing claim. Each attempt gets whatever is left.
    pub encode_timeout_ms: u64,
}

#[derive(Debug)]
pub enum EncodeError {
    Cancelled,
    Request(reqwest::Error),
    /// The whole call — every attempt and every backoff between them — ran past
    /// `[model].encode_timeout_ms`. Distinct from `Request`, which carries
    /// reqwest's own per-request timeout: this one means the embedder kept
    /// answering "busy" until the caller's budget was gone, which is a load
    /// diagnosis, not a network one.
    Timeout(std::time::Duration),
    /// The response was not what the registry promised: unparseable JSON, a row
    /// count that does not match the request, or a vector of the wrong width —
    /// the last being the signature of a server serving a different model than
    /// `[model].id` names.
    Decode(String),
}

impl From<reqwest::Error> for EncodeError {
    fn from(err: reqwest::Error) -> Self {
        Self::Request(err)
    }
}

#[derive(Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a [String],
    encoding_format: &'static str,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingRow>,
}

#[derive(Deserialize)]
struct EmbeddingRow {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelRow>,
}

#[derive(Deserialize)]
struct ModelRow {
    id: String,
}

#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed `texts`, one dense vector per text, in request order.
    async fn embed(
        &self,
        texts: Vec<String>,
        token: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, EncodeError>;

    /// Liveness ping of the server's own `/health` — confirms reachability
    /// without running inference. Bounded by a short timeout so `/health` on
    /// the mindex side can't hang on a wedged embedder.
    async fn health(&self) -> Result<(), EncodeError>;

    /// The ids `GET /v1/models` reports. The handshake half of model identity:
    /// a server that answers must name the model mindex is configured for, or
    /// startup refuses and `/health` reports the embedder as failing — a wrong
    /// model behind the right URL would otherwise index silently, and the dim
    /// check alone cannot tell two different models of the same width apart.
    async fn served_models(&self) -> Result<Vec<String>, EncodeError>;
}

pub struct OpenAiEmbedClient {
    client: reqwest::Client,
    base_url: Url,
    /// The `model` field of every request and what the handshake expects in
    /// `/v1/models`. Defaults to the registry's HF repo; `[model].served_name`
    /// overrides it for a vLLM started with `--served-model-name`.
    served_name: String,
    /// The registry dim — every response row is checked against it.
    expected_dim: usize,
    /// Retry budget for 429/503 (from config).
    max_429_retries: u32,
    /// First backoff (doubles each retry). A field so tests can shrink it.
    backoff_base: Duration,
    /// `/health` and `/v1/models` timeout (from config).
    health_timeout: Duration,
    /// Whole-call `embed` budget (from config).
    encode_timeout: Duration,
    /// The one metric that cannot be a decorator: the retry loop lives *inside*
    /// `embed`, so from outside three retries followed by a success is
    /// indistinguishable from one success. Set by `with_metrics`; `None` in the
    /// tests that construct a bare client.
    retries: Option<Counter>,
}

impl OpenAiEmbedClient {
    pub fn new(
        base_url: Url,
        served_name: String,
        expected_dim: usize,
        tuning: EmbedderTuning,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            served_name,
            expected_dim,
            max_429_retries: tuning.max_429_retries,
            backoff_base: Duration::from_millis(tuning.backoff_base_ms),
            health_timeout: Duration::from_millis(tuning.health_timeout_ms),
            encode_timeout: Duration::from_millis(tuning.encode_timeout_ms),
            retries: None,
        }
    }

    /// Count busy-backoffs against `embedder`. A builder rather than a `new`
    /// parameter so every existing construction site — including the tests —
    /// keeps working unchanged.
    #[must_use]
    pub fn with_metrics(mut self, metrics: &Metrics, embedder: &'static str) -> Self {
        self.retries = Some(
            metrics
                .embed
                .retries
                .get_or_create(&EmbedderLabels { embedder })
                .clone(),
        );
        self
    }

    /// Turn a parsed `/v1/embeddings` body into vectors in request order,
    /// refusing every shape the registry did not promise.
    fn validate(
        &self,
        mut parsed: EmbeddingsResponse,
        expected_rows: usize,
    ) -> Result<Vec<Vec<f32>>, EncodeError> {
        if parsed.data.len() != expected_rows {
            return Err(EncodeError::Decode(format!(
                "/v1/embeddings returned {} rows for {} inputs; the server and \
                 this binary disagree about the request — check the embedder's log",
                parsed.data.len(),
                expected_rows
            )));
        }
        // The API contract says rows arrive in input order; sorting by the
        // index the server itself asserts costs nothing and removes the trust.
        parsed.data.sort_by_key(|row| row.index);
        parsed
            .data
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                if row.index != i {
                    return Err(EncodeError::Decode(format!(
                        "/v1/embeddings row indexes are not 0..{expected_rows} \
                         (missing or duplicate index {i})"
                    )));
                }
                if row.embedding.len() != self.expected_dim {
                    return Err(EncodeError::Decode(format!(
                        "row {i} is {}-d but {} is {}-d — the server at {} is \
                         serving a different model than [model].id names",
                        row.embedding.len(),
                        self.served_name,
                        self.expected_dim,
                        self.base_url
                    )));
                }
                Ok(row.embedding)
            })
            .collect()
    }
}

/// Metrics decorator over [`Embedder`].
///
/// Two instances exist, one per embedder role, each with its label baked in.
/// Two wrappers even when `[model].query_server_url` is unset and the inner
/// `Arc` is literally the same object: the split between indexing traffic
/// (batches of hundreds) and query traffic (one short text) is the interesting
/// axis, and taking it here avoids the `Arc::ptr_eq` dance `/health` needs.
pub struct MeteredEmbedder {
    inner: Arc<dyn Embedder>,
    metrics: Arc<Metrics>,
    embedder: &'static str,
}

impl MeteredEmbedder {
    pub fn new(inner: Arc<dyn Embedder>, metrics: Arc<Metrics>, embedder: &'static str) -> Self {
        Self {
            inner,
            metrics,
            embedder,
        }
    }
}

#[async_trait]
impl Embedder for MeteredEmbedder {
    async fn embed(
        &self,
        texts: Vec<String>,
        token: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, EncodeError> {
        let embedder = self.embedder;
        let n = texts.len() as u64;
        let e = &self.metrics.embed;
        let labels = EmbedderLabels { embedder };

        e.batch_size.get_or_create(&labels).observe(n as f64);

        let started = std::time::Instant::now();
        let result = self.inner.embed(texts, token).await;
        e.duration
            .get_or_create(&labels)
            .observe(started.elapsed().as_secs_f64());

        let outcome = match &result {
            Ok(_) => "ok",
            Err(EncodeError::Cancelled) => "cancelled",
            Err(EncodeError::Request(_)) => "request",
            // Its own label: "the embedder was too busy for too long" is a capacity
            // problem, and bucketing it with network failures would send the reader
            // looking at the wrong thing.
            Err(EncodeError::Timeout(_)) => "timeout",
            Err(EncodeError::Decode(_)) => "decode",
        };
        e.requests
            .get_or_create(&EmbedderOutcomeLabels { embedder, outcome })
            .inc();
        if result.is_ok() {
            e.texts.get_or_create(&labels).inc_by(n);
        }
        result
    }

    async fn health(&self) -> Result<(), EncodeError> {
        self.inner.health().await
    }

    async fn served_models(&self) -> Result<Vec<String>, EncodeError> {
        self.inner.served_models().await
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedClient {
    async fn embed(
        &self,
        texts: Vec<String>,
        token: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, EncodeError> {
        let url = self.base_url.join("v1/embeddings").unwrap(); // join of a literal cannot fail
        let expected_rows = texts.len();
        let body = EmbeddingsRequest {
            model: &self.served_name,
            input: &texts,
            encoding_format: "float",
        };

        // `encode_timeout` bounds the **whole call**, retries and backoffs included,
        // rather than each attempt separately. Per attempt it bounded nothing useful:
        // the worst case was `(1 + max_429_retries)` timeouts plus the backoffs between
        // them — at the defaults, forty minutes — during which a throttled embedder held
        // a search open or kept a file's indexing claim. Each attempt now gets whatever
        // is left, so a busy embedder still gets its retries but the caller's wait has a
        // number it can be told.
        let deadline = tokio::time::Instant::now() + self.encode_timeout;
        let mut attempt: u32 = 0;
        loop {
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                warn!(
                    attempts = attempt,
                    budget = ?self.encode_timeout,
                    "Embedder call exhausted its whole-call budget while retrying; \
                     giving up. Sysadmin: the embedder is saturated — check its load, \
                     or raise [model].encode_timeout_ms."
                );
                return Err(EncodeError::Timeout(self.encode_timeout));
            };
            let send = self
                .client
                .post(url.clone())
                .timeout(remaining)
                .json(&body)
                .send();

            let response = tokio::select! {
                _ = token.cancelled() => return Err(EncodeError::Cancelled),
                res = send => res?,
            };

            // 429 and 503 are both vLLM's "at capacity": back off and retry a few
            // times before surfacing the error.
            let status = response.status();
            if (status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::SERVICE_UNAVAILABLE)
                && attempt < self.max_429_retries
            {
                // Clamped to what is left, so the sleep cannot outlive the budget and
                // the next iteration's check is the one that reports it.
                let delay = (self.backoff_base * 2u32.pow(attempt)).min(remaining);
                warn!(
                    attempt = attempt + 1,
                    max_attempts = self.max_429_retries,
                    status = %status,
                    ?delay,
                    "Embedder returned busy; backing off and retrying."
                );
                if let Some(c) = &self.retries {
                    c.inc();
                }
                tokio::select! {
                    _ = token.cancelled() => return Err(EncodeError::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
                attempt += 1;
                continue;
            }

            // Final attempt, or a non-busy status: turn any non-2xx (including a
            // persistent 429/503) into an error, otherwise parse the body.
            let response = response.error_for_status()?;
            let bytes = tokio::select! {
                _ = token.cancelled() => return Err(EncodeError::Cancelled),
                body = response.bytes() => body?,
            };
            let parsed: EmbeddingsResponse = serde_json::from_slice(&bytes).map_err(|e| {
                EncodeError::Decode(format!(
                    "/v1/embeddings body is not the OpenAI embeddings shape: {e}"
                ))
            })?;
            return self.validate(parsed, expected_rows);
        }
    }

    async fn health(&self) -> Result<(), EncodeError> {
        let url = self.base_url.join("health").unwrap(); // join of a literal cannot fail
        self.client
            .get(url)
            .timeout(self.health_timeout)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn served_models(&self) -> Result<Vec<String>, EncodeError> {
        let url = self.base_url.join("v1/models").unwrap(); // join of a literal cannot fail
        let bytes = self
            .client
            .get(url)
            .timeout(self.health_timeout)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let parsed: ModelsResponse = serde_json::from_slice(&bytes).map_err(|e| {
            EncodeError::Decode(format!(
                "/v1/models body is not the OpenAI models shape: {e}"
            ))
        })?;
        Ok(parsed.data.into_iter().map(|m| m.id).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The retry budget the test client is built with.
    const TEST_MAX_RETRIES: u32 = 3;

    const TEST_DIM: usize = 4;

    fn ok_body(rows: usize, dim: usize) -> serde_json::Value {
        json!({
            "object": "list",
            "model": "test-model",
            "data": (0..rows).map(|i| json!({
                "object": "embedding",
                "index": i,
                "embedding": vec![0.5_f32; dim],
            })).collect::<Vec<_>>(),
        })
    }

    fn test_client(addr: std::net::SocketAddr) -> OpenAiEmbedClient {
        OpenAiEmbedClient {
            client: reqwest::Client::new(),
            base_url: Url::parse(&format!("http://{addr}/")).unwrap(),
            served_name: "test-model".into(),
            expected_dim: TEST_DIM,
            max_429_retries: TEST_MAX_RETRIES,
            backoff_base: Duration::from_millis(1), // keep the tests fast
            health_timeout: Duration::from_secs(2),
            encode_timeout: Duration::from_secs(5),
            retries: None,
        }
    }

    /// Stub embedder that replies `busy_status` for the first `fail_first`
    /// requests, then 200 with a valid one-row body. Returns a client pointed
    /// at it and the shared request counter.
    async fn stub_embedder(
        fail_first: usize,
        busy_status: StatusCode,
    ) -> (OpenAiEmbedClient, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1/embeddings",
            post({
                let hits = hits.clone();
                move || {
                    let hits = hits.clone();
                    async move {
                        let n = hits.fetch_add(1, Ordering::SeqCst);
                        if n < fail_first {
                            (busy_status, "busy").into_response()
                        } else {
                            axum::Json(ok_body(1, TEST_DIM)).into_response()
                        }
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        (test_client(addr), hits)
    }

    fn texts() -> Vec<String> {
        vec!["x".into()]
    }

    #[tokio::test]
    async fn succeeds_after_being_throttled() {
        // 429 twice, then 200 → 3 total requests, Ok.
        let (client, hits) = stub_embedder(2, StatusCode::TOO_MANY_REQUESTS).await;
        let res = client.embed(texts(), CancellationToken::new()).await;
        assert!(res.is_ok(), "expected success after retries, got {res:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    /// vLLM spells "at capacity" as 503 at least as often as 429; both must
    /// take the backoff path rather than surfacing as a hard failure.
    #[tokio::test]
    async fn a_503_is_retried_like_a_429() {
        let (client, hits) = stub_embedder(2, StatusCode::SERVICE_UNAVAILABLE).await;
        let res = client.embed(texts(), CancellationToken::new()).await;
        assert!(res.is_ok(), "expected success after retries, got {res:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_three_retries() {
        // Always 429 → 1 initial + 3 retries = 4 requests, then give up.
        let (client, hits) = stub_embedder(usize::MAX, StatusCode::TOO_MANY_REQUESTS).await;
        let res = client.embed(texts(), CancellationToken::new()).await;
        assert!(
            matches!(res, Err(EncodeError::Request(_))),
            "expected give-up, got {res:?}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1 + TEST_MAX_RETRIES as usize);
    }

    #[tokio::test]
    async fn cancellation_during_backoff_returns_cancelled() {
        // First response is busy; cancelling before the retry must short-circuit.
        let (client, _hits) = stub_embedder(usize::MAX, StatusCode::TOO_MANY_REQUESTS).await;
        let token = CancellationToken::new();
        token.cancel();
        let res = client.embed(texts(), token).await;
        assert!(matches!(res, Err(EncodeError::Cancelled)));
    }

    #[tokio::test]
    async fn embed_times_out_on_a_wedged_embedder() {
        // A stub that accepts the request and then never responds.
        let app = Router::new().route(
            "/v1/embeddings",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                "never reached"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut client = test_client(addr);
        client.encode_timeout = Duration::from_millis(100);

        let started = std::time::Instant::now();
        let res = client.embed(texts(), CancellationToken::new()).await;
        match res {
            Err(EncodeError::Request(e)) => {
                assert!(e.is_timeout(), "expected a timeout, got {e:?}")
            }
            other => panic!("expected Err(Request(timeout)), got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must fail fast, not hang"
        );
    }

    /// The whole-call budget, not the per-attempt one. A `Request(timeout)` means a
    /// single attempt hung; this is the other diagnosis — the embedder answered
    /// promptly every time, always "busy", until the caller's budget was gone.
    #[tokio::test]
    async fn a_permanently_busy_embedder_ends_the_call_at_the_whole_call_budget() {
        let (mut client, hits) = stub_embedder(usize::MAX, StatusCode::TOO_MANY_REQUESTS).await;
        // Ten retries would be granted, but the budget only affords a couple of
        // backoffs — so the *budget* must be what ends the call, not the retry count.
        client.max_429_retries = 10;
        client.backoff_base = Duration::from_millis(100);
        client.encode_timeout = Duration::from_millis(250);

        let started = std::time::Instant::now();
        let res = client.embed(texts(), CancellationToken::new()).await;

        match res {
            Err(EncodeError::Timeout(budget)) => {
                assert_eq!(budget, Duration::from_millis(250), "reports its own budget")
            }
            other => panic!("expected Err(Timeout), got {other:?}"),
        }
        assert!(
            hits.load(Ordering::SeqCst) < 1 + 10,
            "the retry budget must not have been spent in full; the clock ended it"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "overran the whole-call budget by more than an order of magnitude"
        );
    }

    /// The backoff is clamped to what is left of the budget, so a base delay far
    /// larger than the whole budget cannot make one sleep outlive it.
    #[tokio::test]
    async fn a_backoff_longer_than_the_budget_does_not_outlive_it() {
        let (mut client, _hits) = stub_embedder(usize::MAX, StatusCode::TOO_MANY_REQUESTS).await;
        client.max_429_retries = 5;
        client.backoff_base = Duration::from_secs(30);
        client.encode_timeout = Duration::from_millis(150);

        let started = std::time::Instant::now();
        let res = client.embed(texts(), CancellationToken::new()).await;

        assert!(matches!(res, Err(EncodeError::Timeout(_))), "got {res:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a 30s backoff was not clamped to the 150ms budget: took {:?}",
            started.elapsed()
        );
    }

    /// One fixed-body stub, for the response-shape tests.
    async fn stub_with_body(body: serde_json::Value) -> OpenAiEmbedClient {
        let app = Router::new().route(
            "/v1/embeddings",
            post(move || {
                let body = body.clone();
                async move { axum::Json(body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        test_client(addr)
    }

    /// A row of the wrong width is the signature of the server serving a
    /// different model than `[model].id` names — the error must say so and
    /// name both dims, because "search finds nothing obvious" is otherwise
    /// the only symptom.
    #[tokio::test]
    async fn a_wrong_dim_row_is_a_decode_error_naming_both_dims() {
        let mut body = ok_body(2, TEST_DIM);
        body["data"][1]["embedding"] = json!(vec![0.5_f32; TEST_DIM + 3]);
        let client = stub_with_body(body).await;
        let res = client
            .embed(vec!["a".into(), "b".into()], CancellationToken::new())
            .await;
        match res {
            Err(EncodeError::Decode(msg)) => {
                assert!(msg.contains("7-d") && msg.contains("4-d"), "got: {msg}");
                assert!(msg.contains("different model"), "got: {msg}");
            }
            other => panic!("expected Err(Decode), got {other:?}"),
        }
    }

    /// Rows are returned in `index` order regardless of body order — the
    /// vectors are matched to chunks positionally downstream, so a reordered
    /// body silently mis-assigning vectors would corrupt the index.
    #[tokio::test]
    async fn rows_are_reordered_by_index() {
        let body = json!({
            "object": "list",
            "data": [
                { "index": 1, "embedding": vec![1.0_f32; TEST_DIM] },
                { "index": 0, "embedding": vec![0.0_f32; TEST_DIM] },
            ],
        });
        let client = stub_with_body(body).await;
        let out = client
            .embed(vec!["a".into(), "b".into()], CancellationToken::new())
            .await
            .expect("valid body should parse");
        assert_eq!(out[0], vec![0.0_f32; TEST_DIM]);
        assert_eq!(out[1], vec![1.0_f32; TEST_DIM]);
    }

    /// A short response used to be truncated by `zip` downstream — chunks
    /// silently never upserted while the file went `indexed`. The client now
    /// refuses it before anything can be misassigned.
    #[tokio::test]
    async fn a_short_response_is_a_decode_error() {
        let client = stub_with_body(ok_body(1, TEST_DIM)).await;
        let res = client
            .embed(vec!["a".into(), "b".into()], CancellationToken::new())
            .await;
        match res {
            Err(EncodeError::Decode(msg)) => {
                assert!(
                    msg.contains("1 rows") && msg.contains("2 inputs"),
                    "got: {msg}"
                )
            }
            other => panic!("expected Err(Decode), got {other:?}"),
        }
    }

    /// Duplicate indexes must not pass the reorder as if they were a
    /// permutation — after sorting, the row at position i must assert index i.
    #[tokio::test]
    async fn duplicate_row_indexes_are_refused() {
        let body = json!({
            "object": "list",
            "data": [
                { "index": 1, "embedding": vec![1.0_f32; TEST_DIM] },
                { "index": 1, "embedding": vec![2.0_f32; TEST_DIM] },
            ],
        });
        let client = stub_with_body(body).await;
        let res = client
            .embed(vec!["a".into(), "b".into()], CancellationToken::new())
            .await;
        assert!(matches!(res, Err(EncodeError::Decode(_))), "got {res:?}");
    }

    #[tokio::test]
    async fn a_non_json_body_is_a_decode_error() {
        let app = Router::new().route("/v1/embeddings", post(|| async { "not json" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = test_client(addr);
        let res = client.embed(texts(), CancellationToken::new()).await;
        assert!(matches!(res, Err(EncodeError::Decode(_))), "got {res:?}");
    }

    #[tokio::test]
    async fn served_models_reads_the_openai_models_list() {
        let app = Router::new().route(
            "/v1/models",
            get(|| async {
                axum::Json(json!({
                    "object": "list",
                    "data": [
                        { "id": "Qwen/Qwen3-Embedding-0.6B", "object": "model" },
                    ],
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = test_client(addr);
        let models = client.served_models().await.expect("must parse");
        assert_eq!(models, vec!["Qwen/Qwen3-Embedding-0.6B".to_string()]);
    }
}
