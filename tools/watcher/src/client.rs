use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

#[derive(Serialize)]
pub struct Code {
    pub code: String,
}

#[derive(Serialize)]
struct IndexRequest {
    files: HashMap<String, HashMap<String, Code>>,
}

#[derive(Serialize)]
struct DriftRequest {
    files: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
pub struct DriftResponse {
    pub stale: Vec<String>,
    pub missing: Vec<String>,
    pub orphaned: Vec<String>,
    pub indexing: Vec<String>,
}

#[derive(Serialize)]
struct DeleteSelector {
    paths: Vec<String>,
}

#[derive(Serialize)]
struct DeleteFilesRequest {
    include: DeleteSelector,
}

#[derive(Deserialize)]
struct DeleteFilesResponse {
    deleted_files: u64,
}

/// POST `/{protocol}/{guid}/index`. Returns Ok(()) on success; ignores chunk counts.
pub async fn index_batch(
    client: &Client,
    server: &str,
    protocol: &str,
    guid: &str,
    files: HashMap<String, HashMap<String, Code>>,
    cancel: &CancellationToken,
) -> Result<()> {
    let url = format!(
        "{}/{}/{}/index",
        server.trim_end_matches('/'),
        protocol,
        guid
    );

    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = client.post(&url).json(&IndexRequest { files }).send() => {
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
    Ok(())
}

/// DELETE `/projects/{guid}/files` with an exact-path include selector.
/// Returns the number of files soft-deleted (0 when no paths matched — 204 response).
/// Guards against an empty slice before calling the server (server returns 400 on that).
pub async fn delete_files(
    client: &Client,
    server: &str,
    guid: &str,
    paths: Vec<String>,
    cancel: &CancellationToken,
) -> Result<u64> {
    if paths.is_empty() {
        return Ok(0);
    }

    let url = format!("{}/projects/{}/files", server.trim_end_matches('/'), guid);
    let body = DeleteFilesRequest {
        include: DeleteSelector { paths },
    };

    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = client.delete(&url).json(&body).send() => {
            r.with_context(|| format!("DELETE {url}"))?
        }
    };

    let status = resp.status();
    if status.as_u16() == 499 {
        bail!("cancelled");
    }
    // 204 = selector matched nothing — the files were already gone from the index.
    if status == reqwest::StatusCode::NO_CONTENT {
        return Ok(0);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("server {status}: {body}");
    }

    let parsed = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = resp.json::<DeleteFilesResponse>() => r.context("invalid delete response JSON")?,
    };
    Ok(parsed.deleted_files)
}

/// POST `/projects/{guid}/drift` with the working-tree path→sha256 manifest.
pub async fn check_drift(
    client: &Client,
    server: &str,
    guid: &str,
    manifest: HashMap<String, String>,
    cancel: &CancellationToken,
) -> Result<DriftResponse> {
    let url = format!("{}/projects/{}/drift", server.trim_end_matches('/'), guid);

    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled"),
        r = client.post(&url).json(&DriftRequest { files: manifest }).send() => {
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
        r = resp.json::<DriftResponse>() => r.context("invalid drift response JSON")?,
    };
    Ok(parsed)
}
