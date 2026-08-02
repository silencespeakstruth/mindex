use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::backend::metrics::Metrics;

/// Operational tuning for the Ollama chat client (all from `[research]` config).
#[derive(Debug, Clone, Copy)]
pub struct OllamaTuning {
    /// **Ceiling** on `options.num_ctx`. The window actually requested is the
    /// model's own trained length (from `/api/show`) capped by this — the model is
    /// the target, this is only the VRAM guard.
    pub max_num_ctx_tokens: u64,
    /// Whole-turn timeout (connect + full streamed reply). Thinking models can
    /// legitimately think for minutes, so this is generous.
    pub turn_timeout_ms: u64,
    /// How long a turn may stay completely silent — no thinking, no content — before
    /// it is abandoned. Bounds the silent prefix only; `0` disables it.
    pub first_token_timeout_ms: u64,
    /// Liveness-ping timeout for `/health`'s optional Ollama check. Short: a
    /// health probe must not hold the response hostage to a wedged Ollama.
    pub health_timeout_ms: u64,
    /// Below this generation rate (tokens per second of *generation* time) a turn
    /// is logged as contention. `0` disables it, and is the default — the healthy
    /// rate is a fact about a model on a host, not about this code.
    pub slow_turn_tokens_per_second: f64,
}

/// Per-turn generation options, from `[research]` config plus the request's
/// optional `seed`.
///
/// Every field is optional and is **omitted** from the request when `None`, so the
/// model's own Modelfile defaults stay in force — which is the right production
/// default and the wrong setting for comparing models. Those defaults are per
/// model and wildly different (measured on this host: `glm-4.7-flash`
/// temperature 1 / top_p 0.95, `qwen3.6` temperature 1 plus presence_penalty 1.5,
/// `gemma4` top_k 64), so an unpinned bake-off compares Modelfiles and sampling
/// noise rather than models. `seed` is the axis a measurement harness varies to
/// get repetitions that mean something.
///
/// Three of the four are sampling; `num_predict` is not, which is why the struct
/// no longer calls itself that. It is set on the **report turn only**, so every
/// other turn's request body is byte-for-byte what it was before the field
/// existed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<i64>,
    /// Hard ceiling on generated tokens for this turn.
    ///
    /// A **runaway backstop**, not the report's length ceiling — that is the word
    /// count the prompt announces. Ollama cuts at a token, so a tight value would
    /// sever a code fence, fail `validate_report_markdown`, and buy a full-volume
    /// rewrite of the document that just failed. Size it out of reach
    /// (`REPORT_WORDS_TO_TOKENS`) and treat it firing as a defect worth a `warn!`.
    pub num_predict: Option<u64>,
}

impl Sampling {
    /// Write the set fields into an `options` object. Unset fields are left out
    /// entirely rather than sent as a null or a guessed default.
    fn apply(&self, options: &mut serde_json::Map<String, serde_json::Value>) {
        if let Some(t) = self.temperature {
            options.insert("temperature".into(), serde_json::json!(t));
        }
        if let Some(p) = self.top_p {
            options.insert("top_p".into(), serde_json::json!(p));
        }
        if let Some(s) = self.seed {
            options.insert("seed".into(), serde_json::json!(s));
        }
        if let Some(n) = self.num_predict {
            options.insert("num_predict".into(), serde_json::json!(n));
        }
    }
}

/// One function the model may call, in Ollama's `tools` request shape.
///
/// Passing these instead of describing a JSON protocol in the system prompt is
/// what lets each model use *its own* trained tool-call template: the calls then
/// arrive in `message.tool_calls`, a field distinct from `content` and from
/// `thinking`, so a model cannot put its decision in the wrong channel.
#[derive(Serialize, Clone, Debug)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: ToolFunction,
}

#[derive(Serialize, Clone, Debug)]
pub struct ToolFunction {
    pub name: &'static str,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    pub fn function(
        name: &'static str,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function",
            function: ToolFunction {
                name,
                description: description.into(),
                parameters,
            },
        }
    }
}

/// One call the model asked for. Round-tripped: it is deserialized from a reply
/// and serialized back verbatim in the assistant message that precedes the tool
/// results, which is what some chat templates expect to see.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// Present on some servers/models only; echoed back when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub function: CalledFunction,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct CalledFunction {
    pub name: String,
    /// An object, per Ollama's docs — not a JSON string as some APIs send.
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Serialize, Clone, Debug)]
pub struct ChatMessage {
    pub role: &'static str,
    pub content: String,
    /// The calls an assistant turn asked for, replayed into the next request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Which tool produced a `role: "tool"` message's content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain("system", content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::plain("user", content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain("assistant", content)
    }

    /// An assistant turn that asked for tools. The calls must be replayed before
    /// their results, or a template that pairs them up sees results out of thin
    /// air.
    pub fn assistant_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant",
            content: content.into(),
            tool_calls: Some(calls),
            tool_name: None,
        }
    }

    /// One tool's result.
    pub fn tool(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool",
            content: content.into(),
            tool_calls: None,
            tool_name: Some(name.into()),
        }
    }

    fn plain(role: &'static str, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: None,
            tool_name: None,
        }
    }
}

/// One streamed fragment of an in-progress assistant reply. `Thinking` deltas
/// exist only for thinking models (Ollama's `message.thinking` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatDelta {
    Thinking(String),
    Content(String),
}

/// A completed chat turn: the assistant `content` plus what the turn cost.
///
/// The counts come from Ollama's final NDJSON line and are `None` when the
/// server omits them (older versions, or a turn that ended without them) —
/// callers must treat a missing count as "unknown", never as zero, or a token
/// tally silently under-reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatOutcome {
    pub content: String,
    /// Native tool calls this turn asked for, in arrival order. Empty either
    /// because the model is answering, or because its template has no tool
    /// support and it wrote a call into `content` instead — the caller decides
    /// which by looking at the content.
    pub tool_calls: Vec<ToolCall>,
    /// Tokens the server actually evaluated for the prompt (`prompt_eval_count`).
    /// This is the count *after* any silent truncation to `num_ctx`, which is
    /// exactly what makes truncation detectable.
    pub prompt_tokens: Option<u64>,
    /// Tokens generated in the reply (`eval_count`), thinking included.
    pub eval_tokens: Option<u64>,
    /// The `num_ctx` actually requested for this turn — the configured value
    /// clamped to the model's own limit. Reported so callers can size a budget
    /// against the real window instead of the configured one; on a 32k model with a
    /// 64k setting they differ twofold.
    pub num_ctx: u64,
    /// Ollama's own accounting for the turn, in **nanoseconds**, exactly as it
    /// reports it. `None` means the server did not say — never zero, and here the
    /// distinction bites twice as hard as it does for the counts, because a zero
    /// `load_duration` is itself a fact (the model was already resident).
    ///
    /// This exists to tell a slow *model* from a busy *GPU*, which are the same
    /// symptom — a run inching along at a token or two a second — and opposite
    /// diagnoses. One measured run spent 985 seconds at ~1.5 tok/s and read as a
    /// wedged model; nothing in the reply, the log or the metrics could say
    /// otherwise, because none of these numbers were parsed.
    pub load_nanos: Option<u64>,
    pub prompt_eval_nanos: Option<u64>,
    pub eval_nanos: Option<u64>,
    /// Ollama's own end-to-end figure for the turn.
    pub total_nanos: Option<u64>,
    /// Wall clock this client measured around the same turn, in **milliseconds**.
    ///
    /// Not redundant with `total_nanos`, and the only field that can see the thing
    /// this set exists to find: `total_duration` is measured inside Ollama's
    /// handler, so time the request spends queued behind another client's
    /// generation on the same GPU falls entirely outside it. During exactly the
    /// contention we are hunting, Ollama's own numbers look healthy.
    pub wall_ms: u64,
}

impl ChatOutcome {
    /// Tokens generated per second of generation time, or `None` when either half
    /// is unknown or zero.
    ///
    /// Deliberately over `eval_nanos` and not over the wall clock: this is meant to
    /// answer "how fast did the model run *while it had the device*", so that the
    /// queueing it was slow *because of* shows up in [`Self::unaccounted_ms`]
    /// instead of being averaged into the rate and hidden.
    pub fn tokens_per_second(&self) -> Option<f64> {
        let (tokens, nanos) = (self.eval_tokens?, self.eval_nanos?);
        (tokens > 0 && nanos > 0).then(|| tokens as f64 * 1e9 / nanos as f64)
    }

    /// Wall clock this turn took that Ollama did not account for, in milliseconds.
    ///
    /// Named for what it measures, not for what it usually means: it also contains
    /// HTTP, TLS and NDJSON parsing, so a small value is noise. A *large* one is
    /// dominated by queueing — the request sat in front of a busy GPU — but this
    /// field states the measurement and leaves the inference to the reader.
    pub fn unaccounted_ms(&self) -> Option<u64> {
        let total_ms = self.total_nanos? / 1_000_000;
        Some(self.wall_ms.saturating_sub(total_ms))
    }
}

#[derive(Debug)]
pub enum OllamaError {
    Cancelled,
    /// Connect / HTTP-level failure, including the whole-turn timeout.
    Request(reqwest::Error),
    /// The NDJSON stream carried something unparseable, an in-band
    /// `{"error": …}` line (e.g. an unknown model name), or a non-2xx reply —
    /// whose body is read and carried here rather than dropped. `error_for_status`
    /// discards it, and for months that turned Ollama's own explanation of a
    /// failed turn into a bare "500", which is unactionable.
    Decode(String),
    /// The connection was alive and produced nothing for `ms` — no thinking, no
    /// content. Separate from [`Self::Request`] because the diagnosis is different
    /// and specific: the socket did not die, Ollama simply never began answering,
    /// which on a single-GPU host means it is loading (or repeatedly reloading) a
    /// model. Carrying it as its own variant is what lets the run say so.
    Silent {
        ms: u64,
    },
}

impl From<reqwest::Error> for OllamaError {
    fn from(err: reqwest::Error) -> Self {
        Self::Request(err)
    }
}

impl std::fmt::Display for OllamaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OllamaError::Cancelled => write!(f, "cancelled"),
            OllamaError::Request(e) => write!(f, "{e}"),
            OllamaError::Decode(msg) => write!(f, "{msg}"),
            OllamaError::Silent { ms } => write!(
                f,
                "ollama produced no token within {ms} ms — it is most likely still \
                 loading the model (check `journalctl -u ollama` for repeated loads)"
            ),
        }
    }
}

/// The single seam the research loop needs from Ollama: one streamed chat turn.
/// Deltas are pushed into `on_delta` as they arrive (thinking and content
/// separately); the return value carries the complete assistant `content`
/// (thinking excluded) and the turn's token counts. The callback is synchronous
/// — callers forward into an unbounded channel, never block.
#[async_trait]
pub trait OllamaModel: Send + Sync {
    /// One streamed chat turn. `tools` is sent as Ollama's `tools` field, or
    /// omitted entirely when empty — omitting it is the *structural* way to make a
    /// turn tool-free (the report turn), stronger than any instruction.
    /// `sampling` is per-turn rather than per-client because `seed` comes from the
    /// request.
    async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        sampling: Sampling,
        on_delta: &mut (dyn FnMut(ChatDelta) + Send),
        token: &CancellationToken,
    ) -> Result<ChatOutcome, OllamaError>;

    /// The models Ollama has locally (`GET /api/tags`), names only — which is
    /// exactly the set a `/research` request may legally name.
    async fn list_models(&self) -> Result<Vec<String>, OllamaError>;

    /// [`Self::list_models`] with each model's identity attached: blob digest and
    /// the details object (parameter size, quantization, family), for the run
    /// journal. A provided method deriving from `list_models` so every fake keeps
    /// compiling; the HTTP client overrides it to read the fields `/api/tags`
    /// already carries. A default-derived descriptor has no digest — which reads
    /// as "not recorded", never as a wrong one.
    async fn list_model_descriptors(&self) -> Result<Vec<ModelDescriptor>, OllamaError> {
        Ok(self
            .list_models()
            .await?
            .into_iter()
            .map(|name| ModelDescriptor {
                name,
                digest: None,
                details_json: None,
            })
            .collect())
    }

    /// Whether the model declares Ollama's `tools` capability — the pre-flight half
    /// of the `research.model_lacks_tools` diagnosis.
    ///
    /// **Three-valued, and the third value is the load-bearing one.** `Some(false)`
    /// is the only answer a caller may refuse on; `None` means the question could not
    /// be asked (Ollama unreachable, or too old to report capabilities) and must let
    /// the run proceed. The default impl returns `None` so every fake — and any
    /// future non-Ollama backend — keeps working without opting into a refusal it
    /// cannot substantiate.
    ///
    /// `Some(true)` is not a promise: a model can declare `tools` and still have a
    /// template that never emits them, which is why the mid-run symptom check
    /// (`looks_like_tool_call_attempt`) stays. This method only catches the case
    /// that is knowable before a slot is spent.
    async fn supports_tools(&self, _model: &str) -> Option<bool> {
        None
    }

    /// Liveness ping of the Ollama server (used by `GET /health`). Ollama is an
    /// *optional* dependency — a failure here is reported, never fatal.
    ///
    /// It *is* [`Self::list_models`] with the answer thrown away: the model
    /// registry answering is the whole liveness signal, and a second method with
    /// its own copy of the URL and the timeout is how the two would drift.
    async fn health(&self) -> Result<(), OllamaError> {
        self.list_models().await.map(|_| ())
    }
}

/// The slice of `GET /api/tags` this client needs.
///
/// Both fields are `#[serde(default)]` on purpose: `health` is expressed over
/// `list_models`, so a *shape* drift in Ollama's response must degrade to an empty
/// list rather than fail the (optional, never-degrading) `checks.ollama`. Only
/// invalid JSON is an error.
#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Deserialize)]
struct TagEntry {
    #[serde(default)]
    name: String,
    /// The model's blob digest — what makes a mutable tag name comparable across
    /// re-pulls. Missing on old Ollamas: degrade to None, never to an error.
    #[serde(default)]
    digest: Option<String>,
    /// Ollama's details object (parameter size, quantization, family), kept as
    /// raw JSON: the journal stores it whole, nothing in this crate reads inside.
    #[serde(default)]
    details: Option<serde_json::Value>,
}

/// One locally-available model with its identity, for the run journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDescriptor {
    pub name: String,
    pub digest: Option<String>,
    /// The details object serialized back to a JSON string, or `None` when
    /// Ollama sent none.
    pub details_json: Option<String>,
}

/// The slice of `POST /api/show` this client needs.
#[derive(Deserialize)]
struct ShowResponse {
    #[serde(default)]
    model_info: HashMap<String, serde_json::Value>,
    /// What the model declares it can do (`tools`, `thinking`, `vision`, …).
    /// Absent on old Ollamas — hence `default`, and hence [`ShowFacts`] reading
    /// "no `capabilities` key at all" as unknown rather than as "cannot".
    #[serde(default)]
    capabilities: Option<Vec<String>>,
}

/// Ollama's name for the capability a research run is made of.
const TOOLS_CAPABILITY: &str = "tools";

/// What one `/api/show` answers, cached per model for the process.
///
/// One struct rather than two caches because it is one request: asking twice for
/// two fields of the same response is how the two would disagree about a model
/// re-pulled between them.
#[derive(Debug, Clone, Copy, Default)]
struct ShowFacts {
    /// The model's own trained context length, `None` when Ollama does not say.
    context_limit: Option<u64>,
    /// Whether the model declares the `tools` capability. **`None` is not `false`**
    /// — it means Ollama did not answer, or is too old to have the field, and a
    /// missing answer must never refuse a run that would have worked.
    supports_tools: Option<bool>,
}

/// One NDJSON line of `POST /api/chat` with `stream: true`.
#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    message: Option<ChunkMessage>,
    #[serde(default)]
    done: bool,
    /// In-band error (unknown model, OOM, …) — Ollama reports it as a JSON line,
    /// often on an HTTP 200 stream.
    #[serde(default)]
    error: Option<String>,
    /// Both counts ride on the `done: true` line only.
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
    /// Ollama's own timings, nanoseconds, likewise only on the `done` line. Kept in
    /// its units all the way to `ChatOutcome`: converting at the seam would mean
    /// two places knew what a nanosecond was.
    #[serde(default)]
    load_duration: Option<u64>,
    #[serde(default)]
    prompt_eval_duration: Option<u64>,
    #[serde(default)]
    eval_duration: Option<u64>,
    #[serde(default)]
    total_duration: Option<u64>,
}

#[derive(Deserialize)]
struct ChunkMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    thinking: Option<String>,
    /// Arrives in its own chunk, typically just before the `done` line — measured,
    /// not assumed (glm-4.7-flash: chunk 81 of 83).
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

/// Ollama's wording when it cannot read the JSON arguments of a tool call. Measured
/// on this host with `gpt-oss:20b`, which sometimes emits its analysis prose into the
/// same harmony message as the arguments:
///
/// ```text
/// error parsing tool call: raw='We need to understand why … {"query":"insert_batch"}',
/// err=invalid character 'W' looking for beginning of value
/// ```
///
/// Ollama `json.Unmarshal`s the whole string and answers 500 for the turn, before the
/// stream opens — which is what made this recoverable at all (nothing has reached the
/// client yet, so a resend is invisible rather than a duplicated half-reply).
const TOOL_CALL_PARSE_ERROR: &str = "error parsing tool call";

/// Not a tuning knob (see the configuration rule in CLAUDE.md): this is a workaround
/// for an upstream parser defect. The fault is in one sampled reply rather than in
/// the transcript, so a resend at a *different seed* is the fix.
///
/// Five because the arithmetic is lopsided, not because five was tuned: a rejected
/// turn comes back in ~2 s (it fails before generation), while giving up costs the
/// whole run — a minute of tool calls that ends with no report. Measured at 2
/// retries, 3 of 4 failing turns were rescued by the first and one exhausted both.
/// Still not an operator knob: a model that fails every time only fails slower.
const MAX_TOOL_CALL_PARSE_RETRIES: u32 = 5;

pub struct OllamaHttpClient {
    client: reqwest::Client,
    base_url: Url,
    tuning: OllamaTuning,
    /// Two counters that no decorator could reach: both events happen *inside*
    /// one `chat_stream` call and leave no trace in its return value — a resent
    /// turn looks like a slow success, and a trimmed transcript looks like a
    /// normal reply. Set by `with_metrics`; `None` in tests that build a bare
    /// client.
    metrics: Option<Arc<Metrics>>,
    /// `model → what /api/show said about it`. Cached because these are properties
    /// of the model file, asked once per model per process; an unreachable or
    /// unhelpful answer caches as all-`None` rather than being re-asked every turn.
    show_facts: tokio::sync::Mutex<HashMap<String, ShowFacts>>,
}

impl OllamaHttpClient {
    pub fn new(base_url: Url, tuning: OllamaTuning) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            tuning,
            show_facts: tokio::sync::Mutex::new(HashMap::new()),
            metrics: None,
        }
    }

    /// Count tool-call parse retries and silent transcript truncations. A builder
    /// rather than a `new` parameter, so every existing construction site — the
    /// tests included — keeps working unchanged.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// What `/api/show` says about one model, asked once per process.
    ///
    /// `model_info`'s keys are namespaced per architecture
    /// (`glm4moelite.context_length`, `qwen3.context_length`, …), so the context
    /// length is found by suffix rather than named.
    ///
    /// Every failure path yields [`ShowFacts::default`] — all fields `None`, which
    /// each consumer reads as "no hint", never as a negative answer. That is the
    /// difference between an unreachable Ollama costing a run its full window and
    /// an unreachable Ollama refusing the run outright.
    ///
    /// **Only successes are cached.** Caching the failure made one transient blip
    /// permanent for the life of the process: every later run of that model silently
    /// used the configured ceiling instead of the model's own window, and its tool
    /// support stayed unknown, with the first `warn!` as the only evidence. The retry
    /// costs one bounded request per run, which is what the fact is worth.
    async fn show_facts(&self, model: &str) -> ShowFacts {
        if let Some(cached) = self.show_facts.lock().await.get(model) {
            return *cached;
        }
        let url = self.base_url.join("api/show").unwrap(); // join of a literal cannot fail
        let facts = match self
            .client
            .post(url)
            .timeout(Duration::from_millis(self.tuning.health_timeout_ms))
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await
        {
            Ok(resp) => match resp.json::<ShowResponse>().await {
                Ok(show) => ShowFacts {
                    context_limit: show
                        .model_info
                        .iter()
                        .find(|(k, _)| k.ends_with(".context_length"))
                        .and_then(|(_, v)| v.as_u64()),
                    supports_tools: show
                        .capabilities
                        .map(|caps| caps.iter().any(|c| c == TOOLS_CAPABILITY)),
                },
                Err(e) => {
                    warn!(%model, error = %e, "Could not read Ollama /api/show for this model; leaving the configured num_ctx as-is and making no claim about its tool support. The next run re-asks.");
                    return ShowFacts::default();
                }
            },
            Err(e) => {
                warn!(%model, error = %e, "Ollama /api/show is unreachable; leaving the configured num_ctx as-is and making no claim about the model's tool support. The next run re-asks.");
                return ShowFacts::default();
            }
        };
        self.show_facts
            .lock()
            .await
            .insert(model.to_string(), facts);
        facts
    }

    /// The `num_ctx` to actually request: **the model's own trained length**, capped
    /// by the configured ceiling.
    ///
    /// This way round because the model is named per request, so no static setting
    /// can be right for all of them: asking a 32k model for 65k does not buy context
    /// (llama.cpp allocates it and the model degrades past its training length,
    /// silently), while pinning 65k on a 202k model throws away two thirds of the
    /// window for nothing. The ceiling exists only because `num_ctx` allocates KV
    /// cache up front, so an unguarded 262k model could take ~7.5 GiB of VRAM.
    ///
    /// When the model's length is unknowable (old Ollama, odd `model_info`) the
    /// ceiling is used as-is — a missing hint must not fail a run.
    async fn effective_num_ctx(&self, model: &str) -> u64 {
        let ceiling = self.tuning.max_num_ctx_tokens;
        match self.show_facts(model).await.context_limit {
            Some(limit) if limit > ceiling => {
                info!(
                    %model,
                    model_limit = limit,
                    requested = ceiling,
                    "Model's context window exceeds [research].max_num_ctx_tokens; \
                     capping. Raise that ceiling to use the whole window if the GPU \
                     has room for the KV cache."
                );
                ceiling
            }
            Some(limit) => {
                info!(
                    %model,
                    requested = limit,
                    ceiling,
                    "Requesting the model's full context window."
                );
                limit
            }
            None => ceiling,
        }
    }

    /// Ollama trims an over-long prompt to `num_ctx` and streams on as if
    /// nothing happened, so the only symptom is a model that has forgotten the
    /// earlier turns. `prompt_eval_count` is the post-trim size, so a prompt
    /// filling the window is the signal — the one place that silence becomes a
    /// log line.
    /// The request half of one chat turn: build the body, send it, and hand back a
    /// response whose stream has not been touched yet.
    ///
    /// This is where [`TOOL_CALL_PARSE_ERROR`] is absorbed. The retry is safe here
    /// and nowhere later: the 500 arrives before any byte of the reply, so the
    /// caller — and through it the SSE client — never saw the attempt that failed.
    async fn post_chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        sampling: Sampling,
        num_ctx: u64,
        token: &CancellationToken,
    ) -> Result<reqwest::Response, OllamaError> {
        let url = self.base_url.join("api/chat").unwrap(); // join of a literal cannot fail
        let mut sampling = sampling;
        let mut attempts = 0;
        loop {
            let send = self
                .client
                .post(url.clone())
                // One budget for the whole turn: reqwest's timeout covers connect
                // through the end of the streamed body.
                .timeout(Duration::from_millis(self.tuning.turn_timeout_ms))
                .json(&self.chat_body(model, messages, tools, sampling, num_ctx))
                .send();

            let response = tokio::select! {
                _ = token.cancelled() => return Err(OllamaError::Cancelled),
                res = send => res?,
            };
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            // The body is Ollama's own account of what went wrong; reading it is
            // the difference between "500" and a diagnosis.
            let detail = response.text().await.unwrap_or_default();
            if status.as_u16() == 500
                && detail.contains(TOOL_CALL_PARSE_ERROR)
                && attempts < MAX_TOOL_CALL_PARSE_RETRIES
            {
                attempts += 1;
                // The transcript is untouched — the model is told nothing and learns
                // nothing. Only the seed moves, because that is the one thing that
                // failed: a verbatim resend at a pinned seed reproduces the same
                // sampled reply often enough to be useless (measured: it rescued 2
                // of 4 turns, the other 2 failed identically twice).
                sampling.seed = sampling.seed.map(|s| s.wrapping_add(1));
                warn!(
                    %model,
                    attempt = attempts,
                    seed = ?sampling.seed,
                    detail = %detail.chars().take(300).collect::<String>(),
                    "Ollama could not parse the model's tool call; resending the \
                     same transcript at the next seed."
                );
                if let Some(m) = &self.metrics {
                    m.research.parse_retries.inc();
                }
                continue;
            }
            return Err(OllamaError::Decode(format!(
                "ollama returned {status}: {}",
                detail.chars().take(500).collect::<String>()
            )));
        }
    }

    /// Report an abandoned silent turn once, in the one place that knows why.
    ///
    /// A `warn!` rather than a bare error return because this failure is almost never
    /// mindex's: the run will report `ollama.unavailable` to its client, and the line
    /// that says the connection was *alive and mute* — with the model name — is what
    /// points at the right log next.
    fn silent_turn(&self, model: &str, ms: u64) -> OllamaError {
        warn!(
            %model,
            first_token_timeout_ms = ms,
            "Ollama accepted the turn and produced no token at all within the silence \
             window; abandoning it rather than spending the run's budget waiting. The \
             usual cause is a model being loaded or repeatedly evicted and reloaded — \
             check `journalctl -u ollama` for `Load failed` and for other clients \
             asking the same model for a different context size."
        );
        OllamaError::Silent { ms }
    }

    /// One `/api/chat` request body.
    fn chat_body(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        sampling: Sampling,
        num_ctx: u64,
    ) -> serde_json::Value {
        let mut options = serde_json::Map::new();
        options.insert("num_ctx".into(), serde_json::json!(num_ctx));
        sampling.apply(&mut options);
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "options": options,
        });
        if !tools.is_empty() {
            // Absent, not empty: an empty `tools` array is a different thing to
            // some templates, and "no tools exist" is what the report turn means.
            body["tools"] = serde_json::json!(tools);
        }
        body
    }

    fn warn_if_context_exhausted(&self, model: &str, num_ctx: u64, outcome: &ChatOutcome) {
        let Some(prompt) = outcome.prompt_tokens else {
            return;
        };
        if prompt >= num_ctx {
            warn!(
                %model,
                prompt_tokens = prompt,
                num_ctx_tokens = num_ctx,
                "Ollama truncated the research prompt to fit the context window; \
                 the model no longer sees the whole transcript. Raise \
                 [research].max_num_ctx_tokens — but the model's own limit binds \
                 or lower the request's effort."
            );
            // The counter and that `warn!` are the only two symptoms: Ollama trims
            // and streams on, so nothing in the reply says the model stopped
            // seeing its own transcript.
            if let Some(m) = &self.metrics {
                m.research.truncations.inc();
            }
        } else if prompt + outcome.eval_tokens.unwrap_or(0) >= num_ctx {
            warn!(
                %model,
                prompt_tokens = prompt,
                eval_tokens = outcome.eval_tokens.unwrap_or(0),
                num_ctx_tokens = num_ctx,
                "Prompt plus reply filled the Ollama context window; the next \
                 turn of this research run will be truncated. Raise \
                 [research].max_num_ctx_tokens."
            );
        }
    }

    /// Record what this turn's generation rate was, and say so when it is far below
    /// what the host can do.
    ///
    /// Here rather than in the research loop for the same reason
    /// [`Self::warn_if_context_exhausted`] is: this is a per-turn pathology that is
    /// invisible in the returned value, and this is the one place that has the model
    /// name, the metrics handle and the timings at once. The loop would need the
    /// threshold threaded through ten call sites to say the same thing later.
    ///
    /// The wording names the diagnosis on purpose. A run inching along at a token a
    /// second is the *same symptom* as a broken model, a bad prompt and a wedged
    /// server, and the operator who reads this log is about to pick one of those.
    /// On this host the answer is usually the fourth: one GPU, shared with whatever
    /// else the user is running.
    fn warn_if_slow_turn(&self, model: &str, outcome: &ChatOutcome) {
        let Some(rate) = outcome.tokens_per_second() else {
            return;
        };
        if let Some(m) = &self.metrics {
            m.research
                .turn_tokens_per_second
                .get_or_create(&crate::backend::metrics::ModelLabels {
                    model: model.to_string(),
                })
                .observe(rate);
            if let Some(load) = outcome.load_nanos {
                m.research
                    .turn_load_seconds
                    .get_or_create(&crate::backend::metrics::ModelLabels {
                        model: model.to_string(),
                    })
                    .observe(load as f64 / 1e9);
            }
        }
        let floor = self.tuning.slow_turn_tokens_per_second;
        if floor <= 0.0 || rate >= floor {
            return;
        }
        warn!(
            %model,
            tokens_per_second = format!("{rate:.1}"),
            eval_tokens = outcome.eval_tokens.unwrap_or(0),
            load_ms = outcome.load_nanos.unwrap_or(0) / 1_000_000,
            unaccounted_ms = outcome.unaccounted_ms().unwrap_or(0),
            threshold = format!("{floor:.1}"),
            "Ollama generated this turn far below the configured healthy rate for \
             this host. An anomalously low rate is contention — another process \
             holding the GPU, or Ollama evicting and reloading this model (see \
             load_ms) — not a broken model or a bad prompt. The run will be slow \
             and may spend its whole wall-clock budget waiting."
        );
    }
}

#[async_trait]
impl OllamaModel for OllamaHttpClient {
    async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        sampling: Sampling,
        on_delta: &mut (dyn FnMut(ChatDelta) + Send),
        token: &CancellationToken,
    ) -> Result<ChatOutcome, OllamaError> {
        let num_ctx = self.effective_num_ctx(model).await;
        // Started before the request, not after it: the queue this is meant to see
        // forms in front of Ollama, so a clock started once the response headers
        // arrive would miss precisely the wait it exists to measure.
        let wall = std::time::Instant::now();
        // The silence guard spans the request *and* the wait for the first token,
        // because the two are one silence from the caller's side and the stall can
        // land in either: Ollama holds the connection open while it loads, so the
        // response headers themselves may be what never arrives. It is armed here,
        // once, and disarmed by the first delta of any channel — after that a turn is
        // bounded by `turn_timeout_ms` and the run's own deadline, as before.
        let silence = self.tuning.first_token_timeout_ms;
        let silent_until =
            (silence > 0).then(|| tokio::time::Instant::now() + Duration::from_millis(silence));
        let post = self.post_chat(model, messages, tools, sampling, num_ctx, token);
        let response = match silent_until {
            Some(deadline) => match tokio::time::timeout_at(deadline, post).await {
                Ok(r) => r?,
                Err(_) => return Err(self.silent_turn(model, silence)),
            },
            None => post.await?,
        };

        let mut stream = response.bytes_stream();
        let mut buf = String::new();
        let mut content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        // Disarmed by the first delta, not by the first *chunk*: Ollama's stream can
        // open with keep-alive-ish lines carrying an empty message, and a byte that
        // says nothing is not the model answering.
        let mut spoke = false;

        loop {
            let next = tokio_stream::StreamExt::next(&mut stream);
            let chunk = match silent_until.filter(|_| !spoke) {
                Some(deadline) => tokio::select! {
                    _ = token.cancelled() => return Err(OllamaError::Cancelled),
                    chunk = tokio::time::timeout_at(deadline, next) => match chunk {
                        Ok(c) => c,
                        Err(_) => return Err(self.silent_turn(model, silence)),
                    },
                },
                None => tokio::select! {
                    _ = token.cancelled() => return Err(OllamaError::Cancelled),
                    chunk = next => chunk,
                },
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));

            // NDJSON: consume every complete line; a partial line stays buffered.
            while let Some(nl) = buf.find('\n') {
                let line: String = buf.drain(..=nl).collect();
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let parsed: ChatChunk = serde_json::from_str(line).map_err(|e| {
                    OllamaError::Decode(format!("unparseable /api/chat line ({e}): {line:.200}"))
                })?;
                if let Some(err) = parsed.error {
                    return Err(OllamaError::Decode(format!("ollama error: {err}")));
                }
                if let Some(msg) = parsed.message {
                    if let Some(t) = msg.thinking
                        && !t.is_empty()
                    {
                        spoke = true;
                        on_delta(ChatDelta::Thinking(t));
                    }
                    if !msg.content.is_empty() {
                        spoke = true;
                        content.push_str(&msg.content);
                        on_delta(ChatDelta::Content(msg.content));
                    }
                    // A turn may carry several calls, spread over chunks. A tool call
                    // is the model answering as much as prose is — a turn that emits
                    // one and nothing else must not read as silence.
                    spoke |= !msg.tool_calls.is_empty();
                    tool_calls.extend(msg.tool_calls);
                }
                if parsed.done {
                    let outcome = ChatOutcome {
                        content,
                        tool_calls,
                        prompt_tokens: parsed.prompt_eval_count,
                        eval_tokens: parsed.eval_count,
                        num_ctx,
                        load_nanos: parsed.load_duration,
                        prompt_eval_nanos: parsed.prompt_eval_duration,
                        eval_nanos: parsed.eval_duration,
                        total_nanos: parsed.total_duration,
                        wall_ms: wall.elapsed().as_millis() as u64,
                    };
                    self.warn_if_context_exhausted(model, num_ctx, &outcome);
                    self.warn_if_slow_turn(model, &outcome);
                    return Ok(outcome);
                }
            }
        }
        // Stream ended without a `done: true` line — a truncated reply.
        Err(OllamaError::Decode(
            "ollama stream ended without a done marker (truncated reply)".into(),
        ))
    }

    async fn list_models(&self) -> Result<Vec<String>, OllamaError> {
        // `/api/tags` rather than `/`: it proves the model registry answers, not
        // just that something is listening on the port. `health_timeout_ms` because
        // this is that ping — expressing one over the other must not change how long
        // `GET /health` may block.
        let url = self.base_url.join("api/tags").unwrap(); // join of a literal cannot fail
        let tags: TagsResponse = self
            .client
            .get(url)
            .timeout(Duration::from_millis(self.tuning.health_timeout_ms))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(tags
            .models
            .into_iter()
            .map(|m| m.name)
            .filter(|n| !n.is_empty())
            .collect())
    }

    async fn list_model_descriptors(&self) -> Result<Vec<ModelDescriptor>, OllamaError> {
        // The same `/api/tags` read as `list_models`, keeping the fields that
        // identify the artifact behind the name.
        let url = self.base_url.join("api/tags").unwrap(); // join of a literal cannot fail
        let tags: TagsResponse = self
            .client
            .get(url)
            .timeout(Duration::from_millis(self.tuning.health_timeout_ms))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(tags
            .models
            .into_iter()
            .filter(|m| !m.name.is_empty())
            .map(|m| ModelDescriptor {
                name: m.name,
                digest: m.digest.filter(|d| !d.is_empty()),
                details_json: m
                    .details
                    .as_ref()
                    .and_then(|d| serde_json::to_string(d).ok()),
            })
            .collect())
    }

    async fn supports_tools(&self, model: &str) -> Option<bool> {
        self.show_facts(model).await.supports_tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::post;

    /// Serves scripted NDJSON lines for `/api/chat` (plus a live `/api/tags`)
    /// and returns a client.
    async fn stub_ollama(lines: &'static [&'static str]) -> OllamaHttpClient {
        let app = Router::new()
            .route(
                "/api/chat",
                post(move || async move {
                    let mut body = String::new();
                    for l in lines {
                        body.push_str(l);
                        body.push('\n');
                    }
                    body
                }),
            )
            // Two named models and one nameless entry: the payload is load-bearing
            // now that `/config` publishes this list and `health` is expressed over
            // reading it.
            .route(
                "/api/tags",
                axum::routing::get(|| async {
                    r#"{"models":[{"name":"a:latest"},{"name":"b:7b"},{}]}"#
                }),
            )
            .route(
                "/api/show",
                post(|| async {
                    // Namespaced key, as real Ollama reports it.
                    r#"{"model_info":{"testarch.context_length":4096}}"#
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        OllamaHttpClient::new(
            Url::parse(&format!("http://{addr}/")).unwrap(),
            OllamaTuning {
                max_num_ctx_tokens: 4096,
                turn_timeout_ms: 5000,
                first_token_timeout_ms: 0,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 2000,
            },
        )
    }

    async fn run(client: &OllamaHttpClient) -> (Result<ChatOutcome, OllamaError>, Vec<ChatDelta>) {
        let mut deltas = Vec::new();
        let res = client
            .chat_stream(
                "test-model",
                &[ChatMessage::user("hi")],
                &[],
                Sampling::default(),
                &mut |d| deltas.push(d),
                &CancellationToken::new(),
            )
            .await;
        (res, deltas)
    }

    /// An Ollama stub that answers the first `failures` calls to `/api/chat` with
    /// `status` and `body`, then serves a normal one-line reply. Shares the counter
    /// with the caller so a test can assert how many turns actually went out.
    async fn flaky_ollama(
        failures: usize,
        status: axum::http::StatusCode,
        body: &'static str,
    ) -> (
        OllamaHttpClient,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&calls);
        let app = Router::new()
            .route(
                "/api/chat",
                post(move || {
                    let seen = std::sync::Arc::clone(&seen);
                    async move {
                        if seen.fetch_add(1, Ordering::SeqCst) < failures {
                            return (status, body.to_string());
                        }
                        (
                            axum::http::StatusCode::OK,
                            format!(
                                "{}\n",
                                serde_json::json!({
                                    "message": { "content": "recovered" },
                                    "done": true,
                                })
                            ),
                        )
                    }
                }),
            )
            .route("/api/show", post(|| async { r#"{"model_info":{}}"# }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = OllamaHttpClient::new(
            Url::parse(&format!("http://{addr}/")).unwrap(),
            OllamaTuning {
                max_num_ctx_tokens: 4096,
                turn_timeout_ms: 5000,
                first_token_timeout_ms: 0,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 500,
            },
        );
        (client, calls)
    }

    /// Verbatim from the failing run this retry exists for.
    const HARMONY_PARSE_500: &str = r#"{"error":"error parsing tool call: raw='We need to understand why indexing never deletes vectors from Qdrant.{\"query\":\"insert_batch\"}', err=invalid character 'W' looking for beginning of value"}"#;

    #[tokio::test]
    async fn a_tool_call_ollama_could_not_parse_is_retried_and_the_caller_never_sees_it() {
        // The fault is in one sampled reply, not in the transcript, so resending
        // the identical turn is the whole fix. Nothing reached the client before
        // the 500 (the stream had not opened), which is what makes it invisible.
        let (client, calls) = flaky_ollama(
            1,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            HARMONY_PARSE_500,
        )
        .await;
        let (res, deltas) = run(&client).await;
        assert_eq!(res.unwrap().content, "recovered");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        // One reply's worth of deltas — the failed attempt contributed none.
        assert_eq!(deltas.len(), 1);
    }

    /// Like `flaky_ollama`, but keeps every request body it was sent so a test can
    /// compare the retry against the attempt it followed.
    async fn recording_ollama() -> (OllamaHttpClient, RequestLog) {
        let bodies: RequestLog = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&bodies);
        let app = Router::new()
            .route(
                "/api/chat",
                post(move |body: String| {
                    let seen = std::sync::Arc::clone(&seen);
                    async move {
                        let mut log = seen.lock().unwrap();
                        log.push(serde_json::from_str(&body).unwrap());
                        if log.len() == 1 {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                HARMONY_PARSE_500.to_string(),
                            );
                        }
                        (
                            axum::http::StatusCode::OK,
                            "{\"message\":{\"content\":\"ok\"},\"done\":true}\n".to_string(),
                        )
                    }
                }),
            )
            .route("/api/show", post(|| async { r#"{"model_info":{}}"# }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = OllamaHttpClient::new(
            Url::parse(&format!("http://{addr}/")).unwrap(),
            OllamaTuning {
                max_num_ctx_tokens: 4096,
                turn_timeout_ms: 5000,
                first_token_timeout_ms: 0,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 500,
            },
        );
        (client, bodies)
    }

    type RequestLog = std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

    #[tokio::test]
    async fn the_parse_retry_moves_the_seed_and_nothing_else() {
        // Why the seed and not a nudge: the model did not misunderstand the task,
        // it sampled one bad reply. Editing the transcript would teach every later
        // turn a lesson it does not need and change what PROMPT_VERSION describes.
        let (client, bodies) = recording_ollama().await;
        client
            .chat_stream(
                "m",
                &[ChatMessage::user("hi")],
                &[],
                Sampling {
                    seed: Some(7),
                    ..Sampling::default()
                },
                &mut |_| {},
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        let sent = bodies.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["options"]["seed"], 7);
        assert_eq!(sent[1]["options"]["seed"], 8);
        assert_eq!(sent[0]["messages"], sent[1]["messages"]);
    }

    #[tokio::test]
    async fn the_parse_retry_gives_up_rather_than_resending_forever() {
        let (client, calls) = flaky_ollama(
            99,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            HARMONY_PARSE_500,
        )
        .await;
        let (res, _) = run(&client).await;
        match res {
            // The give-up carries Ollama's own words, not a bare status.
            Err(OllamaError::Decode(msg)) => {
                assert!(msg.contains("error parsing tool call"), "{msg}")
            }
            other => panic!("expected a Decode error, got {other:?}"),
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1 + MAX_TOOL_CALL_PARSE_RETRIES as usize
        );
    }

    #[tokio::test]
    async fn other_server_errors_are_not_retried_and_keep_ollamas_explanation() {
        // Only the parser defect is spurious. A 500 for any other reason, or a 404
        // for an unknown model, is a real answer — resending it would just spend
        // the turn timeout twice.
        let (client, calls) = flaky_ollama(
            99,
            axum::http::StatusCode::NOT_FOUND,
            r#"{"error":"model 'nope' not found"}"#,
        )
        .await;
        let (res, _) = run(&client).await;
        match res {
            Err(OllamaError::Decode(msg)) => assert!(msg.contains("not found"), "{msg}"),
            other => panic!("expected a Decode error, got {other:?}"),
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// An Ollama stub that answers `/api/chat` with the `options` object it was
    /// sent, so a test can assert what actually went on the wire.
    async fn echo_options_ollama() -> OllamaHttpClient {
        let app = Router::new()
            .route(
                "/api/chat",
                post(|body: String| async move {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                    let opts = serde_json::to_string(&v["options"]).unwrap();
                    format!(
                        "{}\n",
                        serde_json::json!({
                            "message": { "content": opts },
                            "done": true,
                        })
                    )
                }),
            )
            .route("/api/show", post(|| async { r#"{"model_info":{}}"# }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        OllamaHttpClient::new(
            Url::parse(&format!("http://{addr}/")).unwrap(),
            OllamaTuning {
                max_num_ctx_tokens: 4096,
                turn_timeout_ms: 5000,
                first_token_timeout_ms: 0,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 500,
            },
        )
    }

    async fn options_sent_for(sampling: Sampling) -> serde_json::Value {
        let client = echo_options_ollama().await;
        let outcome = client
            .chat_stream(
                "m",
                &[ChatMessage::user("hi")],
                &[],
                sampling,
                &mut |_| {},
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        serde_json::from_str(&outcome.content).unwrap()
    }

    /// Unset sampling must not appear at all: a `null` or a guessed default would
    /// silently override the model's own Modelfile, which is what the production
    /// default deliberately defers to.
    #[tokio::test]
    async fn unset_sampling_fields_are_absent_from_the_request() {
        let opts = options_sent_for(Sampling::default()).await;
        assert_eq!(opts["num_ctx"], 4096);
        assert!(opts.get("temperature").is_none(), "{opts}");
        assert!(opts.get("top_p").is_none(), "{opts}");
        assert!(opts.get("seed").is_none(), "{opts}");
        // The report turn is the only one that arms this. Its absence everywhere
        // else is what keeps every non-report request byte-for-byte what it was
        // before the field existed.
        assert!(opts.get("num_predict").is_none(), "{opts}");
    }

    /// Pinned sampling is what makes a model comparison a comparison: without it
    /// each model plays at its own Modelfile temperature and the spread between
    /// two runs of one model swamps the gap between two models.
    #[tokio::test]
    async fn set_sampling_fields_reach_ollama_options() {
        let opts = options_sent_for(Sampling {
            temperature: Some(0.2),
            top_p: Some(0.9),
            seed: Some(7),
            num_predict: Some(3600),
        })
        .await;
        assert_eq!(opts["temperature"], 0.2);
        assert_eq!(opts["top_p"], 0.9);
        assert_eq!(opts["seed"], 7);
        assert_eq!(opts["num_predict"], 3600);
        // The window is still decided by the model/ceiling logic, not by sampling.
        // `num_predict` bounds what is *generated* into that window; it is not the
        // window.
        assert_eq!(opts["num_ctx"], 4096);
    }

    #[tokio::test]
    async fn streams_thinking_and_content_separately() {
        let client = stub_ollama(&[
            r#"{"message":{"role":"assistant","thinking":"hmm"},"done":false}"#,
            r#"{"message":{"role":"assistant","content":"Hel"},"done":false}"#,
            r#"{"message":{"role":"assistant","content":"lo"},"done":false}"#,
            r#"{"message":{"role":"assistant","content":""},"done":true}"#,
        ])
        .await;
        let (res, deltas) = run(&client).await;
        assert_eq!(res.unwrap().content, "Hello");
        assert_eq!(
            deltas,
            vec![
                ChatDelta::Thinking("hmm".into()),
                ChatDelta::Content("Hel".into()),
                ChatDelta::Content("lo".into()),
            ]
        );
    }

    #[tokio::test]
    async fn done_line_token_counts_are_captured() {
        let client = stub_ollama(&[
            r#"{"message":{"role":"assistant","content":"hi"},"done":false}"#,
            r#"{"message":{"content":""},"done":true,"prompt_eval_count":1234,"eval_count":56}"#,
        ])
        .await;
        let outcome = run(&client).await.0.unwrap();
        assert_eq!(outcome.prompt_tokens, Some(1234));
        assert_eq!(outcome.eval_tokens, Some(56));
    }

    #[tokio::test]
    async fn missing_token_counts_stay_none_not_zero() {
        let client =
            stub_ollama(&[r#"{"message":{"role":"assistant","content":"hi"},"done":true}"#]).await;
        let outcome = run(&client).await.0.unwrap();
        assert_eq!(outcome.prompt_tokens, None);
        assert_eq!(outcome.eval_tokens, None);
    }

    /// Ollama's own timings, which for the whole life of this client were parsed by
    /// nothing and dropped by serde. They are the only thing that can say whether a
    /// slow run was a slow model or a busy device.
    #[tokio::test]
    async fn a_done_line_carries_the_turns_durations_through_to_the_outcome() {
        let client = stub_ollama(&[
            r#"{"message":{"role":"assistant","content":"hi"},"done":false}"#,
            r#"{"message":{"content":""},"done":true,"eval_count":60,"load_duration":500000000,"prompt_eval_duration":300000000,"eval_duration":3000000000,"total_duration":3900000000}"#,
        ])
        .await;
        let outcome = run(&client).await.0.unwrap();
        assert_eq!(outcome.load_nanos, Some(500_000_000));
        assert_eq!(outcome.prompt_eval_nanos, Some(300_000_000));
        assert_eq!(outcome.eval_nanos, Some(3_000_000_000));
        assert_eq!(outcome.total_nanos, Some(3_900_000_000));
        // 60 tokens over 3 s of generation, measured over generation time so that
        // waiting shows up as unaccounted rather than dragging the rate down.
        assert_eq!(outcome.tokens_per_second(), Some(20.0));
        // Wall clock is measured, not scripted, so only the direction is assertable:
        // it covers the whole request and cannot be less than what Ollama accounts
        // for by more than rounding.
        assert!(outcome.unaccounted_ms().is_some());
    }

    /// A zero `load_duration` is a fact — the model was already resident — so the
    /// unknown case must not be spelled the same way.
    #[tokio::test]
    async fn a_done_line_without_durations_is_unknown_not_zero() {
        let client =
            stub_ollama(&[r#"{"message":{"role":"assistant","content":"hi"},"done":true}"#]).await;
        let outcome = run(&client).await.0.unwrap();
        assert_eq!(outcome.load_nanos, None);
        assert_eq!(outcome.eval_nanos, None);
        assert_eq!(outcome.total_nanos, None);
        assert_eq!(outcome.tokens_per_second(), None);
        assert_eq!(outcome.unaccounted_ms(), None);
    }

    /// The threshold defaults to off, because a healthy rate is a fact about one
    /// model on one host rather than about this code. Off must mean silent — not
    /// "warn on everything below zero", which is how a float default goes wrong.
    #[tokio::test]
    async fn a_slow_turn_threshold_of_zero_never_warns() {
        let client = stub_ollama(&[
            r#"{"message":{"content":"x"},"done":true,"eval_count":2,"eval_duration":10000000000,"total_duration":10000000000}"#,
        ])
        .await;
        // 0.2 tok/s — as slow as anything gets — and the default tuning is 0.0.
        assert_eq!(client.tuning.slow_turn_tokens_per_second, 0.0);
        let outcome = run(&client).await.0.unwrap();
        assert_eq!(outcome.tokens_per_second(), Some(0.2));
    }

    #[tokio::test]
    async fn in_band_error_line_is_surfaced() {
        let client = stub_ollama(&[r#"{"error":"model 'nope' not found"}"#]).await;
        let (res, _) = run(&client).await;
        match res {
            Err(OllamaError::Decode(msg)) => assert!(msg.contains("not found"), "{msg}"),
            other => panic!("expected Decode error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn truncated_stream_without_done_is_an_error() {
        let client =
            stub_ollama(&[r#"{"message":{"role":"assistant","content":"half"},"done":false}"#])
                .await;
        let (res, deltas) = run(&client).await;
        assert!(matches!(res, Err(OllamaError::Decode(_))), "{res:?}");
        // Deltas seen before the truncation were still delivered.
        assert_eq!(deltas, vec![ChatDelta::Content("half".into())]);
    }

    /// The model is the target: a window smaller than the ceiling is what gets
    /// requested, because asking for more than the model was trained for degrades it
    /// silently rather than buying context.
    #[tokio::test]
    async fn the_models_own_window_is_requested_when_it_fits_under_the_ceiling() {
        // The stub reports 4096; the ceiling below is 8192.
        let client = stub_ollama(&[r#"{"message":{"content":"hi"},"done":true}"#]).await;
        let big = OllamaHttpClient::new(
            client.base_url.clone(),
            OllamaTuning {
                max_num_ctx_tokens: 8192,
                turn_timeout_ms: 5000,
                first_token_timeout_ms: 0,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 2000,
            },
        );
        assert_eq!(big.effective_num_ctx("test-model").await, 4096);
        // Cached: the second call must not need the server.
        assert_eq!(
            big.show_facts
                .lock()
                .await
                .get("test-model")
                .unwrap()
                .context_limit,
            Some(4096)
        );
    }

    /// The ceiling is the guard: a model whose window exceeds it gets capped,
    /// because `num_ctx` allocates KV cache up front and VRAM is finite.
    #[tokio::test]
    async fn a_window_larger_than_the_ceiling_is_capped() {
        let client = stub_ollama(&[r#"{"message":{"content":"hi"},"done":true}"#]).await;
        let small = OllamaHttpClient::new(
            client.base_url.clone(),
            OllamaTuning {
                max_num_ctx_tokens: 2048,
                turn_timeout_ms: 5000,
                first_token_timeout_ms: 0,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 2000,
            },
        );
        assert_eq!(small.effective_num_ctx("test-model").await, 2048);
    }

    /// An `/api/show` stub answering one fixed body, for the capability reads.
    async fn show_stub(body: &'static str) -> OllamaHttpClient {
        let app = Router::new().route("/api/show", post(move || async move { body }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        OllamaHttpClient::new(
            Url::parse(&format!("http://{addr}/")).unwrap(),
            OllamaTuning {
                max_num_ctx_tokens: 4096,
                turn_timeout_ms: 5000,
                first_token_timeout_ms: 0,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 2000,
            },
        )
    }

    /// The three answers, and the third is why this is an `Option`.
    ///
    /// `Some(false)` is the only one a caller may refuse a run on. A model that lists
    /// `tools` says so; a model that lists other capabilities and not `tools` says
    /// so too; and an Ollama that reports no capabilities at all — the stub above,
    /// and every Ollama older than the field — says nothing, which must not be read
    /// as "cannot".
    #[tokio::test]
    async fn tool_support_is_read_from_the_model_and_absence_is_not_a_no() {
        let yes = show_stub(r#"{"capabilities":["completion","tools","thinking"]}"#).await;
        assert_eq!(yes.supports_tools("m").await, Some(true));

        // Measured on this host: `qwen2.5vl:7b` reports exactly this.
        let no = show_stub(r#"{"capabilities":["completion","vision"]}"#).await;
        assert_eq!(no.supports_tools("m").await, Some(false));

        let silent = show_stub(r#"{"model_info":{"testarch.context_length":4096}}"#).await;
        assert_eq!(silent.supports_tools("m").await, None);
    }

    /// An Ollama that cannot be reached answers `None`, not `Some(false)` — a
    /// pre-flight check that cannot be performed must never become a refusal.
    #[tokio::test]
    async fn an_unreachable_ollama_makes_no_claim_about_tool_support() {
        // A port nothing is listening on: the request fails, and the failure caches.
        let client = OllamaHttpClient::new(
            Url::parse("http://127.0.0.1:1/").unwrap(),
            OllamaTuning {
                max_num_ctx_tokens: 4096,
                turn_timeout_ms: 5000,
                first_token_timeout_ms: 0,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 500,
            },
        );
        assert_eq!(client.supports_tools("m").await, None);
        // And the ceiling still applies, from the same cached all-`None` answer.
        assert_eq!(client.effective_num_ctx("m").await, 4096);
    }

    #[tokio::test]
    async fn an_unreachable_show_endpoint_falls_back_to_the_ceiling() {
        // Nothing listening: an unknown model window must degrade to "use the
        // ceiling", never to zero or a failure — the chat call is what matters.
        let client = OllamaHttpClient::new(
            Url::parse("http://127.0.0.1:1/").unwrap(),
            OllamaTuning {
                max_num_ctx_tokens: 32768,
                turn_timeout_ms: 5000,
                first_token_timeout_ms: 0,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 500,
            },
        );
        assert_eq!(client.effective_num_ctx("m").await, 32768);
    }

    #[tokio::test]
    async fn health_pings_the_tag_registry() {
        let client = stub_ollama(&[]).await;
        assert!(client.health().await.is_ok());
    }

    #[tokio::test]
    async fn list_models_reads_the_tag_registry() {
        let client = stub_ollama(&[]).await;
        // The nameless entry is dropped rather than offered as an empty model name:
        // `/config` publishes this list for a client to render as a closed choice.
        assert_eq!(
            client.list_models().await.unwrap(),
            vec!["a:latest".to_string(), "b:7b".to_string()]
        );
    }

    #[tokio::test]
    async fn health_fails_when_nothing_listens() {
        let client = OllamaHttpClient::new(
            Url::parse("http://127.0.0.1:1/").unwrap(),
            OllamaTuning {
                max_num_ctx_tokens: 4096,
                turn_timeout_ms: 5000,
                first_token_timeout_ms: 0,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 2000,
            },
        );
        assert!(matches!(
            client.health().await,
            Err(OllamaError::Request(_))
        ));
    }

    #[tokio::test]
    async fn cancelled_token_short_circuits() {
        let client = stub_ollama(&[r#"{"message":{"content":"x"},"done":true}"#]).await;
        let token = CancellationToken::new();
        token.cancel();
        let res = client
            .chat_stream(
                "m",
                &[ChatMessage::user("hi")],
                &[],
                Sampling::default(),
                &mut |_| {},
                &token,
            )
            .await;
        assert!(matches!(res, Err(OllamaError::Cancelled)));
    }

    /// An Ollama that accepts the turn and says nothing — a model being loaded, or
    /// evicted and reloaded under a second client. The socket is healthy, so
    /// `turn_timeout_ms` never fires and, before this guard, the run's own deadline
    /// was the first thing to notice: a whole budget spent producing nothing.
    ///
    /// Real time in small increments, deliberately: `start_paused` would auto-advance
    /// past the guard and prove only that `timeout_at` exists.
    #[tokio::test]
    async fn a_turn_that_never_answers_is_abandoned_long_before_the_turn_timeout() {
        let app = Router::new()
            .route(
                "/api/chat",
                post(|| async {
                    // Longer than the guard, shorter than the test's patience.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    r#"{"message":{"content":"too late"},"done":true}"#
                }),
            )
            .route(
                "/api/show",
                post(|| async { r#"{"model_info":{"testarch.context_length":4096}}"# }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = OllamaHttpClient::new(
            Url::parse(&format!("http://{addr}/")).unwrap(),
            OllamaTuning {
                max_num_ctx_tokens: 4096,
                // The whole point: the dead-socket timeout is far above the silence
                // window, exactly as the config validation requires.
                turn_timeout_ms: 30_000,
                first_token_timeout_ms: 200,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 2000,
            },
        );

        let started = std::time::Instant::now();
        let (res, deltas) = run(&client).await;
        assert!(
            matches!(res, Err(OllamaError::Silent { ms: 200 })),
            "a mute turn must be abandoned as silent, got {res:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the guard must fire on its own window, not on the turn timeout"
        );
        assert!(deltas.is_empty(), "nothing was said: {deltas:?}");
    }

    /// The other half: the guard bounds the silent *prefix* only. A model that starts
    /// answering and then thinks for a long stretch is healthy, and abandoning it
    /// would be the tight-timeout mistake the whole design avoids.
    #[tokio::test]
    async fn the_silence_guard_is_spent_by_the_first_token() {
        let app = Router::new()
            .route(
                "/api/chat",
                post(|| async {
                    let (tx, rx) = tokio::sync::mpsc::channel::<Result<_, std::io::Error>>(2);
                    tokio::spawn(async move {
                        let _ = tx
                            .send(Ok(axum::body::Bytes::from(
                                "{\"message\":{\"thinking\":\"hm\"}}\n",
                            )))
                            .await;
                        // Well past the guard: it must already be disarmed.
                        tokio::time::sleep(Duration::from_millis(600)).await;
                        let _ = tx
                            .send(Ok(axum::body::Bytes::from(
                                "{\"message\":{\"content\":\"answer\"},\"done\":true}\n",
                            )))
                            .await;
                    });
                    axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
                }),
            )
            .route(
                "/api/show",
                post(|| async { r#"{"model_info":{"testarch.context_length":4096}}"# }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = OllamaHttpClient::new(
            Url::parse(&format!("http://{addr}/")).unwrap(),
            OllamaTuning {
                max_num_ctx_tokens: 4096,
                turn_timeout_ms: 30_000,
                first_token_timeout_ms: 200,
                slow_turn_tokens_per_second: 0.0,
                health_timeout_ms: 2000,
            },
        );

        let (res, deltas) = run(&client).await;
        let outcome = res.expect("a turn that spoke must not be abandoned");
        assert_eq!(outcome.content, "answer");
        assert_eq!(
            deltas,
            vec![
                ChatDelta::Thinking("hm".into()),
                ChatDelta::Content("answer".into())
            ]
        );
    }
}
