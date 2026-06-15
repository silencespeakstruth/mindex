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
}

/// `files[language][relative_path] = chunk_count`
/// chunk_count == 0 means the file was unchanged (hash match, no re-indexing).
#[derive(Deserialize, Debug)]
pub struct IndexResponse {
    pub files: HashMap<String, HashMap<String, u64>>,
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
