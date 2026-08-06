use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

#[derive(Serialize)]
pub struct Code {
    pub code: String,
}

/// `files[language][relative_path] = Code { code }`
#[derive(Serialize)]
pub struct IndexRequest {
    pub files: HashMap<String, HashMap<String, Code>>,
    /// Rebuild even when the server's stored hash and derivation versions match.
    pub force: bool,
    /// Rebuild only symbols: no slicing, no embedding, no Qdrant on the server side.
    pub symbols_only: bool,
    /// Re-embed only the stored chunks into the active model's collection: no
    /// slicing, no symbols — the cheap half of a model switch.
    pub vectors_only: bool,
}

/// `files[language][relative_path] = chunk_count`
/// chunk_count == 0 means the file was unchanged (hash match, no re-indexing).
#[derive(Deserialize, Debug)]
pub struct IndexResponse {
    pub files: HashMap<String, HashMap<String, u64>>,
}

/// `POST /projects/{guid}/drift` body: working-tree `path → sha256`.
#[derive(Serialize)]
pub struct DriftRequest {
    pub files: HashMap<String, String>,
}

/// Divergence of the working tree from the index. `indexing` is informational
/// (in-flight, no action); the other three need a reindex / delete.
#[derive(Deserialize, Debug, Default)]
pub struct DriftResponse {
    pub stale: Vec<String>,
    pub missing: Vec<String>,
    pub orphaned: Vec<String>,
    pub indexing: Vec<String>,
}

/// How a commit touched one path. Mirrors the server's `ChangeType`.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct CommitPath {
    pub path: String,
    pub change_type: ChangeType,
    /// Source of a rename or copy. The server enforces the biconditional —
    /// present exactly for `renamed`/`copied` — because a `Some` here on a
    /// modification is the signature of a mis-parsed `--raw -z` stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct CommitEntry {
    pub sha: String,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: i64,
    pub committed_at: i64,
    pub parent_count: usize,
    pub subject: String,
    pub body: String,
    pub paths: Vec<CommitPath>,
}

/// `POST /v0/{guid}/history` body. A full-set replace **within `since`**: the
/// server drops anything inside that window this request does not name, which is
/// what makes a force-push or a rebase need no special handling. Omitting
/// `since` claims to speak for the whole history.
#[derive(Serialize)]
pub struct HistoryRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    pub commits: Vec<CommitEntry>,
}

#[derive(Deserialize, Debug, Default)]
pub struct HistoryResponse {
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
}

/// `POST /{protocol}/{guid}/history` — an ingestion route like `/index`, so
/// unlike `/drift` the URL carries the protocol segment.
pub async fn post_history(
    client: &Client,
    server: &str,
    protocol: &str,
    project: &str,
    request: HistoryRequest,
    cancel: &CancellationToken,
) -> Result<HistoryResponse> {
    let url = format!(
        "{}/{}/{}/history",
        server.trim_end_matches('/'),
        protocol,
        project
    );

    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = client.post(&url).json(&request).send() => {
            r.with_context(|| format!("POST {url}"))?
        }
    };

    let status = resp.status();
    if status.as_u16() == 499 {
        bail!("cancelled");
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("server {status}: {body}");
    }

    let parsed = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = resp.json::<HistoryResponse>() => r.context("invalid response JSON")?,
    };

    Ok(parsed)
}

/// `POST /projects/{guid}/drift`. The drift route is a management endpoint, so the
/// URL has no `{protocol}` segment (unlike `/index`).
pub async fn check_drift(
    client: &Client,
    server: &str,
    project: &str,
    request: DriftRequest,
    cancel: &CancellationToken,
) -> Result<DriftResponse> {
    let url = format!(
        "{}/projects/{}/drift",
        server.trim_end_matches('/'),
        project
    );

    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = client.post(&url).json(&request).send() => {
            r.with_context(|| format!("POST {url}"))?
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("server {status}: {body}");
    }

    let parsed = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = resp.json::<DriftResponse>() => r.context("invalid response JSON")?,
    };

    Ok(parsed)
}

pub async fn upload_batch(
    client: &Client,
    server: &str,
    protocol: &str,
    project: &str,
    request: IndexRequest,
    cancel: &CancellationToken,
) -> Result<IndexResponse> {
    let url = format!(
        "{}/{}/{}/index",
        server.trim_end_matches('/'),
        protocol,
        project
    );

    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = client.post(&url).json(&request).send() => {
            r.with_context(|| format!("POST {url}"))?
        }
    };

    let status = resp.status();

    // 499 = server acknowledged client cancellation
    if status.as_u16() == 499 {
        bail!("cancelled");
    }

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("server {status}: {body}");
    }

    let parsed = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = resp.json::<IndexResponse>() => r.context("invalid response JSON")?,
    };

    Ok(parsed)
}

// ─── Streaming upload (`?stream=yes`) ────────────────────────────────────────
//
// The server reports the pipeline live as SSE: `started`, then per-file
// `prepared`/`skipped`, per-embed-batch `embedded` (the honest chunks-per-second
// source), per-file `indexed`, and exactly one terminal `done`/`error`. The wire
// shape is pinned server-side (`IndexEvent` in `src/backend/v0/models.rs`); this
// reader ignores events and fields it does not know, so a newer server degrades
// to less detail rather than an error.

/// A non-terminal event of a streaming `/index` request, handed to the caller's
/// callback as it arrives. Terminals are folded into the function's return value
/// (`done` → `IndexResponse`, `error` → `Err`).
#[derive(Debug, Clone, PartialEq)]
pub enum IndexStreamEvent {
    Started {
        files: u64,
        symbols_only: bool,
    },
    Prepared {
        path: String,
        chunks: u64,
        symbols: u64,
    },
    Skipped {
        path: String,
        /// `unchanged`, `in_flight` or `cancelled` today; opaque by design so a
        /// new server-side reason displays instead of erroring.
        reason: String,
    },
    Embedded {
        batch_chunks: u64,
        chunks_done: u64,
        chunks_total: u64,
        elapsed_ms: u64,
    },
    Indexed {
        path: String,
        count: u64,
    },
}

/// What `upload_batch_streaming` produced, and over which wire. `streamed: false`
/// means the server answered plain JSON — an older mindex that has no
/// `?stream=yes` simply ignores the unknown query — so the caller knows its
/// callback saw nothing and per-file output still has to come from `response`.
pub struct StreamOutcome {
    pub response: IndexResponse,
    pub streamed: bool,
}

/// Incremental SSE framer: bytes in, complete `(event, data)` frames out.
///
/// Owns a byte buffer rather than a string because a network chunk can split a
/// multi-byte UTF-8 character; decoding happens per complete frame. Frames are
/// separated by a blank line; multi-line `data:` never occurs here (the server
/// serializes each payload with `serde_json::to_string`, which escapes newlines)
/// but is joined per the SSE spec anyway. Keep-alive comment lines (`:`) and
/// unknown fields are ignored.
struct SseFramer {
    buf: Vec<u8>,
}

impl SseFramer {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<(String, String)> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(pos) = find_frame_end(&self.buf) {
            let frame: Vec<u8> = self.buf.drain(..pos + 2).collect();
            let text = String::from_utf8_lossy(&frame[..pos]);
            let mut event = String::from("message");
            let mut data: Vec<&str> = Vec::new();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data.push(rest.trim_start());
                }
            }
            if !data.is_empty() {
                frames.push((event, data.join("\n")));
            }
        }
        frames
    }
}

fn find_frame_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

/// Turn one `(event, data)` frame into a callback invocation or a terminal.
/// Returns `Ok(Some(response))` for `done`, `Err` for `error`, `Ok(None)` for
/// everything else (unknown events and malformed data are skipped, not fatal —
/// killing a live upload over one unreadable progress line helps nobody).
fn dispatch_frame(
    event: &str,
    data: &str,
    on_event: &mut (dyn FnMut(IndexStreamEvent) + Send),
) -> Result<Option<IndexResponse>> {
    #[derive(Deserialize)]
    struct Started {
        files: u64,
        symbols_only: bool,
    }
    #[derive(Deserialize)]
    struct Prepared {
        path: String,
        chunks: u64,
        symbols: u64,
    }
    #[derive(Deserialize)]
    struct Skipped {
        path: String,
        reason: String,
    }
    #[derive(Deserialize)]
    struct Embedded {
        batch_chunks: u64,
        chunks_done: u64,
        chunks_total: u64,
        elapsed_ms: u64,
    }
    #[derive(Deserialize)]
    struct Indexed {
        path: String,
        count: u64,
    }
    #[derive(Deserialize)]
    struct Done {
        files: HashMap<String, HashMap<String, u64>>,
    }
    #[derive(Deserialize)]
    struct ErrorEvent {
        code: String,
        detail: String,
    }

    match event {
        "started" => {
            if let Ok(e) = serde_json::from_str::<Started>(data) {
                on_event(IndexStreamEvent::Started {
                    files: e.files,
                    symbols_only: e.symbols_only,
                });
            }
        }
        "prepared" => {
            if let Ok(e) = serde_json::from_str::<Prepared>(data) {
                on_event(IndexStreamEvent::Prepared {
                    path: e.path,
                    chunks: e.chunks,
                    symbols: e.symbols,
                });
            }
        }
        "skipped" => {
            if let Ok(e) = serde_json::from_str::<Skipped>(data) {
                on_event(IndexStreamEvent::Skipped {
                    path: e.path,
                    reason: e.reason,
                });
            }
        }
        "embedded" => {
            if let Ok(e) = serde_json::from_str::<Embedded>(data) {
                on_event(IndexStreamEvent::Embedded {
                    batch_chunks: e.batch_chunks,
                    chunks_done: e.chunks_done,
                    chunks_total: e.chunks_total,
                    elapsed_ms: e.elapsed_ms,
                });
            }
        }
        "indexed" => {
            if let Ok(e) = serde_json::from_str::<Indexed>(data) {
                on_event(IndexStreamEvent::Indexed {
                    path: e.path,
                    count: e.count,
                });
            }
        }
        "done" => {
            let d = serde_json::from_str::<Done>(data).context("invalid `done` event JSON")?;
            return Ok(Some(IndexResponse { files: d.files }));
        }
        "error" => {
            let e =
                serde_json::from_str::<ErrorEvent>(data).context("invalid `error` event JSON")?;
            bail!("server error {}: {}", e.code, e.detail);
        }
        _ => {}
    }
    Ok(None)
}

/// `POST /{protocol}/{guid}/index?stream=yes`, reporting progress through
/// `on_event` as the server works. Falls back transparently when the server
/// answers plain JSON (an older mindex ignores the unknown query parameter) —
/// see [`StreamOutcome::streamed`].
pub async fn upload_batch_streaming(
    client: &Client,
    server: &str,
    protocol: &str,
    project: &str,
    request: IndexRequest,
    cancel: &CancellationToken,
    on_event: &mut (dyn FnMut(IndexStreamEvent) + Send),
) -> Result<StreamOutcome> {
    let url = format!(
        "{}/{}/{}/index?stream=yes",
        server.trim_end_matches('/'),
        protocol,
        project
    );

    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = client
            .post(&url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&request)
            .send() => {
            r.with_context(|| format!("POST {url}"))?
        }
    };

    let status = resp.status();
    if status.as_u16() == 499 {
        bail!("cancelled");
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("server {status}: {body}");
    }

    let streaming = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));
    if !streaming {
        let parsed = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("cancelled"),
            r = resp.json::<IndexResponse>() => r.context("invalid response JSON")?,
        };
        return Ok(StreamOutcome {
            response: parsed,
            streamed: false,
        });
    }

    // Cancellation contract: dropping `resp` (bailing out of this function)
    // closes the connection, which is precisely how the server is told to cancel.
    let mut resp = resp;
    let mut framer = SseFramer::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("cancelled"),
            c = resp.chunk() => c.context("reading the event stream")?,
        };
        let Some(bytes) = chunk else {
            bail!("event stream ended without a terminal done/error event");
        };
        for (event, data) in framer.push(&bytes) {
            if let Some(response) = dispatch_frame(&event, &data, on_event)? {
                return Ok(StreamOutcome {
                    response,
                    streamed: true,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(frames: Vec<(String, String)>) -> Vec<(String, String)> {
        frames
    }

    #[test]
    fn framer_reassembles_frames_split_mid_utf8() {
        let mut f = SseFramer::new();
        // "тест.rs" split inside the multi-byte 'т'.
        let full = "event: indexed\ndata: {\"path\":\"тест.rs\",\"count\":3}\n\n".as_bytes();
        let (a, b) = full.split_at(20);
        assert!(collect(f.push(a)).is_empty());
        let frames = f.push(b);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, "indexed");
        assert!(frames[0].1.contains("тест.rs"));
    }

    #[test]
    fn framer_ignores_keepalive_comments_and_yields_in_order() {
        let mut f = SseFramer::new();
        let frames = f.push(
            b": keep-alive\n\nevent: started\ndata: {\"files\":2}\n\nevent: embedded\ndata: {}\n\n",
        );
        assert_eq!(
            frames.iter().map(|(e, _)| e.as_str()).collect::<Vec<_>>(),
            ["started", "embedded"]
        );
    }

    #[test]
    fn dispatch_done_returns_the_response_and_error_bails() {
        let mut seen = Vec::new();
        let mut cb = |e: IndexStreamEvent| seen.push(e);

        let done = dispatch_frame(
            "done",
            r#"{"files":{"rust":{"a.rs":7}},"files_indexed":1,"chunks":7,"elapsed_ms":10}"#,
            &mut cb,
        )
        .expect("done parses")
        .expect("done is terminal");
        assert_eq!(done.files["rust"]["a.rs"], 7);

        let err = dispatch_frame("error", r#"{"code":"internal","detail":"boom"}"#, &mut cb)
            .expect_err("error is a failure");
        assert!(err.to_string().contains("internal"));
        assert!(seen.is_empty(), "terminals never reach the callback");
    }

    #[test]
    fn dispatch_forwards_progress_events_and_skips_unknown() {
        let mut seen = Vec::new();
        let mut cb = |e: IndexStreamEvent| seen.push(e);

        for (event, data) in [
            ("started", r#"{"files":1,"symbols_only":false}"#),
            (
                "prepared",
                r#"{"path":"a.rs","language":"rust","chunks":2,"symbols":3}"#,
            ),
            (
                "skipped",
                r#"{"path":"b.rs","language":"rust","reason":"unchanged"}"#,
            ),
            (
                "embedded",
                r#"{"batch_chunks":2,"chunks_done":2,"chunks_total":2,"elapsed_ms":5}"#,
            ),
            ("indexed", r#"{"path":"a.rs","language":"rust","count":2}"#),
            ("brand_new_event", r#"{"x":1}"#),
            ("prepared", "not json at all"),
        ] {
            let terminal = dispatch_frame(event, data, &mut cb).expect("non-terminal");
            assert!(terminal.is_none());
        }

        assert_eq!(
            seen,
            vec![
                IndexStreamEvent::Started {
                    files: 1,
                    symbols_only: false,
                },
                IndexStreamEvent::Prepared {
                    path: "a.rs".into(),
                    chunks: 2,
                    symbols: 3,
                },
                IndexStreamEvent::Skipped {
                    path: "b.rs".into(),
                    reason: "unchanged".into(),
                },
                IndexStreamEvent::Embedded {
                    batch_chunks: 2,
                    chunks_done: 2,
                    chunks_total: 2,
                    elapsed_ms: 5,
                },
                IndexStreamEvent::Indexed {
                    path: "a.rs".into(),
                    count: 2,
                },
            ]
        );
    }
}
