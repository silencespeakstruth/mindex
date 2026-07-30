//! Keeps `GET /config`'s `research.models` list current.
//!
//! The list has to come from somewhere that is not the request path: asking Ollama
//! inside `get_config` would put an optional dependency's latency (and its
//! timeouts) on an endpoint every client calls at startup. So one worker asks on a
//! tick, in the house shape (interval + `MissedTickBehavior::Skip` + a cancellation
//! token, tick body split into a clock-free [`refresh_once`]), and the handler reads
//! the snapshot it leaves behind.
//!
//! **A failed tick keeps the previous snapshot** — that is the rule this file exists
//! to get right, and the unit test below is its guard. Ollama is optional and may
//! blip; blanking a client's model picker because one probe timed out is worse than
//! handing it a list a few minutes old. For the same reason the worker is gated on
//! nothing: an Ollama that comes up an hour after mindex must be picked up, and
//! probing first to decide whether to poll would *be* the loop.
//!
//! [`ModelCatalog::refreshed_at`] is not decoration. Without it "Ollama has no
//! models" and "Ollama has never been reached" are the same empty array, and a
//! client cannot tell whether to offer a closed choice or fall back to free text —
//! the distinction `outline`'s `indexed` flag exists for.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::models::ollama::OllamaModel;

/// The published Ollama model registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    /// Model names, as Ollama spells them. Empty until the first tick succeeds.
    pub models: Vec<String>,
    /// Unix seconds of the last **successful** refresh; `None` = never succeeded.
    pub refreshed_at: Option<i64>,
}

/// Shared handle: one writer (this worker), many readers (`get_config`).
///
/// `tokio::sync::RwLock` rather than the std one because the reader is an async
/// handler that must not block, and the std lock would force poison handling —
/// i.e. an `unwrap()` — in a production path. Reads dominate; the write is a
/// wholesale replace every interval. Neither side holds the guard across an
/// `.await`.
pub type SharedCatalog = Arc<tokio::sync::RwLock<ModelCatalog>>;

pub async fn run(
    ollama: Arc<dyn OllamaModel>,
    catalog: SharedCatalog,
    refresh_interval_seconds: u64,
    token: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(refresh_interval_seconds.max(1)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!(
        refresh_interval_seconds,
        "Ollama model catalog: started (the list `GET /config` publishes)."
    );

    // Logged on *transition*, not per tick: a host that simply has no Ollama would
    // otherwise warn forever about an optional dependency.
    let mut degraded = false;

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = token.cancelled() => {
                info!("Ollama model catalog: shutting down.");
                break;
            }
        }

        match refresh_once(ollama.as_ref(), &catalog).await {
            Ok(n) => {
                if degraded {
                    degraded = false;
                    info!(models = n, "Ollama model catalog: reachable again.");
                }
            }
            Err(e) => {
                if !degraded {
                    degraded = true;
                    warn!(
                        error = %e,
                        "Ollama model catalog: could not read the model registry; keeping the \
                         previously published list. Research still works if the model named in \
                         a request exists. Check Ollama is running and that \
                         [research].ollama_url points at it (a container reaches the host at \
                         host.docker.internal, not 127.0.0.1)."
                    );
                }
            }
        }
    }
}

/// One refresh. Returns how many models were published, or the error that left the
/// previous snapshot standing.
///
/// Split out of the loop so the "a failed tick changes nothing" rule is testable
/// without a clock — the `gc::collect` / `metrics::collect_once` precedent.
pub(crate) async fn refresh_once(
    ollama: &dyn OllamaModel,
    catalog: &tokio::sync::RwLock<ModelCatalog>,
) -> Result<usize, crate::models::ollama::OllamaError> {
    let models = ollama.list_models().await?;
    let n = models.len();
    *catalog.write().await = ModelCatalog {
        models,
        refreshed_at: Some(crate::unix_now()),
    };
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ollama::{
        ChatDelta, ChatMessage, ChatOutcome, OllamaError, Sampling, ToolSpec,
    };
    use async_trait::async_trait;

    /// Answers `list_models` from a script; every other method is unreachable here.
    struct Registry(Result<Vec<String>, ()>);

    #[async_trait]
    impl OllamaModel for Registry {
        async fn chat_stream(
            &self,
            _model: &str,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _sampling: Sampling,
            _on_delta: &mut (dyn FnMut(ChatDelta) + Send),
            _token: &CancellationToken,
        ) -> Result<ChatOutcome, OllamaError> {
            unreachable!("the catalog worker does not chat")
        }

        async fn list_models(&self) -> Result<Vec<String>, OllamaError> {
            self.0
                .clone()
                .map_err(|()| OllamaError::Decode("registry down".to_string()))
        }
    }

    #[tokio::test]
    async fn a_successful_tick_publishes_the_list_and_stamps_the_time() {
        let catalog = tokio::sync::RwLock::new(ModelCatalog::default());
        let ok = Registry(Ok(vec!["a".to_string(), "b".to_string()]));

        assert_eq!(refresh_once(&ok, &catalog).await.unwrap(), 2);

        let published = catalog.read().await.clone();
        assert_eq!(published.models, vec!["a".to_string(), "b".to_string()]);
        assert!(published.refreshed_at.is_some());
    }

    #[tokio::test]
    async fn a_failed_tick_keeps_the_previous_list_and_its_timestamp() {
        let catalog = tokio::sync::RwLock::new(ModelCatalog::default());
        refresh_once(&Registry(Ok(vec!["a".to_string()])), &catalog)
            .await
            .unwrap();
        let before = catalog.read().await.clone();

        assert!(refresh_once(&Registry(Err(())), &catalog).await.is_err());

        // Not cleared, and not re-stamped: a client's picker must not empty because
        // one probe failed, and `refreshed_at` must keep saying how old the list is.
        assert_eq!(catalog.read().await.clone(), before);
    }

    #[tokio::test]
    async fn an_empty_registry_is_published_as_empty_but_stamped() {
        let catalog = tokio::sync::RwLock::new(ModelCatalog::default());
        assert_eq!(
            refresh_once(&Registry(Ok(vec![])), &catalog).await.unwrap(),
            0
        );

        // The whole point of the timestamp: this is "Ollama has no models", which a
        // client must be able to tell from "Ollama was never reached".
        let published = catalog.read().await.clone();
        assert!(published.models.is_empty());
        assert!(published.refreshed_at.is_some());
    }
}
