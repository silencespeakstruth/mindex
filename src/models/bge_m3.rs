use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// On HTTP 429 (the embedder's "resource busy" backpressure) `encode` retries this
/// many times with exponential backoff before giving up — at which point the file
/// is marked failed and the retry worker re-attempts it later.
const MAX_429_RETRIES: u32 = 3;
/// First backoff; doubles each retry (200ms → 400ms → 800ms).
const BACKOFF_BASE: Duration = Duration::from_millis(200);

#[derive(Serialize)]
pub struct BGEm3EmbedRequest {
    pub texts: Vec<String>,
}

#[derive(Deserialize, Debug)]
pub struct BGEm3EmbedResponse {
    pub dense_vecs: Vec<Vec<f32>>,
    pub sparse_vecs: Vec<HashMap<u32, f32>>,
    pub colbert_vecs: Vec<Vec<Vec<f32>>>,
}

#[derive(Debug)]
pub enum EncodeError {
    Cancelled,
    Request(reqwest::Error),
}

impl From<reqwest::Error> for EncodeError {
    fn from(err: reqwest::Error) -> Self {
        Self::Request(err)
    }
}

#[async_trait]
pub trait BGEm3Model: Send + Sync {
    async fn encode(
        &self,
        req: BGEm3EmbedRequest,
        token: CancellationToken,
    ) -> Result<BGEm3EmbedResponse, EncodeError>;

    /// Liveness ping of the embedder's own `/health` — confirms reachability
    /// without loading the model. Bounded by a short timeout so `/health` on the
    /// mindex side can't hang on a wedged embedder.
    async fn health(&self) -> Result<(), EncodeError>;
}

pub struct BGEm3HttpClient {
    client: reqwest::Client,
    base_url: Url,
    /// First 429 backoff (doubles each retry). A field so tests can shrink it.
    backoff_base: Duration,
}

impl BGEm3HttpClient {
    pub fn new(base_url: Url) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            backoff_base: BACKOFF_BASE,
        }
    }
}

#[async_trait]
impl BGEm3Model for BGEm3HttpClient {
    async fn encode(
        &self,
        req: BGEm3EmbedRequest,
        token: CancellationToken,
    ) -> Result<BGEm3EmbedResponse, EncodeError> {
        let url = self.base_url.join("encode").unwrap(); // This should not ever happen.

        let mut attempt: u32 = 0;
        loop {
            let send = self.client.post(url.clone()).json(&req).send();

            let response = tokio::select! {
                _ = token.cancelled() => return Err(EncodeError::Cancelled),
                res = send => res?,
            };

            // 429 = the embedder is at capacity. Back off and retry a few times
            // before surfacing the error.
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                && attempt < MAX_429_RETRIES
            {
                let delay = self.backoff_base * 2u32.pow(attempt);
                warn!(
                    attempt = attempt + 1,
                    max_attempts = MAX_429_RETRIES,
                    ?delay,
                    "Embedder returned 429 (busy); backing off and retrying."
                );
                tokio::select! {
                    _ = token.cancelled() => return Err(EncodeError::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
                attempt += 1;
                continue;
            }

            // Final attempt, or a non-429 status: turn any non-2xx (including a
            // persistent 429) into an error, otherwise parse the body.
            let response = response.error_for_status()?;
            let body = tokio::select! {
                _ = token.cancelled() => return Err(EncodeError::Cancelled),
                body = response.json::<BGEm3EmbedResponse>() => body?,
            };
            return Ok(body);
        }
    }

    async fn health(&self) -> Result<(), EncodeError> {
        let url = self.base_url.join("health").unwrap(); // join of a literal cannot fail
        self.client
            .get(url)
            .timeout(Duration::from_secs(2))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stub embedder that replies 429 for the first `fail_first` requests, then 200
    /// with an (empty but valid) `BGEm3EmbedResponse`. Returns a client pointed at it
    /// and the shared request counter.
    async fn stub_embedder(fail_first: usize) -> (BGEm3HttpClient, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/encode",
            post({
                let hits = hits.clone();
                move || {
                    let hits = hits.clone();
                    async move {
                        let n = hits.fetch_add(1, Ordering::SeqCst);
                        if n < fail_first {
                            (StatusCode::TOO_MANY_REQUESTS, "busy").into_response()
                        } else {
                            axum::Json(serde_json::json!({
                                "dense_vecs": [],
                                "sparse_vecs": [],
                                "colbert_vecs": []
                            }))
                            .into_response()
                        }
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = BGEm3HttpClient {
            client: reqwest::Client::new(),
            base_url: Url::parse(&format!("http://{addr}/")).unwrap(),
            backoff_base: Duration::from_millis(1), // keep the test fast
        };
        (client, hits)
    }

    fn req() -> BGEm3EmbedRequest {
        BGEm3EmbedRequest { texts: vec!["x".into()] }
    }

    #[tokio::test]
    async fn succeeds_after_being_throttled() {
        // 429 twice, then 200 → 3 total requests, Ok.
        let (client, hits) = stub_embedder(2).await;
        let res = client.encode(req(), CancellationToken::new()).await;
        assert!(res.is_ok(), "expected success after retries, got {res:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_three_retries() {
        // Always 429 → 1 initial + 3 retries = 4 requests, then give up.
        let (client, hits) = stub_embedder(usize::MAX).await;
        let res = client.encode(req(), CancellationToken::new()).await;
        assert!(matches!(res, Err(EncodeError::Request(_))), "expected give-up, got {res:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 1 + MAX_429_RETRIES as usize);
    }

    #[tokio::test]
    async fn cancellation_during_backoff_returns_cancelled() {
        // First response is 429; cancelling before the retry must short-circuit.
        let (client, _hits) = stub_embedder(usize::MAX).await;
        let token = CancellationToken::new();
        token.cancel();
        let res = client.encode(req(), token).await;
        assert!(matches!(res, Err(EncodeError::Cancelled)));
    }
}
