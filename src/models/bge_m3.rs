use async_trait::async_trait;
use prometheus_client::metrics::counter::Counter;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::backend::metrics::{EmbedderLabels, EmbedderOutcomeLabels, Metrics};

/// Operational tuning for the embedder HTTP client (all from `[model]` config).
#[derive(Debug, Clone, Copy)]
pub struct BGEm3Tuning {
    /// On HTTP 429 (the embedder's "resource busy" backpressure) `encode` retries
    /// this many times with exponential backoff before giving up — at which point
    /// the file is marked failed and the retry worker re-attempts it later.
    pub max_429_retries: u32,
    /// First 429 backoff; doubles each retry (e.g. 200ms → 400ms → 800ms).
    pub backoff_base_ms: u64,
    /// Liveness-ping timeout for the embedder's `/health`.
    pub health_timeout_ms: u64,
    /// Ceiling on one `/encode` **call**, retries and 429 backoffs included.
    ///
    /// Per attempt it bounded nothing useful: the worst case was
    /// `(1 + max_429_retries)` full timeouts plus the backoffs between them — forty
    /// minutes at the defaults — while a throttled embedder held a search open or kept
    /// a file's indexing claim. Each attempt now gets whatever is left of this.
    pub encode_timeout_ms: u64,
}

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

/// `/encode` replies with a little-endian binary stream, not JSON: the ColBERT
/// head is a multivector (one 1024-d vector *per token*), so a JSON body ran to
/// hundreds of MB and the embedder spent ~70% of each request boxing it into
/// Python floats + serializing. The heads have fixed shapes, so a length-prefixed
/// binary needs no schema library. Layout (mirrors `pack_encode` in
/// `embedder/src/bge_m3_api/__main__.py` — keep the two in lockstep):
///
/// ```text
/// magic   b"BM3\x01"                       (4 bytes)
/// n       u32                              chunk count
/// dim     u32                              dense/colbert width (1024)
/// dense   n * dim * f32                    one row per chunk
/// sparse  per chunk: count u32, count*u32 token-ids, count*f32 weights
/// colbert per chunk: tokens u32, tokens * dim * f32
/// ```
const ENCODE_MAGIC: &[u8; 4] = b"BM3\x01";

#[derive(Debug)]
pub enum EncodeError {
    Cancelled,
    Request(reqwest::Error),
    /// The whole call — every attempt and every 429 backoff between them — ran past
    /// `[model].encode_timeout_ms`. Distinct from `Request`, which carries reqwest's
    /// own per-request timeout: this one means the embedder kept answering "busy"
    /// until the caller's budget was gone, which is a load diagnosis, not a network
    /// one.
    Timeout(std::time::Duration),
    /// The embedder's binary body didn't match the wire format (truncated, bad
    /// magic, or trailing bytes) — a client/embedder version skew.
    Decode(String),
}

impl From<reqwest::Error> for EncodeError {
    fn from(err: reqwest::Error) -> Self {
        Self::Request(err)
    }
}

/// Forward-only cursor over the binary `/encode` body, with bounds checks so a
/// short or malformed body yields `EncodeError::Decode` instead of a panic.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], EncodeError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| {
                EncodeError::Decode(format!(
                    "truncated /encode body: need {n} bytes at offset {}, body is {} bytes",
                    self.pos,
                    self.buf.len()
                ))
            })?;
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u32(&mut self) -> Result<u32, EncodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_f32s(&mut self, count: usize) -> Result<Vec<f32>, EncodeError> {
        Ok(self
            .take(count * 4)?
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }

    fn read_u32s(&mut self, count: usize) -> Result<Vec<u32>, EncodeError> {
        Ok(self
            .take(count * 4)?
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Refuse a `Vec::with_capacity` the body could not possibly fill.
    ///
    /// Reads are bounds-checked; capacities were not, and `count` comes straight off
    /// the wire as a `u32`. A corrupt or hostile `/encode` body therefore asked for a
    /// ~100 GB allocation and aborted the process before a single read could reject it.
    /// The bound needs no invented constant: `count` elements of `per` bytes cannot
    /// exceed what is left in the buffer.
    fn checked_capacity(&self, count: usize, per: usize) -> Result<usize, EncodeError> {
        match count.checked_mul(per) {
            Some(bytes) if bytes <= self.remaining() => Ok(count),
            _ => Err(EncodeError::Decode(format!(
                "/encode body claims {count} elements of {per} bytes but only \
                 {} bytes remain; truncated or wire-format mismatch",
                self.remaining()
            ))),
        }
    }
}

/// Parse the binary `/encode` body into the same shape the rest of the code
/// already consumes. The per-token ColBERT vectors and dense rows become `Vec`s
/// (native, no JSON tokenizing), so downstream `embed.rs` is unchanged.
fn parse_encode_response(buf: &[u8]) -> Result<BGEm3EmbedResponse, EncodeError> {
    let mut r = Reader::new(buf);
    if r.take(4)? != ENCODE_MAGIC {
        return Err(EncodeError::Decode(
            "bad magic in /encode body; embedder and client wire formats disagree".into(),
        ));
    }
    let n = r.read_u32()? as usize;
    let dim = r.read_u32()? as usize;

    // Each of the three sections is at least `n` elements of its own smallest possible
    // size, so the buffer must be able to hold that much before anything is reserved.
    let mut dense_vecs = Vec::with_capacity(r.checked_capacity(n, dim * 4)?);
    for _ in 0..n {
        dense_vecs.push(r.read_f32s(dim)?);
    }

    // A sparse row is at least its own `count` u32, so 4 bytes each at minimum.
    let mut sparse_vecs = Vec::with_capacity(r.checked_capacity(n, 4)?);
    for _ in 0..n {
        let count = r.read_u32()? as usize;
        let ids = r.read_u32s(count)?;
        let weights = r.read_f32s(count)?;
        sparse_vecs.push(ids.into_iter().zip(weights).collect());
    }

    let mut colbert_vecs = Vec::with_capacity(r.checked_capacity(n, 4)?);
    for _ in 0..n {
        let tokens = r.read_u32()? as usize;
        let mut chunk = Vec::with_capacity(r.checked_capacity(tokens, dim * 4)?);
        for _ in 0..tokens {
            chunk.push(r.read_f32s(dim)?);
        }
        colbert_vecs.push(chunk);
    }

    if r.remaining() != 0 {
        return Err(EncodeError::Decode(format!(
            "{} trailing bytes after /encode body; wire-format mismatch",
            r.remaining()
        )));
    }
    Ok(BGEm3EmbedResponse {
        dense_vecs,
        sparse_vecs,
        colbert_vecs,
    })
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
    /// 429-retry budget (from config).
    max_429_retries: u32,
    /// First 429 backoff (doubles each retry). A field so tests can shrink it.
    backoff_base: Duration,
    /// `/health` ping timeout (from config).
    health_timeout: Duration,
    /// Per-attempt `/encode` request timeout (from config).
    encode_timeout: Duration,
    /// The one metric that cannot be a decorator: the 429 retry loop lives
    /// *inside* `encode`, so from outside three retries followed by a success is
    /// indistinguishable from one success. Set by `with_metrics`; `None` in the
    /// tests that construct a bare client.
    retries_429: Option<Counter>,
}

impl BGEm3HttpClient {
    pub fn new(base_url: Url, tuning: BGEm3Tuning) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            max_429_retries: tuning.max_429_retries,
            backoff_base: Duration::from_millis(tuning.backoff_base_ms),
            health_timeout: Duration::from_millis(tuning.health_timeout_ms),
            encode_timeout: Duration::from_millis(tuning.encode_timeout_ms),
            retries_429: None,
        }
    }

    /// Count 429 backoffs against `embedder`. A builder rather than a `new`
    /// parameter so every existing construction site — including the tests —
    /// keeps working unchanged.
    #[must_use]
    pub fn with_metrics(mut self, metrics: &Metrics, embedder: &'static str) -> Self {
        self.retries_429 = Some(
            metrics
                .embed
                .retries
                .get_or_create(&EmbedderLabels { embedder })
                .clone(),
        );
        self
    }
}

/// Metrics decorator over [`BGEm3Model`].
///
/// Two instances exist, one per embedder role, each with its label baked in.
/// Two wrappers even when `[model].query_server_url` is unset and the inner
/// `Arc` is literally the same object: the split between indexing traffic
/// (batches of hundreds) and query traffic (one short text) is the interesting
/// axis, and taking it here avoids the `Arc::ptr_eq` dance `/health` needs.
pub struct MeteredEmbedder {
    inner: Arc<dyn BGEm3Model>,
    metrics: Arc<Metrics>,
    embedder: &'static str,
}

impl MeteredEmbedder {
    pub fn new(inner: Arc<dyn BGEm3Model>, metrics: Arc<Metrics>, embedder: &'static str) -> Self {
        Self {
            inner,
            metrics,
            embedder,
        }
    }
}

#[async_trait]
impl BGEm3Model for MeteredEmbedder {
    async fn encode(
        &self,
        req: BGEm3EmbedRequest,
        token: CancellationToken,
    ) -> Result<BGEm3EmbedResponse, EncodeError> {
        let embedder = self.embedder;
        let texts = req.texts.len() as u64;
        let e = &self.metrics.embed;
        let labels = EmbedderLabels { embedder };

        e.batch_size.get_or_create(&labels).observe(texts as f64);

        let started = std::time::Instant::now();
        let result = self.inner.encode(req, token).await;
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
            e.texts.get_or_create(&labels).inc_by(texts);
        }
        result
    }

    async fn health(&self) -> Result<(), EncodeError> {
        self.inner.health().await
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
                .json(&req)
                .send();

            let response = tokio::select! {
                _ = token.cancelled() => return Err(EncodeError::Cancelled),
                res = send => res?,
            };

            // 429 = the embedder is at capacity. Back off and retry a few times
            // before surfacing the error.
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                && attempt < self.max_429_retries
            {
                // Clamped to what is left, so the sleep cannot outlive the budget and
                // the next iteration's check is the one that reports it.
                let delay = (self.backoff_base * 2u32.pow(attempt)).min(remaining);
                warn!(
                    attempt = attempt + 1,
                    max_attempts = self.max_429_retries,
                    ?delay,
                    "Embedder returned 429 (busy); backing off and retrying."
                );
                if let Some(c) = &self.retries_429 {
                    c.inc();
                }
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
            let bytes = tokio::select! {
                _ = token.cancelled() => return Err(EncodeError::Cancelled),
                body = response.bytes() => body?,
            };
            return parse_encode_response(&bytes);
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

    /// The 429-retry budget the test client is built with (was the `MAX_429_RETRIES`
    /// const before it moved to config).
    const TEST_MAX_429_RETRIES: u32 = 3;

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
                            // Empty-but-valid binary body: magic + n=0 + dim=1024.
                            let mut body = ENCODE_MAGIC.to_vec();
                            body.extend_from_slice(&0u32.to_le_bytes());
                            body.extend_from_slice(&1024u32.to_le_bytes());
                            body.into_response()
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
            max_429_retries: TEST_MAX_429_RETRIES,
            backoff_base: Duration::from_millis(1), // keep the test fast
            health_timeout: Duration::from_secs(2),
            encode_timeout: Duration::from_secs(5),
            retries_429: None,
        };
        (client, hits)
    }

    fn req() -> BGEm3EmbedRequest {
        BGEm3EmbedRequest {
            texts: vec!["x".into()],
        }
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
        assert!(
            matches!(res, Err(EncodeError::Request(_))),
            "expected give-up, got {res:?}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1 + TEST_MAX_429_RETRIES as usize
        );
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

    #[tokio::test]
    async fn encode_times_out_on_a_wedged_embedder() {
        // A stub that accepts the request and then never responds.
        let app = Router::new().route(
            "/encode",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                "never reached"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = BGEm3HttpClient {
            client: reqwest::Client::new(),
            base_url: Url::parse(&format!("http://{addr}/")).unwrap(),
            max_429_retries: TEST_MAX_429_RETRIES,
            backoff_base: Duration::from_millis(1),
            health_timeout: Duration::from_secs(2),
            encode_timeout: Duration::from_millis(100),
            retries_429: None,
        };

        let started = std::time::Instant::now();
        let res = client.encode(req(), CancellationToken::new()).await;
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

    /// Build a wire-format body by hand (the encoding side `pack_encode` lives in
    /// Python; this mirrors it byte-for-byte to guard the Rust parser).
    fn pack(
        dim: u32,
        dense: &[Vec<f32>],
        sparse: &[Vec<(u32, f32)>],
        colbert: &[Vec<Vec<f32>>],
    ) -> Vec<u8> {
        let n = dense.len() as u32;
        let mut b = ENCODE_MAGIC.to_vec();
        b.extend_from_slice(&n.to_le_bytes());
        b.extend_from_slice(&dim.to_le_bytes());
        for row in dense {
            for v in row {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        for chunk in sparse {
            b.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
            for (id, _) in chunk {
                b.extend_from_slice(&id.to_le_bytes());
            }
            for (_, w) in chunk {
                b.extend_from_slice(&w.to_le_bytes());
            }
        }
        for chunk in colbert {
            b.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
            for tok in chunk {
                for v in tok {
                    b.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
        b
    }

    #[test]
    fn parses_binary_roundtrip_with_ragged_colbert() {
        // 2 chunks, dim=2; ragged ColBERT (1 token vs 3) and varied sparse counts.
        let dense = vec![vec![0.5_f32, -0.5], vec![1.0, 2.0]];
        let sparse = vec![vec![(7_u32, 0.25_f32)], vec![(3, 0.1), (9, 0.9)]];
        let colbert = vec![
            vec![vec![1.0_f32, 1.0]],
            vec![vec![0.0, 1.0], vec![2.0, 2.0], vec![3.0, -3.0]],
        ];
        let buf = pack(2, &dense, &sparse, &colbert);

        let out = parse_encode_response(&buf).expect("valid body should parse");
        assert_eq!(out.dense_vecs, dense);
        assert_eq!(out.colbert_vecs, colbert);
        assert_eq!(out.sparse_vecs[0][&7], 0.25);
        assert_eq!(out.sparse_vecs[1][&9], 0.9);
        assert_eq!(out.sparse_vecs[1].len(), 2);
    }

    #[test]
    fn rejects_bad_magic_and_truncation() {
        let good = pack(2, &[vec![1.0_f32, 2.0]], &[vec![]], &[vec![]]);

        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(matches!(
            parse_encode_response(&bad_magic),
            Err(EncodeError::Decode(_))
        ));

        // Drop the last 4 bytes → the final dense float is now missing.
        assert!(matches!(
            parse_encode_response(&good[..good.len() - 4]),
            Err(EncodeError::Decode(_))
        ));

        // Extra trailing byte → wire-format mismatch.
        let mut trailing = good;
        trailing.push(0);
        assert!(matches!(
            parse_encode_response(&trailing),
            Err(EncodeError::Decode(_))
        ));
    }
}
