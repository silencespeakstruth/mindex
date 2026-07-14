use anyhow::{bail, Context, Result};
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
