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
