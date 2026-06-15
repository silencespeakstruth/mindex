use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

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
}

pub struct BGEm3HttpClient {
    client: reqwest::Client,
    base_url: Url,
}

impl BGEm3HttpClient {
    pub fn new(base_url: Url) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
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

        let request_builder = self.client.post(url).json(&req);

        tokio::select! {
            _ = token.cancelled() => {
                Err(EncodeError::Cancelled)
            }

            res = request_builder.send() => {
                let response = res?;
                let body = response.json::<BGEm3EmbedResponse>().await?;
                Ok(body)
            }
        }
    }
}
