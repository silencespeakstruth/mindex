//! The `/research` loop: a local Ollama thinking model iteratively queries the
//! index (one question per turn) and finishes with a Markdown report.
//!
//! The loop is isolated behind two seams so it is testable without Ollama,
//! Qdrant or the embedder: [`OllamaModel`] (the chat turns) and
//! [`ResearchTools`] (the index lookups — production impl wraps
//! `search_core`/`symbols_core`). Events flow to the SSE handler through an
//! unbounded channel; a closed channel or a cancelled token both mean the
//! client is gone and the loop stops quietly.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::backend::error::ApiError;
use crate::backend::v0::handlers::GREP_MIN_PATTERN_CHARS;
use crate::backend::v0::models::{
    CallDirection, CallersResponse, GrepResponse, ListFilesResponse, OutlineResponse,
    ReadChunksResponse, SearchFilter, SearchRequest, SearchResult, SymbolRoleFilter,
    SymbolsRequest, SymbolsResponse,
};
use crate::models::ollama::{
    ChatDelta, ChatMessage, ChatOutcome, OllamaError, OllamaModel, Sampling, ToolCall, ToolSpec,
};

/// Identifies the instructions a run was driven by, stamped onto the `done`
/// event and the per-run log record.
///
/// Two reports produced by different prompts are not comparable, and without this
/// nothing on the wire or in the log says which prompt produced which — so a
/// measurement corpus silently mixes generations and a regression looks like model
/// variance. Cheap to carry, impossible to reconstruct after the fact.
///
/// **Bump on any edit to** `system_prompt`, [`PLAN_REQUEST`],
/// [`SUFFICIENCY_REQUEST`], the re-open nudge that follows it,
/// [`REVALIDATION_SYSTEM_PROMPT`], [`format_citation_complaint`],
/// [`format_ungrounded_complaint`], [`REPORT_SYSTEM_PROMPT`], either report turn's
/// user message, the
/// budget-exhausted nudges, or [`tool_specs`] — anything that changes what the
/// model is asked or what it may call. The run-state note counts too, but only its
/// wording ([`format_state_note`]'s labels): its *contents* are the run's own
/// history and differ every run by design. Not a version of the *code*: refactors
/// that leave the wording identical leave this alone.
///
/// `MAJOR.MINOR`, the notation documented on
/// [`CHUNKS_DERIVATION_VERSION`](crate::slicing::traits::CHUNKS_DERIVATION_VERSION).
/// Nothing ever compares this one — it is pure provenance — so the split between
/// the two numbers is the only thing that gives it meaning: MINOR for reworded
/// instructions, MAJOR for a run that asks the model to do a different job.
pub const PROMPT_VERSION: &str = "1.0";

/// How many extra results a prefixed `search` fetches before filtering.
///
/// The prefix cannot be pushed into the query (see `execute`), so recall under it
/// depends entirely on how deep the unfiltered ranking is read. Four times the
/// requested width is the compromise: enough that a prefix naming a real
/// subdirectory usually still fills `search_top_k`, cheap because the extra cost
/// is Qdrant's, not the embedder's — the query vector is computed once either way.
const PREFIX_OVERFETCH: usize = 4;
/// Rows per role per `symbols` tool call.
const SYMBOLS_LIMIT: usize = 10;
/// Re-asks allowed when a reply carries no parseable action; after that, finalize.
///
/// Two, not one, because the common cause is recoverable and specific: a thinking
/// model puts its JSON in the *thinking* channel and leaves `content` empty (the
/// action never reaches `parse_action`, which only sees content). The re-ask below
/// names that mistake, and naming it is worth a second attempt. Parsing the action
/// out of the thinking instead would be worse than it looks — thinking holds every
/// candidate the model considered and discarded, so picking one (the last, say)
/// reliably replays a call it already made and burns the duplicate budget.
const MAX_PARSE_RETRIES: usize = 2;
/// Rejected duplicate calls tolerated before the model is forced to finalize.
///
/// A rejection deliberately consumes no tool budget (nothing was executed), so
/// this cap is the *only* thing bounding it: a model that keeps re-asking the
/// same question would otherwise loop forever — every turn gets a fresh
/// `turn_timeout_ms`, there is no aggregate time budget, and there is no cancel
/// endpoint, so two stubborn jobs would permanently hold both
/// `[research].max_concurrent` slots.
const MAX_DUPLICATE_CALLS: usize = 3;

/// Report turns re-asked after the model returned an empty `final` channel.
///
/// Measured on `gpt-oss:20b`: 4 runs in a 36-run sweep ended with `content` empty
/// after 1157-2273 generated tokens — the report had been written into the analysis
/// channel and never moved to `final`. Retrying is cheap (the transcript is
/// unchanged and nothing was streamed, so only one turn is repeated) against a run
/// that is otherwise wholly wasted, which is why the cap is generous rather than
/// minimal. Not configurable, for the same reason as the parse retry below it: a
/// model that never fills `final` only fails more slowly if this is raised.
const MAX_EMPTY_REPORT_RETRIES: u32 = 5;

/// Times the tool loop may be re-entered after the sufficiency check found a
/// planned sub-question still unanswered.
///
/// The loop's termination rests on counters, not on the clock, and re-entry is a
/// `continue` at the phase level — so it needs a bound of its own, exactly like
/// `duplicate_calls`. One, not more: the re-entry exists to catch the model that
/// declared victory early (measured: `gemma4:12b` finalizes at a median of 4
/// steps and scores 34%), and a second pass over the same gap is the rephrasing
/// loop again at a coarser grain. Every budget axis is still checked on re-entry,
/// so a run with nothing left to spend never takes it.
const MAX_REOPENS: usize = 1;

/// Executed tool calls allowed in the citation-revalidation phase.
///
/// Small on purpose: the phase exists to let the model *read* a location it cited
/// but never opened, which is one `read_chunks` or `outline` per bad citation, not
/// a second investigation. Runs stopped by a budget skip the phase's tools
/// entirely — there is nothing left to spend — and correct or drop the claim
/// instead.
const MAX_REVALIDATION_STEPS: usize = 4;

/// Model turns allowed in the revalidation phase, whatever they produce.
///
/// `MAX_REVALIDATION_STEPS` bounds the executed calls; this bounds the turns that
/// execute nothing (a repeat, an unmappable call, prose), which would otherwise
/// spin for the same reason `MAX_DUPLICATE_CALLS` exists.
const MAX_REVALIDATION_TURNS: usize = 3;

/// Entries per list in the run-state note. Enough to be a memory, short enough
/// that the note cannot grow with the transcript it is meant to summarise.
const STATE_NOTE_MAX_ITEMS: usize = 12;

/// Notes the model may keep at once.
///
/// Deliberately double `STATE_NOTE_MAX_ITEMS`: the other lists are a *history* — what
/// was already asked, so it is not asked again — and truncating one loses nothing but
/// an old query. Notes are the run's compressed conclusions, the only thing the model
/// wrote that survives a turn, so they are the last thing to drop. At the cap the
/// oldest is dropped and the drop is announced, because a memory that forgets
/// silently is worse than one with a known size.
const MAX_NOTES: usize = 24;

/// Characters per note. A note is a conclusion; at this length it cannot become a
/// second copy of the evidence, which is already in the transcript and would be paid
/// for twice — once in the note and again on every later turn that resends it.
const MAX_NOTE_CHARS: usize = 500;

/// Research depth requested by the client, mapped to a tool-call budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

/// What one run may spend. Resolved from `[research].effort.<level>` and then
/// overridden field-by-field by the request's optional `budget` — these are tuning
/// knobs, so the levels live in config with defaults and validation rather than as
/// `match` arms on [`Effort`].
///
/// Four axes with **different jobs**, stopping at whichever is reached first, and
/// `done.reason` says which:
///
/// - `max_seconds` is the budget — it is what the caller waits for.
/// - `max_tokens` is the *cost* axis. Every turn resends the whole transcript, so
///   the token sum grows super-linearly with turns and is what the run actually
///   costs the GPU.
/// - `max_steps` is a backstop, nothing more. A step is not a unit of anything:
///   `outline` is one indexed SELECT while `search` is a GPU embed plus a vector
///   query, and one turn may ask for several (measured: 16 steps over 20 turns).
/// - `context_fraction` is a **guard**, not a lever — it exists so a small-window
///   model stops before Ollama silently trims the transcript. Measured at the
///   deepest setting, the largest prompt was ~12k against a 65k window, so it is
///   not what ends a normal run. It is deliberately not per-request overridable:
///   raising it buys nothing but truncation.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub max_seconds: u64,
    /// Local tokens (prompt + eval, summed over turns) the investigation may spend.
    pub max_tokens: u64,
    pub context_fraction: f64,
    pub max_steps: usize,
    /// Chunks one `search` call returns. Not a budget axis — nothing stops on it —
    /// but it rides along because it is per-effort config the loop needs, and it is
    /// validated against `[search].max_top_k` at startup (research builds its
    /// `SearchRequest` directly and so never meets the request validator).
    pub search_top_k: u64,
}

impl Budget {
    /// An effort preset with the request's overrides applied, axis by axis.
    ///
    /// A partial override keeps the preset for every axis it does not name, so
    /// `{"max_seconds": 60}` shortens the run without silently deepening anything
    /// else. `context_fraction` never comes from the request — see the struct docs.
    ///
    /// The overrides are range-checked at the edge (`validate::research_budget`),
    /// so by the time they get here they are within `[research].max_request_*`.
    pub fn resolve(
        preset: &crate::config::EffortBudget,
        over: Option<crate::backend::v0::models::ResearchBudgetOverride>,
    ) -> Budget {
        let over = over.unwrap_or_default();
        Budget {
            max_seconds: over.max_seconds.unwrap_or(preset.max_seconds),
            max_tokens: over.max_tokens.unwrap_or(preset.max_tokens),
            context_fraction: preset.context_fraction,
            max_steps: over.max_steps.unwrap_or(preset.max_steps),
            search_top_k: preset.search_top_k,
        }
    }
}

/// Why the loop stopped asking questions.
///
/// Without this, a report cut short by the budget and one the model considered
/// complete arrive identically, and a consumer (scout, the VS Code view) has to
/// decide whether to follow up on vibes. Each variant corresponds to exactly one
/// `break` in the tool loop. The `as_str` values are wire values — part of the
/// SSE contract, so treat them like `ApiError` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneReason {
    /// The model chose to finalize — it considered the evidence sufficient.
    Finalized,
    /// The tool budget (`effort` → `max_steps`) ran out first.
    BudgetExhausted,
    /// The model failed to produce a parseable action twice, so it was forced
    /// to write the report on whatever evidence it had.
    Unparseable,
    /// The model kept repeating calls it had already made (`MAX_DUPLICATE_CALLS`).
    RepeatedCalls,
    /// The wall-clock budget for the investigation ran out.
    TimeExhausted,
    /// The local-token budget ran out — the run had read and generated as many
    /// tokens as it was allowed. This is the cost axis `max_steps` only pretended
    /// to be.
    TokensExhausted,
    /// A prompt reached the allowed fraction of the context window. Continuing
    /// would let Ollama silently truncate the transcript, which reads as a model
    /// that forgot what it found.
    ContextExhausted,
}

impl DoneReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DoneReason::Finalized => "finalized",
            DoneReason::BudgetExhausted => "budget_exhausted",
            DoneReason::Unparseable => "unparseable",
            DoneReason::RepeatedCalls => "repeated_calls",
            DoneReason::TimeExhausted => "time_exhausted",
            DoneReason::TokensExhausted => "tokens_exhausted",
            DoneReason::ContextExhausted => "context_exhausted",
        }
    }

    /// Whether the report was cut short rather than finished on purpose.
    pub fn is_truncated(self) -> bool {
        self != DoneReason::Finalized
    }
}

/// The budget axis closest to exhaustion — i.e. the one that will end this run.
///
/// One field rather than four ratios recomputed in every client, and the answer a
/// human actually wants while watching a run: *what is this run going to run out
/// of?* On a finished run it is the retrospective version of the same question,
/// and it should agree with `done.reason` whenever a budget ended the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    Time,
    Tokens,
    Steps,
    Context,
}

impl Binding {
    pub fn as_str(self) -> &'static str {
        match self {
            Binding::Time => "time",
            Binding::Tokens => "tokens",
            Binding::Steps => "steps",
            Binding::Context => "context",
        }
    }
}

/// What a run has spent so far, against what it may spend.
///
/// Carried by both the `progress` and the `done` event: the same numbers in the
/// same shape, so a client renders one meter and a measurement harness reads the
/// run's cost off the stream instead of reconstructing it from server logs it
/// cannot see for traffic it did not initiate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunProgress {
    pub steps: usize,
    pub max_steps: usize,
    pub elapsed_ms: u64,
    pub max_ms: u64,
    /// `prompt_tokens + eval_tokens` — what the run has cost the GPU.
    pub tokens: u64,
    pub max_tokens: u64,
    pub prompt_tokens: u64,
    pub eval_tokens: u64,
    /// The largest single prompt so far: how close the transcript came to the
    /// window, which is what decides whether evidence was silently dropped.
    pub peak_prompt_tokens: u64,
    /// The window in use (0 until a turn has reported it).
    pub num_ctx: u64,
    /// Model turns completed, including the report turn once it happens. Diverges
    /// from `steps` on purpose — see [`Budget`].
    pub turns: usize,
}

impl RunProgress {
    /// Share of `num_ctx` the largest prompt reached, 0.0 until a turn reports one.
    pub fn context_pct(&self) -> f64 {
        if self.num_ctx == 0 {
            return 0.0;
        }
        (self.peak_prompt_tokens as f64 * 100.0) / self.num_ctx as f64
    }

    /// The axis with the largest consumed share. Ties resolve towards the primary
    /// budget (time first, context — a guard, not a budget — last).
    pub fn binding(&self, context_fraction: f64) -> Binding {
        let ratio = |used: f64, max: f64| if max > 0.0 { used / max } else { 0.0 };
        let context_ceiling = self.num_ctx as f64 * context_fraction;
        let axes = [
            (
                Binding::Time,
                ratio(self.elapsed_ms as f64, self.max_ms as f64),
            ),
            (
                Binding::Tokens,
                ratio(self.tokens as f64, self.max_tokens as f64),
            ),
            (
                Binding::Steps,
                ratio(self.steps as f64, self.max_steps as f64),
            ),
            (
                Binding::Context,
                ratio(self.peak_prompt_tokens as f64, context_ceiling),
            ),
        ];
        axes.into_iter()
            .fold((Binding::Time, f64::MIN), |best, axis| {
                if axis.1 > best.1 { axis } else { best }
            })
            .0
    }

    fn to_json(self, context_fraction: f64) -> Value {
        json!({
            "steps": self.steps,
            "max_steps": self.max_steps,
            "elapsed_ms": self.elapsed_ms,
            "max_ms": self.max_ms,
            "tokens": self.tokens,
            "max_tokens": self.max_tokens,
            "prompt_tokens": self.prompt_tokens,
            "eval_tokens": self.eval_tokens,
            "peak_prompt_tokens": self.peak_prompt_tokens,
            "num_ctx": self.num_ctx,
            "context_pct": (self.context_pct() * 10.0).round() / 10.0,
            "turns": self.turns,
            "binding": self.binding(context_fraction).as_str(),
        })
    }
}

/// One SSE event of a research stream. `name()`/`data()` define the wire shape,
/// which is mirrored in four places that must move together: `post_research`'s
/// doc comment, its OpenAPI 200 description, `tools/vscode` and scout's SSE
/// reader — the last two ignore what they don't recognise, so a field added here
/// and nowhere else is simply never seen.
#[derive(Debug, Clone, PartialEq)]
pub enum ResearchEvent {
    /// A delta of the model's thinking (thinking models only).
    Thinking { text: String },
    /// One executed tool call.
    Step {
        n: usize,
        call: StepCall,
        hits: usize,
    },
    /// Budget consumption, so a live run is steerable instead of opaque. Emitted
    /// once before the first turn (limits, zero spent), then after every executed
    /// step and every completed turn. Deliberately **not** on a timer: a ticker
    /// would be a second task racing the cancellation token for a number the
    /// client can interpolate itself between events.
    Progress {
        progress: RunProgress,
        context_fraction: f64,
    },
    /// A delta of the final Markdown report.
    Summary { text: String },
    /// How many of the report's `path:start-end` citations the run's own tool
    /// results support. Emitted once, after the report and before `done`.
    ///
    /// `report` always describes the report the client was actually shown.
    /// `revalidation` is present only when that report is a *corrected* one, and
    /// says what the draft had got wrong — without it, a clean report and a
    /// repaired one look identical on the wire.
    Citations {
        report: CitationReport,
        revalidation: Option<Revalidation>,
    },
    /// The run's final state and full cost. Carries every `progress` field, so a
    /// consumer that only reads `done` still gets the whole record.
    Done {
        progress: RunProgress,
        context_fraction: f64,
        reason: DoneReason,
    },
    /// A failure after the stream started (the HTTP status is already 200).
    Error { code: String, detail: String },
}

/// The tool call a `step` event describes.
///
/// Typed rather than a `&'static str` plus a `String`: the wire shape gives each
/// action its own argument key (`query`/`name`/`path`/`glob`), and deciding which
/// by matching on an action *string* put the same list in two places — the arm
/// that built the event and the arm that named its key. Adding a tool then meant
/// remembering both, with a silent `"query"` fallback if you didn't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepCall {
    Search { query: String },
    Symbols { name: String },
    Outline { path: String },
    Callers { name: String },
    ListFiles { glob: String },
    ReadChunks { path: String },
    Grep { pattern: String },
    Note { text: String },
    RevisePlan { plan: String },
}

impl StepCall {
    /// The wire `action` value.
    pub fn action(&self) -> &'static str {
        match self {
            StepCall::Search { .. } => "search",
            StepCall::Symbols { .. } => "symbols",
            StepCall::Outline { .. } => "outline",
            StepCall::Callers { .. } => "callers",
            StepCall::ListFiles { .. } => "list_files",
            StepCall::ReadChunks { .. } => "read_chunks",
            StepCall::Grep { .. } => "grep",
            StepCall::Note { .. } => "note",
            StepCall::RevisePlan { .. } => "revise_plan",
        }
    }

    /// The argument's wire key and value — one pair per action, by construction.
    fn argument(&self) -> (&'static str, &str) {
        match self {
            StepCall::Search { query } => ("query", query),
            StepCall::Symbols { name } => ("name", name),
            StepCall::Outline { path } => ("path", path),
            StepCall::Callers { name } => ("name", name),
            StepCall::ListFiles { glob } => ("glob", glob),
            StepCall::ReadChunks { path } => ("path", path),
            StepCall::Grep { pattern } => ("pattern", pattern),
            StepCall::Note { text } => ("text", text),
            StepCall::RevisePlan { plan } => ("plan", plan),
        }
    }
}

impl ResearchEvent {
    pub fn name(&self) -> &'static str {
        match self {
            ResearchEvent::Thinking { .. } => "thinking",
            ResearchEvent::Step { .. } => "step",
            ResearchEvent::Progress { .. } => "progress",
            ResearchEvent::Summary { .. } => "summary",
            ResearchEvent::Citations { .. } => "citations",
            ResearchEvent::Done { .. } => "done",
            ResearchEvent::Error { .. } => "error",
        }
    }

    pub fn data(&self) -> Value {
        match self {
            ResearchEvent::Thinking { text } => json!({ "text": text }),
            ResearchEvent::Step { n, call, hits } => {
                // One argument key per action, named for what it is; scout's step
                // whitelist and the VS Code renderer both key on these.
                let (arg_key, argument) = call.argument();
                json!({
                    "n": n,
                    "action": call.action(),
                    arg_key: argument,
                    "hits": hits,
                })
            }
            ResearchEvent::Progress {
                progress,
                context_fraction,
            } => progress.to_json(*context_fraction),
            ResearchEvent::Summary { text } => json!({ "text": text }),
            ResearchEvent::Citations {
                report,
                revalidation,
            } => json!({
                "total": report.total,
                "verified": report.verified,
                "path_only": report.path_only,
                "unverified": report.unverified,
                "unverified_paths": report.unverified_paths,
                // Freshness, beside provenance: how many of the report's citations
                // point into a file the index rewrote (or dropped) after this run
                // read it. A reader that spot-checks nothing else should check these.
                "stale": report.stale,
                "stale_paths": report.stale_paths,
                // Flat rather than nested: both consumers whitelist the keys they
                // render and silently drop the rest, so a nested object would have
                // to be taught as a shape instead of as three names. Null when the
                // draft needed no correction.
                "draft_unverified": revalidation.map(|r| r.draft_unverified),
                "draft_path_only": revalidation.map(|r| r.draft_path_only),
                "draft_stale": revalidation.map(|r| r.draft_stale),
                "revalidation_steps": revalidation.map(|r| r.steps),
            }),
            ResearchEvent::Done {
                progress,
                context_fraction,
                reason,
            } => {
                // `steps`/`elapsed_ms` stay top-level: they were the original
                // contract, and the cost fields are added around them.
                let mut data = progress.to_json(*context_fraction);
                if let Value::Object(map) = &mut data {
                    map.insert("reason".into(), Value::String(reason.as_str().into()));
                    // Which instructions produced this report. Two reports written
                    // under different prompts are not comparable, and nothing else
                    // on the stream says which was in force.
                    map.insert(
                        "prompt_version".into(),
                        Value::String(PROMPT_VERSION.into()),
                    );
                }
                data
            }
            ResearchEvent::Error { code, detail } => json!({ "code": code, "detail": detail }),
        }
    }
}

/// The index lookups the loop may perform. Production impl wraps
/// `search_core`/`symbols_core`; tests fake it.
#[async_trait]
pub trait ResearchTools: Send + Sync {
    async fn search(
        &self,
        req: SearchRequest,
        token: &CancellationToken,
    ) -> Result<Vec<SearchResult>, ApiError>;

    async fn symbols(
        &self,
        req: SymbolsRequest,
        token: &CancellationToken,
    ) -> Result<SymbolsResponse, ApiError>;

    /// One file's definitions, in source order. Out of scope is reported as such
    /// (`in_scope: false`), never as an empty outline.
    async fn outline(
        &self,
        path: String,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<OutlineResponse, ApiError>;

    /// The approximate call graph around one exact name, read off `parent_name`.
    /// Lexical, so the edges are exact only up to name collision. Call sites outside
    /// the scope are dropped and counted, not silently omitted.
    async fn callers(
        &self,
        name: String,
        direction: CallDirection,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<CallersResponse, ApiError>;

    /// Indexed paths matching a glob, within the run's scope.
    async fn list_files(
        &self,
        glob: String,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<ListFilesResponse, ApiError>;

    /// Exact literal search over the indexed chunk text — what `search` cannot do,
    /// because it matches meaning rather than bytes.
    async fn grep(
        &self,
        pattern: String,
        glob: Option<String>,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<GrepResponse, ApiError>;

    /// The indexed code covering a line range of one file. Out of scope is reported
    /// as such, like `outline`.
    async fn read_chunks(
        &self,
        path: String,
        start_line: usize,
        end_line: usize,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<ReadChunksResponse, ApiError>;

    /// The index's current identity for a set of paths — the freshness probe.
    ///
    /// **Not a model-facing tool**: no [`Action`] variant, no [`StepCall`], no entry
    /// in `tool_specs`, no `step` event, and it costs no budget axis. The model
    /// never asks for this; the loop asks on its own behalf, between turns, so it
    /// can tell the model which of the files it has read have moved underneath it.
    /// A path with no row back has left the index.
    async fn file_versions(
        &self,
        paths: Vec<String>,
        token: &CancellationToken,
    ) -> Result<Vec<FileVersion>, ApiError>;
}

/// Metrics decorator over [`ResearchTools`].
///
/// Gives per-tool call counts and latency across all seven model-facing tools
/// plus the freshness probe, without touching `execute` or the tool loop — which
/// matters because `execute` is one `match` over `Action` and adding a timer to
/// each arm is exactly the kind of edit that gets half-applied.
///
/// `NoMatch` is its own outcome rather than an error: `execute` treats it as a
/// *finding* ("no results"), and folding it into `error` would make the tool
/// error rate read as broken when the run is merely searching for something that
/// is not there.
pub struct MeteredResearchTools {
    inner: Arc<dyn ResearchTools>,
    metrics: Arc<crate::backend::metrics::Metrics>,
}

impl MeteredResearchTools {
    pub fn new(
        inner: Arc<dyn ResearchTools>,
        metrics: Arc<crate::backend::metrics::Metrics>,
    ) -> Self {
        Self { inner, metrics }
    }

    fn record<T>(&self, tool: &'static str, started: Instant, result: &Result<T, ApiError>) {
        let outcome = match result {
            Ok(_) => "ok",
            Err(ApiError::NoMatch) => "no_match",
            Err(_) => "error",
        };
        let labels = crate::backend::metrics::ToolOutcomeLabels { tool, outcome };
        let r = &self.metrics.research;
        r.tool_calls.get_or_create(&labels).inc();
        r.tool_duration
            .get_or_create(&labels)
            .observe(started.elapsed().as_secs_f64());
    }
}

#[async_trait]
impl ResearchTools for MeteredResearchTools {
    async fn search(
        &self,
        req: SearchRequest,
        token: &CancellationToken,
    ) -> Result<Vec<SearchResult>, ApiError> {
        let t = Instant::now();
        let r = self.inner.search(req, token).await;
        self.record("search", t, &r);
        r
    }

    async fn symbols(
        &self,
        req: SymbolsRequest,
        token: &CancellationToken,
    ) -> Result<SymbolsResponse, ApiError> {
        let t = Instant::now();
        let r = self.inner.symbols(req, token).await;
        self.record("symbols", t, &r);
        r
    }

    async fn outline(
        &self,
        path: String,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<OutlineResponse, ApiError> {
        let t = Instant::now();
        let r = self.inner.outline(path, scope, token).await;
        self.record("outline", t, &r);
        r
    }

    async fn callers(
        &self,
        name: String,
        direction: CallDirection,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<CallersResponse, ApiError> {
        let t = Instant::now();
        let r = self.inner.callers(name, direction, scope, token).await;
        self.record("callers", t, &r);
        r
    }

    async fn list_files(
        &self,
        glob: String,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<ListFilesResponse, ApiError> {
        let t = Instant::now();
        let r = self.inner.list_files(glob, scope, token).await;
        self.record("list_files", t, &r);
        r
    }

    async fn grep(
        &self,
        pattern: String,
        glob: Option<String>,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<GrepResponse, ApiError> {
        let t = Instant::now();
        let r = self.inner.grep(pattern, glob, scope, token).await;
        self.record("grep", t, &r);
        r
    }

    async fn read_chunks(
        &self,
        path: String,
        start_line: usize,
        end_line: usize,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<ReadChunksResponse, ApiError> {
        let t = Instant::now();
        let r = self
            .inner
            .read_chunks(path, start_line, end_line, scope, token)
            .await;
        self.record("read_chunks", t, &r);
        r
    }

    async fn file_versions(
        &self,
        paths: Vec<String>,
        token: &CancellationToken,
    ) -> Result<Vec<FileVersion>, ApiError> {
        let t = Instant::now();
        let r = self.inner.file_versions(paths, token).await;
        // Labelled like the rest even though the model never asks for it: it is
        // the one tool call the *loop* makes, and its cost belongs in the same
        // family so "what did this run spend on tools" is one query.
        self.record("file_versions", t, &r);
        r
    }
}

/// One file's identity in the index, as of a probe.
///
/// `sha256` is the hash the *index* holds, not the working tree's: research reads
/// the index, so the only change that matters to a run is a change that has been
/// indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
    pub path: String,
    pub sha256: String,
    /// A reindex of this file is in flight (`status='indexing'`). Worth its own
    /// flag rather than folding into "changed": while it holds, the file's chunks
    /// are in the `has_id` candidate set but their vectors may not be in Qdrant
    /// yet, so `search` can silently under-retrieve it — while `read_chunks`,
    /// `outline` and `symbols` (pure SQL) still work.
    pub in_flight: bool,
}

/// The set of files a run may see, enforced on **every** model-facing lookup.
///
/// One struct rather than two loose `Option<SearchFilter>` parameters, and that is
/// the point of it: before it existed, `symbols`, `outline`, `callers` and
/// `read_chunks` had nowhere to put a scope, so a run the caller had scoped to
/// `docs/**` could still read any file in the project by naming it. The scope was
/// enforced on retrieval (`search`) and enumeration (`list_files`) and nowhere else —
/// which is a scope in the documentation and not in the server. With the scope as a
/// required argument, a tool added later cannot quietly be the next exception.
///
/// Evaluated in SQLite, by `build_file_filter`, for a reason worth keeping: the server
/// crate has no glob matcher of its own (`globset` lives in `tools/mindexfile`), so an
/// in-process check would introduce a fifth glob dialect into a codebase that has
/// already paid for having four.
#[derive(Debug, Clone, Default)]
pub struct ToolScope {
    pub include: Option<SearchFilter>,
    pub exclude: Option<SearchFilter>,
}

impl ToolScope {
    /// Whether anything is actually restricted. Guards the extra work — an unscoped
    /// run must build exactly the SQL it always did, so that the public endpoints
    /// sharing these cores are provably unaffected.
    pub fn is_scoped(&self) -> bool {
        fn any(f: &Option<SearchFilter>) -> bool {
            f.as_ref().is_some_and(|f| {
                f.paths.as_ref().is_some_and(|p| !p.is_empty())
                    || f.programming_languages
                        .as_ref()
                        .is_some_and(|l| !l.is_empty())
            })
        }
        any(&self.include) || any(&self.exclude)
    }

    /// The scope in one line, for the model.
    ///
    /// Rendered in exactly one place and used in three — the system prompt, the
    /// run-state note and every out-of-scope refusal — so the walls are always
    /// described the same way. An empty scope renders as the whole project rather
    /// than as nothing, because "no restriction" is a fact the model should be able
    /// to read.
    pub fn describe(&self) -> String {
        fn part(out: &mut Vec<String>, label: &str, f: &Option<SearchFilter>) {
            let Some(f) = f else { return };
            if let Some(paths) = f.paths.as_ref().filter(|p| !p.is_empty()) {
                let list: Vec<String> = paths.iter().map(|p| format!("`{}`", p.0)).collect();
                out.push(format!("{label} paths {}", list.join(", ")));
            }
            if let Some(langs) = f.programming_languages.as_ref().filter(|l| !l.is_empty()) {
                let list: Vec<&str> = langs.iter().map(|l| l.name()).collect();
                out.push(format!("{label} languages {}", list.join(", ")));
            }
        }
        let mut parts = Vec::new();
        part(&mut parts, "only", &self.include);
        part(&mut parts, "never", &self.exclude);
        if parts.is_empty() {
            "the whole project".to_string()
        } else {
            parts.join("; ")
        }
    }
}

/// Everything one research job needs.
pub struct ResearchParams {
    pub question: String,
    pub model: String,
    /// The files this run may see. Passed to every lookup, not just to `search`.
    pub scope: ToolScope,
    pub budget: Budget,
    /// Sampling for every turn of this run: `[research]` config with the request's
    /// `seed` applied over it.
    pub sampling: Sampling,
    /// How long the report phase gets once the investigation ends
    /// (`[research].report_timeout_ms`).
    ///
    /// Not part of [`Budget`] on purpose: it is not an axis the run spends against
    /// and not something a request may override. It is the operator's bound on the
    /// *tail* of a run — which is why `budget.max_seconds + report_timeout_ms` is
    /// the longest a caller can wait.
    pub report_timeout_ms: u64,
    /// Where to record a pathology that happens *inside* one turn and leaves no trace
    /// in its return value — the runaway-thinking abandonment below. The same reason
    /// the embedder's 429 retries and Ollama's tool-call-parse retries are counted in
    /// place rather than at a seam: from outside, three abandoned turns and three
    /// ordinary empty replies are the same thing.
    ///
    /// `Option` so every test constructs a params value without a metric registry, as
    /// the `with_metrics` seams do.
    pub metrics: Option<Arc<crate::backend::metrics::Metrics>>,
    /// Abandon a turn once its thinking channel has streamed this many characters;
    /// `0` disables the guard (`[research].max_turn_thinking_chars`).
    ///
    /// Not a budget axis and deliberately not request-overridable, for the reason
    /// `context_fraction` is not: nothing good lies on either side of the default.
    /// Raising it buys a longer wedge, lowering it buys abandoned healthy turns, and
    /// the caller holds no information the server lacks — the number depends on the
    /// model, and the model is already the server's to look up.
    pub max_turn_thinking_chars: usize,
}

/// The model's reply each turn must be exactly one of these, as a JSON object.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
enum Action {
    Search {
        query: String,
        /// Optional: keep only hits whose path starts with this. Applied as a
        /// *post-filter* over a widened result set, never by adding to the run's
        /// `include` — see `execute`.
        #[serde(default)]
        path_prefix: Option<String>,
    },
    Symbols {
        name: String,
        #[serde(default)]
        role: Option<SymbolRoleFilter>,
        /// Optional: the tags.scm syntax type (`function`, `method`, `class`, …).
        /// A free string, not an enum — the vocabulary is upstream query data and
        /// differs per language, so validating it here would reject labels that
        /// are perfectly real for some grammar.
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        anchor_path: Option<String>,
    },
    Outline {
        path: String,
    },
    /// The approximate call graph, read off `parent_name` — see `callers_core`.
    Callers {
        name: String,
        #[serde(default)]
        direction: Option<CallDirection>,
    },
    // `rename_all = "lowercase"` would make this `listfiles`; the prompt (and
    // every model that has seen a JSON tool API) says `list_files`.
    #[serde(rename = "list_files")]
    ListFiles {
        glob: String,
    },
    #[serde(rename = "read_chunks")]
    ReadChunks {
        path: String,
        start_line: usize,
        end_line: usize,
    },
    /// Exact literal search over the indexed chunk text.
    Grep {
        pattern: String,
        /// Optional: only files matching this SQLite `GLOB`.
        #[serde(default)]
        glob: Option<String>,
    },
    /// A conclusion the model wants to still have next turn.
    Note {
        text: String,
    },
    // As with `list_files`, `rename_all = "lowercase"` would spell this
    // `reviseplan`.
    #[serde(rename = "revise_plan")]
    RevisePlan {
        plan: String,
    },
    Finalize,
}

/// The tools offered to the model, mirroring [`Action`].
///
/// These *are* the protocol now: each model expresses a call through its own
/// trained template and Ollama hands it back in `message.tool_calls`. Descriptions
/// still carry the retrieval lesson (identifiers beat prose), because the model
/// reads them when choosing.
fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::function(
            "search",
            "Semantic search over the project's indexed code. Returns the top matching \
             chunks with path and line span. Matches TEXT, so a query carrying real \
             identifiers finds implementations, while a plain-English question tends to \
             return the tests that describe the behaviour. One question per call.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "One focused question, or code terms/identifiers."
                    },
                    "path_prefix": {
                        "type": "string",
                        "description": "Optional: keep only hits under this path prefix, \
                                        e.g. \"src/db/\". Use it when you know where the \
                                        answer lives and the query keeps returning \
                                        elsewhere."
                    }
                },
                "required": ["query"]
            }),
        ),
        ToolSpec::function(
            "symbols",
            "Where an EXACT identifier is defined and referenced, with kinds and \
             enclosing scopes. Use it once you know a name.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Exact, case-sensitive identifier." },
                    "role": {
                        "type": "string",
                        "enum": ["definition", "reference"],
                        "description": "Optional: restrict to definitions or references."
                    },
                    "kind": {
                        "type": "string",
                        "description": "Optional: restrict to one syntax kind, e.g. \"function\", \
                                        \"method\", \"class\", \"call\". Useful to separate a \
                                        function from a type of the same name. The labels come \
                                        from tree-sitter and are NOT uniform across languages (a \
                                        Rust struct and enum both read \"class\"), so use it to \
                                        narrow a noisy result, not to reason about the language. \
                                        The kinds a file actually uses are visible in outline."
                    },
                    "anchor_path": {
                        "type": "string",
                        "description": "Optional: rank candidates in this file first."
                    }
                },
                "required": ["name"]
            }),
        ),
        ToolSpec::function(
            "callers",
            "The call graph around an EXACT identifier: which definitions reference it \
             (direction \"in\", the default), or what the definition of that name \
             references (direction \"out\"). Answers \"who uses this\" and \"what does \
             this depend on\" in one call, instead of reading every hit by hand. \
             IMPORTANT: these edges are LEXICAL — they match the name only and are not \
             resolved, so a common name like \"new\" or \"get\" mixes unrelated \
             definitions and a renamed import is missed. Treat the result as candidates \
             and confirm with read_chunks.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Exact, case-sensitive identifier." },
                    "direction": {
                        "type": "string",
                        "enum": ["in", "out"],
                        "description": "\"in\" (default): who references this name. \
                                        \"out\": what the definition of this name references."
                    }
                },
                "required": ["name"]
            }),
        ),
        ToolSpec::function(
            "outline",
            "Every definition in one file, in source order, with line ranges and the \
             file's language. The fastest way to learn the real names in a file — do \
             this before searching for a concept. Note the `kind` labels come from \
             tree-sitter and are not uniform across languages (a Rust struct or enum \
             both show as \"class\"), so trust the name, not the label's syntax.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repo-relative path." }
                },
                "required": ["path"]
            }),
        ),
        ToolSpec::function(
            "list_files",
            "Indexed paths matching a glob. `*` matches across directories here, so \
             \"*research*\" finds src/research.rs and \"src/*\" finds everything under src/.",
            json!({
                "type": "object",
                "properties": {
                    "glob": { "type": "string", "description": "Glob over repo-relative paths." }
                },
                "required": ["glob"]
            }),
        ),
        ToolSpec::function(
            "read_chunks",
            "The indexed code covering a line range of one file — use it to actually \
             READ a location `symbols` or `outline` gave you, instead of searching for \
             its line numbers. Coverage is by indexed chunk and is sparse: very short \
             definitions (imports, consts, small helpers) have no chunk, and the reply \
             says so rather than pretending the lines are empty.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repo-relative path." },
                    "start_line": { "type": "integer", "description": "First line, 1-based." },
                    "end_line": { "type": "integer", "description": "Last line, inclusive." }
                },
                "required": ["path", "start_line", "end_line"]
            }),
        ),
        ToolSpec::function(
            "grep",
            "EXACT literal search over the indexed code — the opposite of `search`, \
             which matches meaning and therefore cannot find a specific string. Use it \
             for an error code, a config key, a magic number, a log message, a string \
             literal, or any token `symbols` does not know because the language's tags \
             query never tagged it. Case-insensitive substring match; wildcards are NOT \
             interpreted, the pattern is taken literally. At least 3 characters.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "The literal text to find, at least 3 characters."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Optional: only files matching this glob, e.g. \"src/*\"."
                    }
                },
                "required": ["pattern"]
            }),
        ),
        ToolSpec::function(
            "note",
            "Write down a conclusion you want to still have later. YOUR REASONING IS \
             NOT KEPT: everything you think is discarded after each turn, and only tool \
             results and these notes survive. So when you work something out — how a \
             mechanism actually functions, which of two candidates is the real one, a \
             dead end not worth revisiting — record it here in one or two sentences, \
             with the `path:line` it rests on. Write conclusions, not evidence; the \
             evidence is already above. Notes are shown back to you every turn and are \
             available when you write the report.",
            json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "One or two sentences. A conclusion, with its location."
                    }
                },
                "required": ["text"]
            }),
        ),
        ToolSpec::function(
            "revise_plan",
            "Replace your plan with a corrected one. Use it when the investigation has \
             shown the original plan asked the wrong questions — a sub-question that \
             turned out to be irrelevant, or a real one you did not foresee. The new \
             plan replaces the old entirely, so repeat the parts still worth answering; \
             it is what you will be held to at the end.",
            json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "string",
                        "description": "The full revised plan, as a numbered list of sub-questions."
                    }
                },
                "required": ["plan"]
            }),
        ),
        ToolSpec::function(
            "finalize",
            "Call this when the evidence suffices; you will then be asked for the \
             final report. You may also simply answer in prose instead of calling any \
             tool, which means the same thing.",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
}

/// Turn a native tool call into an [`Action`].
///
/// `Action` is `#[serde(tag = "action")]`, so a call is just its arguments object
/// with the function name injected — one deserializer for both the native and the
/// fallback path, and the same error messages.
fn action_from_call(call: &ToolCall) -> Option<Action> {
    let mut obj = match &call.function.arguments {
        Value::Object(map) => map.clone(),
        // Some templates send no arguments at all for a zero-arg tool.
        Value::Null => serde_json::Map::new(),
        _ => return None,
    };
    obj.insert(
        "action".to_string(),
        Value::String(call.function.name.clone()),
    );
    Action::deserialize(Value::Object(obj)).ok()
}

/// Case- and whitespace-insensitive form of a search query, for the exact-repeat
/// key. `"GC sweep"`, `"gc  sweep"` and `" gc sweep "` are one call, not three.
fn normalize_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Similarity above which two search queries count as the same call.
///
/// A token-set Jaccard, deliberately crude: it must catch "fn research_inner
/// implementation" vs "fn research_inner impl" (the measured failure) without
/// needing a model or an embedding.
///
/// 0.5 also rejects a *mild refinement* — one word added or swapped — and that is
/// deliberate rather than a limitation accepted: refining instead of learning a
/// name is precisely the loop the model gets trapped in, and the rejection
/// message tells it what to do instead. A false positive is cheap by
/// construction: the call costs no step, only one of `MAX_DUPLICATE_CALLS` turns.
const NEAR_DUPLICATE_JACCARD: f64 = 0.5;
/// Below this many distinct tokens, similarity is noise — a one- or two-word
/// query overlapping another by a single token is not a rephrasing.
const NEAR_DUPLICATE_MIN_TOKENS: usize = 3;

/// Whether `candidate` is a rephrasing of `previous`.
fn is_near_duplicate(previous: &str, candidate: &str) -> bool {
    let tokens = |s: &str| -> HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let (a, b) = (tokens(previous), tokens(candidate));
    if a.len() < NEAR_DUPLICATE_MIN_TOKENS || b.len() < NEAR_DUPLICATE_MIN_TOKENS {
        return false;
    }
    let intersection = a.intersection(&b).count() as f64;
    let union = a.union(&b).count() as f64;
    union > 0.0 && intersection / union >= NEAR_DUPLICATE_JACCARD
}

/// Whether a reply *looks like* an attempt to call a tool in prose — a JSON object
/// carrying a `name` or `action` key.
///
/// This is a diagnostic, not a parser. mindex speaks Ollama's native tool-calling
/// API; a model whose template has no tool support emits its call as text instead,
/// and the only sane response is to say so. Parsing it was worse than useless: it
/// bought a second protocol (its own prompt paragraph, its own shapes, its own
/// branch in the loop) for the *worst*-scoring model in the bake-off, and the name
/// it produced was mangled (`"search Semantic code search over…"`), so
/// accommodating it meant guessing — turning a model error into a successful call
/// with unpredictable arguments.
fn looks_like_tool_call_attempt(content: &str) -> bool {
    for (i, ch) in content.char_indices() {
        if ch != '{' {
            continue;
        }
        let mut iter = serde_json::Deserializer::from_str(&content[i..]).into_iter::<Value>();
        if let Some(Ok(Value::Object(map))) = iter.next()
            && (map.get("action").is_some_and(Value::is_string)
                || map.get("name").is_some_and(Value::is_string))
        {
            return true;
        }
    }
    false
}

/// The tool-loop system prompt.
///
/// It does not describe a JSON wire format: the tools are passed natively
/// (`tool_specs`), so each model uses its own trained template and the mechanics
/// are the server's problem. What remains is the part a model cannot infer — the
/// retrieval strategy, measured on this corpus: identifiers find implementations,
/// plain English finds the tests that describe them.
fn system_prompt(budget: Budget, scope: &ToolScope) -> String {
    format!(
        r#"You are a code-research agent working over "mindex", a semantic index of ONE project's source code. You cannot open files or browse: the tools are your only access to the code. Gather evidence with them, then answer.

READ THIS — it decides whether you succeed. `search` matches *text*, and this project's code is written in identifiers while your questions are in plain English. A plain-English query tends to return the TEST that describes a behaviour, not the code that implements it. The identifier is what finds the implementation, so your first job is to LEARN THE REAL NAMES and only then search for them:

  list_files → outline → (now you have exact names) → symbols / search / callers → read_chunks

That rule governs CODE. The index also contains this project's DOCUMENTATION — `*.md` files: `README.md`, `CLAUDE.md`, the per-tool READMEs — and there the rule inverts, because documentation is written in the same plain English you think in. Ask those questions in plain English, in the words you would use with a colleague. Documentation is where a project states what it does not say in code: why a design was chosen over the alternative, which steps a change must touch, what an invariant is for. When your question is "why", "what are all the places", or "what is the rule for", search the prose FIRST — it is often a single hit against many steps of reading code, and it will hand you the identifiers to search for next. `list_files` with glob `**/*.md` shows you what documentation exists.

`outline`, `list_files` and `callers` are metadata lookups — cheap, and they cost the same as any other step, so spending your first step or two on them pays for itself. When `symbols` or `outline` hands you a location, READ IT with `read_chunks` — do not search for its line numbers, that never works. If you know which directory the answer lives in, pass `path_prefix` to `search` rather than re-asking the same query and hoping.

Once you have an exact name, `callers` answers "who uses this" and "what does this depend on" in ONE call, where searching would cost several and still miss the uses that spell the call differently. Reach for it when the question is about reach, blast radius or dependencies. Its edges are matched by NAME ALONE and are not resolved, so a short or common name will mix in unrelated code — check a site with `read_chunks` before you build an argument on it.

HOW YOUR MEMORY WORKS, because it is not what you expect: your internal reasoning is NOT carried from one turn to the next. Only the tool results and the text you write alongside your calls come back to you. So write one short line with every call — what the last result established, and what this call tests. That line is the only note-to-self you will still have next turn, and without it you will re-derive the same plan from raw output over and over.

For anything you want to KEEP, use `note`. Whenever you settle something — how a mechanism actually works, which of two candidates is the real one, a direction that turned out to be a dead end — record it in one or two sentences with the `path:line` it rests on. Your notes are repeated back to you every turn and are in front of you when you write the report, so they are how a long investigation accumulates instead of drifting. If your plan turns out to have asked the wrong questions, do not abandon it silently: `revise_plan` replaces it, and the replacement is what you will be judged against.

`search` finds code by MEANING and therefore cannot find a specific string. When you want an exact literal — an error code, a config key, a magic number, a log message, a string in quotes — use `grep`. It reads the bytes.

A message headed "Run state" is maintained for you by the server and repeated every turn. It lists what you have already asked and already been shown. Read it before choosing a call; it is there so you never spend a turn rediscovering your own history.

Rules:
- Never repeat a call you already made; repeats are rejected and wasted. Rephrasing the same question is just as wasteful, and is also rejected — if a search disappointed you, do not re-ask it in other words. Get a name (list_files/outline) and search for that instead.
- You may ask for several tools in one turn when they are genuinely independent.
- Your budget is at most {max_steps} tool calls and {max_seconds} seconds, whichever runs out first. The clock is a HARD deadline: when it expires you are cut off mid-thought and whatever you have becomes the report. So do not save the important lookup for later, and call `finalize` — or simply answer in prose — as soon as the evidence suffices. A report you chose to write is worth more than one the clock ended.
- Cite only what a tool actually showed you. Before the report ships, every `path:start-end` in it is checked against the locations your own calls returned, and you will be asked to fix the ones that were not.
{scope_rule}"#,
        max_steps = budget.max_steps,
        max_seconds = budget.max_seconds,
        scope_rule = if scope.is_scoped() {
            format!(
                "\nYOUR SCOPE IS RESTRICTED: this run may only see {}. That restriction is \
                 enforced on EVERY tool, not just on search — a file outside it is REFUSED by \
                 name, not returned empty, so do not read a refusal as \"the file does not \
                 exist\". It was set by whoever started this run and you cannot widen it, so \
                 spend no calls probing for a way around it. Work within the scope; if the \
                 answer genuinely lies outside it, say so plainly in your report.\n",
                scope.describe()
            )
        } else {
            String::new()
        },
    )
}

/// The question put to the model before any tool is offered, whose answer becomes
/// the run's plan.
///
/// It exists because of where a thinking model does its planning. `ChatMessage`
/// carries no `thinking` field and `ChatOutcome` never captures one, so the
/// channel the model reasons in is discarded after every turn — it then
/// re-derives its approach each turn from a growing pile of raw tool output, which
/// is what "looping" looks like from outside. The reply to this question is pushed
/// back as an ordinary **assistant** message, and assistant content *is* replayed.
/// It is the same thought, moved into the one channel that survives.
///
/// Also the run's only sufficiency criterion. Without a list of sub-questions,
/// "finalize when the evidence suffices" is unanchored, and the two failure modes
/// it produces are both measured on this corpus: stopping at four steps with a
/// third of the answer, or never stopping at all.
const PLAN_REQUEST: &str = "Before you touch a tool: break this question into 3-6 \
    sub-questions you must answer to write a correct report, and for each one name \
    the artifact you expect to answer it — a file, a symbol, an invariant. Number \
    them. Be concrete and brief: one line each, no preamble, no investigation yet. \
    This list is your plan, and you will be held to it at the end.";

/// Asked after the tool loop ends, before the report is written.
///
/// Its job is not to gather anything — it is to make the gap explicit while the
/// evidence is still in the transcript. A run that answered four of six
/// sub-questions and a run that answered all six are otherwise indistinguishable
/// in the report, and `done.reason` only says whether a budget bound, not whether
/// the question was covered.
const SUFFICIENCY_REQUEST: &str = "The investigation is paused. Go through your \
    numbered plan and, for each sub-question, write one line: the number, then \
    ANSWERED with the `path:start-end` that answers it, or UNANSWERED with what is \
    still missing. Judge only by evidence a tool actually returned in this run — \
    what you already knew about this codebase does not count. No report yet, no \
    tool calls, just the list.";

/// The system prompt for the revalidation phase, when the draft cited locations
/// the run never saw.
///
/// A separate role, like the report prompt, and for the same measured reason: a
/// transcript that has spent its whole length rewarding one reply shape keeps
/// producing it unless told the game changed. Here the change is the opposite
/// direction — the tools are open again, briefly, for one narrow purpose.
const REVALIDATION_SYSTEM_PROMPT: &str = "You are checking your own draft report \
    before it ships. Some of its citations are not backed by anything your tools \
    returned during this run. The tools are open again for a few calls, for that \
    purpose only: read the locations you cited but never opened, so the claim is \
    either confirmed against real code or dropped. Do not start a new \
    investigation and do not write the report yet.";

/// Whether a sufficiency verdict still reports an open item.
///
/// A substring test, and deliberately not more: the vocabulary is one the server
/// dictated ([`SUFFICIENCY_REQUEST`] asks for the literal words), so parsing it
/// into a structure would buy precision the decision does not need. The cost of a
/// false positive is one re-opened tool loop that finds nothing new, bounded by
/// [`MAX_REOPENS`] and by every budget axis; the cost of a false negative is the
/// run behaving exactly as it did before this existed.
fn declares_unanswered(verdict: &str) -> bool {
    verdict.to_ascii_uppercase().contains("UNANSWERED")
}

/// The report turn passes no tools at all. Omitting the field (rather than
/// sending an empty list) is what makes "there is nothing to call" structural:
/// measured across three models, ~1 run in 5 used to answer the report request
/// with one more tool call, and no wording prevented it reliably.
const NO_TOOLS: &[ToolSpec] = &[];

/// The system prompt for the report turn, replacing the tool-protocol one.
///
/// A conversation that has spent up to sixteen turns rewarding one shape of reply
/// (a bare JSON action, prose forbidden) will keep producing it unless told the
/// game has changed. Stating the new role is cheaper and more reliable than
/// arguing with the old instruction from a user message.
const REPORT_SYSTEM_PROMPT: &str = "You are a technical writer. Below is a research \
    question about a codebase and the evidence a research agent gathered for it — \
    code excerpts, symbol locations and file outlines. Your only job now is to write \
    the report. You have NO tools: there is nothing left to call, and any JSON you \
    emit would be discarded. Write Markdown prose, grounded in the evidence, citing \
    locations as `path:start-end`.";

/// The report turn's user message: the standing instruction, plus a truncation
/// preamble when the run did not finish on its own terms.
///
/// The preamble is the reporting half of the hard deadline. A run cut off at its wall
/// clock has, by construction, an incomplete answer, and a model that has just been
/// told "write the final report" will otherwise write it in the confident register of
/// a finished investigation. `done.reason` already says the run was truncated, but
/// that is a field on a wire event — the *report* is what gets read, pasted and
/// quoted months later, so the disclaimer has to be inside it.
///
/// The sub-questions come from the plan rather than from the sufficiency turn, which
/// a truncated run deliberately skips: that turn's job is re-opening the loop, and a
/// run with nothing left to spend cannot take it.
fn report_request(
    reason: DoneReason,
    state: &RunState,
    budget: Budget,
    elapsed: Duration,
) -> String {
    let base = "The investigation is over and the tools are closed. Write the final report \
         answering the original research question, using only the evidence above. \
         Output a complete, self-contained Markdown document (headings, code spans \
         where useful) and NOTHING else — no JSON, no tool call, no preamble. Cite \
         evidence as `path:start-end`, and cite only locations a tool returned in \
         this run. If the evidence was insufficient, say so explicitly and state \
         what is missing.";
    if !reason.is_truncated() {
        return base.to_string();
    }
    let limit = match reason {
        // Named in minutes, and the elapsed time alongside it: a model told only
        // "the time limit" tends to invent a number for the sentence it is being
        // asked to write.
        DoneReason::TimeExhausted => format!(
            "its time limit of {} minutes, having run for {} seconds",
            budget.max_seconds.div_ceil(60).max(1),
            elapsed.as_secs()
        ),
        DoneReason::TokensExhausted => "its token limit".to_string(),
        DoneReason::BudgetExhausted => "its tool-call limit".to_string(),
        DoneReason::ContextExhausted => {
            "the limit of what the model can hold in context".to_string()
        }
        DoneReason::RepeatedCalls => "the loop repeating itself".to_string(),
        DoneReason::Unparseable => "replies that could not be acted on".to_string(),
        DoneReason::Finalized => unreachable!("not truncated"),
    };
    let plan = match &state.plan {
        Some(p) => format!(
            "\n\nThis was your plan:\n\n{p}\n\nName the sub-questions you never got to, \
             explicitly, as an unanswered list."
        ),
        None => String::new(),
    };
    format!(
        "IMPORTANT: this investigation did NOT finish — it was stopped by {limit}. Begin \
         the report by saying so in one sentence, so nobody reads it as a complete \
         answer. Then report what you did establish, and be explicit about which parts \
         of the question remain open and what you would have looked at next. Do not \
         present a partial finding as a settled one, and do not pad the gaps with what \
         you would expect the code to do.{plan}\n\n{base}"
    )
}

/// The report the *server* writes when the report window expired before the model
/// produced anything.
///
/// A last resort, and deliberately a dull one: it asserts nothing about the code. It
/// repeats the question, says how the run ended, replays the plan the model wrote and
/// lists the files the run was actually shown. That is enough to tell the caller what
/// happened and where to look, and every path in it came from a tool result — so the
/// citation check scores it honestly rather than flagging the server's own prose as
/// invention.
///
/// `elapsed` must be the **investigation's** elapsed time, not the run's: this notice
/// exists to not overstate, and quoting a figure that silently includes the report
/// window would have it misreport the one number it is about.
///
/// The alternative — salvaging the model's half-written draft — is not available:
/// `chat_stream` discards accumulated content when it is cancelled, and a report
/// truncated mid-sentence reads as authoritative in a way this does not.
fn forced_synthesis(
    params: &ResearchParams,
    state: &RunState,
    evidence: &Evidence,
    reason: DoneReason,
    elapsed: Duration,
) -> String {
    let mut out = String::from("# Research incomplete\n\n");
    out.push_str(&format!(
        "**No report was produced.** The investigation ran for {} seconds and stopped \
         because {}; the model was then given {} seconds to write a report and did not \
         finish one. What follows is an account of the run itself, written by the \
         server — it contains no findings about the code.\n\n",
        elapsed.as_secs(),
        match reason {
            DoneReason::Finalized => "the model judged the evidence sufficient",
            DoneReason::TimeExhausted => "it reached its time limit",
            DoneReason::TokensExhausted => "it reached its token limit",
            DoneReason::BudgetExhausted => "it reached its tool-call limit",
            DoneReason::ContextExhausted => "it filled its share of the context window",
            DoneReason::RepeatedCalls => "it kept repeating the same lookups",
            DoneReason::Unparseable => "its replies could not be acted on",
        },
        params.report_timeout_ms / 1000,
    ));
    out.push_str(&format!("## Question\n\n{}\n\n", params.question.trim()));
    if let Some(plan) = &state.plan {
        out.push_str(&format!(
            "## The plan the run set itself\n\n{}\n\n",
            plan.trim()
        ));
    }
    let paths = evidence.paths();
    if paths.is_empty() {
        out.push_str("## Evidence\n\nThe run was shown no files before it stopped.\n");
    } else {
        out.push_str(&format!(
            "## Files the run read ({})\n\nThese are the locations it had gathered when \
             it stopped. They are where an answer would have come from.\n\n",
            paths.len()
        ));
        for path in &paths {
            out.push_str(&format!("- `{path}`\n"));
        }
    }
    out
}

/// What the run has already done, kept so it can be handed back to the model.
///
/// Everything here is derived from calls the loop already tracks — nothing new is
/// asked of the model, and producing it costs no tokens. It exists because the
/// transcript is the run's only memory and by step 19 it is ~165k tokens of chunk
/// bodies, in which "I already asked that" is not written anywhere. Rendered by
/// [`format_state_note`] into a single message that is re-pinned next to the
/// generation point every turn, rather than left to decay in the middle of a long
/// context.
#[derive(Debug, Default)]
struct RunState {
    /// The model's own plan from the [`PLAN_REQUEST`] turn, repeated every turn
    /// because a plan the model cannot see is not a plan it can be held to.
    plan: Option<String>,
    /// Conclusions the model chose to keep, in the order it wrote them.
    ///
    /// The only part of this struct the model authors rather than the loop observing
    /// it, and the reason it exists: `ChatMessage` has no thinking field, so a model's
    /// reasoning is erased every turn and re-derived from raw tool output. Everything
    /// else here answers "what have I already asked"; this answers "what have I already
    /// worked out".
    notes: Vec<String>,
    searches: Vec<String>,
    symbols: Vec<String>,
    outlines: Vec<String>,
    callers: Vec<String>,
    globs: Vec<String>,
    greps: Vec<String>,
    reads: Vec<String>,
}

impl RunState {
    /// Record an action that is about to execute. Duplicates never reach here —
    /// they are rejected upstream — so the lists stay short by construction.
    fn record(&mut self, action: &Action) {
        match action {
            Action::Search { query, path_prefix } => self.searches.push(match path_prefix {
                Some(p) => format!("{query} (under {p})"),
                None => query.clone(),
            }),
            Action::Symbols { name, .. } => self.symbols.push(name.clone()),
            Action::Outline { path } => self.outlines.push(path.clone()),
            Action::Callers { name, direction } => self.callers.push(format!(
                "{name} ({})",
                match direction.unwrap_or(CallDirection::In) {
                    CallDirection::In => "who references it",
                    CallDirection::Out => "what it references",
                }
            )),
            Action::ListFiles { glob } => self.globs.push(glob.clone()),
            Action::ReadChunks {
                path,
                start_line,
                end_line,
            } => self.reads.push(format!("{path}:{start_line}-{end_line}")),
            Action::Grep { pattern, glob } => self.greps.push(match glob {
                Some(g) => format!("{pattern} (in {g})"),
                None => pattern.clone(),
            }),
            // Both are applied by `apply_local`, which owns the caps and the wording
            // of its own replies; there is nothing to record as "already asked".
            Action::Note { .. } | Action::RevisePlan { .. } => {}
            Action::Finalize => {}
        }
    }

    /// Keep a note, or say why not.
    ///
    /// Returns the tool reply. Every refusal is explicit: a cap the model cannot see
    /// is a cap it will keep hitting, and a note that vanished silently is worse than
    /// one that was never written, because the model will go on relying on it.
    fn keep_note(&mut self, text: &str) -> String {
        let text = text.trim();
        if text.is_empty() {
            return "Nothing recorded: the note was empty.".to_string();
        }
        if text.chars().count() > MAX_NOTE_CHARS {
            return format!(
                "Not recorded: that note is {} characters and the limit is {MAX_NOTE_CHARS}. \
                 Write the conclusion, not the evidence — the evidence is already above. \
                 Send it again, shorter.",
                text.chars().count()
            );
        }
        self.notes.push(text.to_string());
        if self.notes.len() > MAX_NOTES {
            let dropped = self.notes.remove(0);
            return format!(
                "Note kept. You are at the {MAX_NOTES}-note limit, so your oldest note was \
                 dropped to make room: \"{dropped}\""
            );
        }
        format!("Note kept ({} of {MAX_NOTES}).", self.notes.len())
    }
}

/// Fold one executed call into the run's tool-usage counters.
///
/// Reads the *reply* for the refusal cases, which is where the truth is: a note over
/// the character cap and a note that was kept are the same `Action`, and only the
/// text the model was sent distinguishes them. Cheap and exact, against the
/// alternative of threading a verdict back out of `apply_local` and `execute` for the
/// sake of a counter.
fn tally_tool_use(tools: &mut RunTools, action: &Action, reply: &str, hits: usize) {
    match action {
        Action::Note { .. } => {
            if reply.starts_with("Note kept") {
                tools.notes_written += 1;
            } else {
                tools.notes_rejected += 1;
            }
        }
        Action::RevisePlan { .. } if reply.starts_with("Plan replaced") => {
            tools.plan_revisions += 1;
        }
        Action::Grep { .. } => {
            tools.grep_calls += 1;
            if hits > 0 {
                tools.grep_hits += 1;
            }
        }
        // The scope's cost and benefit. A refusal is path-keyed and reported by
        // `out_of_scope_reply`; hidden rows are counted in the name-keyed formatters,
        // which is why both are matched on the reply rather than on a response field —
        // the field lives on four different response types.
        Action::Outline { .. } | Action::ReadChunks { .. }
            if reply.contains("outside this run's scope") =>
        {
            tools.out_of_scope_refusals += 1;
        }
        _ => {}
    }
    if let Action::Symbols { .. } | Action::Callers { .. } | Action::Grep { .. } = action {
        tools.out_of_scope_rows += hidden_row_count(reply);
    }
}

/// The count a formatter reported as hidden by the scope, parsed back out of its own
/// reply.
///
/// Ugly, and the alternative is uglier: `Executed` would need a scope-accounting field
/// that five of eight actions leave at zero, threaded for one journal column. The
/// formatters own the wording; this reads the one number they all print the same way.
fn hidden_row_count(reply: &str) -> usize {
    for marker in [
        " are outside this run's scope",
        " more are outside this run's scope",
        " site(s) are outside this run's scope",
    ] {
        if let Some(at) = reply.find(marker) {
            let digits: String = reply[..at]
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit() || *c == ' ')
                .collect::<String>()
                .chars()
                .rev()
                .filter(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse() {
                return n;
            }
        }
    }
    0
}

/// Run an action that changes the run's own state rather than reading the index.
///
/// `note` and `revise_plan` deliberately do not go through [`ResearchTools`]: they
/// touch no index, need no cancellation token and cannot fail in a way worth
/// reporting. Keeping both the mutation and its reply here means the tool loop still
/// has one call site per kind, instead of a special case per tool.
fn apply_local(state: &mut RunState, action: &Action) -> Executed {
    match action {
        Action::Note { text } => Executed {
            call: StepCall::Note {
                text: text.trim().to_string(),
            },
            hits: 0,
            text: state.keep_note(text),
            shown: vec![],
        },
        Action::RevisePlan { plan } => {
            let plan = plan.trim().to_string();
            let reply = if plan.is_empty() {
                "Plan unchanged: the replacement was empty.".to_string()
            } else {
                state.plan = Some(plan.clone());
                "Plan replaced. From the next turn the run-state note carries the new \
                 one, and it is what your evidence will be judged against."
                    .to_string()
            };
            Executed {
                call: StepCall::RevisePlan { plan },
                hits: 0,
                text: reply,
                shown: vec![],
            }
        }
        other => unreachable!("{other:?} is not a local action"),
    }
}

/// One capped, comma-free list line, or nothing when the list is empty.
fn state_note_line(out: &mut String, label: &str, items: &[String], cap: usize) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("\n{label}:\n"));
    for item in items.iter().take(cap) {
        out.push_str(&format!("  - {item}\n"));
    }
    if items.len() > cap {
        out.push_str(&format!("  - …and {} more\n", items.len() - cap));
    }
}

/// The model's own notes, as the message they are pinned in.
///
/// Separate from [`format_state_note`] because it is also needed on its own: the
/// report turn does not rebuild the run-state note (its instructions are fixed by
/// then), but the notes are the model's accumulated conclusions and are exactly what
/// the report should be written from. Returns `None` when there is nothing to pin,
/// so no empty message is pushed.
fn format_notes_note(state: &RunState) -> Option<String> {
    if state.notes.is_empty() {
        return None;
    }
    let mut out = String::from(
        "Your notes (maintained by the server from your own `note` calls — the only \
         conclusions of yours that survived):\n",
    );
    for note in &state.notes {
        out.push_str(&format!("  - {note}\n"));
    }
    Some(out)
}

/// The run-state note pinned before each turn.
///
/// Deliberately a *user* message: it is not something the model said, and
/// attributing it to the assistant would make invented history indistinguishable
/// from the model's own words. Placed after the previous turn's `role: "tool"`
/// replies and before the next assistant turn, so the call/reply pairing the
/// tool loop guarantees is untouched.
fn format_state_note(
    state: &RunState,
    evidence: &Evidence,
    scope: &ToolScope,
    progress: &RunProgress,
) -> String {
    let mut out = String::from(
        "Run state (maintained by the server — this is your history, not an instruction):\n",
    );
    if scope.is_scoped() {
        // Repeated here as well as in the system prompt: by turn 15 the system
        // prompt is tens of thousands of tokens back, and a wall the model has
        // forgotten is a wall it spends calls rediscovering.
        out.push_str(&format!(
            "\nScope of this run (enforced on every tool, cannot be widened): {}\n",
            scope.describe()
        ));
    }
    if let Some(plan) = &state.plan {
        out.push_str(&format!("\nYour plan:\n{}\n", plan.trim()));
    }
    // First after the plan, and with a cap of its own: these are conclusions, not
    // history, and they are the one thing here the model wrote itself.
    state_note_line(
        &mut out,
        "Your notes (conclusions you chose to keep — the only ones that survived)",
        &state.notes,
        MAX_NOTES,
    );
    state_note_line(
        &mut out,
        "Searches already executed (re-asking these, or rephrasing them, is rejected)",
        &state.searches,
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "Literals already grepped for",
        &state.greps,
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "Symbols already looked up",
        &state.symbols,
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "Files already outlined",
        &state.outlines,
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "Call graphs already asked for",
        &state.callers,
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "Globs already listed",
        &state.globs,
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "Line ranges already read",
        &state.reads,
        STATE_NOTE_MAX_ITEMS,
    );

    // Paths only. Which files the model has seen *anything* from is the cheap,
    // useful half; the spans are already in the transcript above.
    state_note_line(
        &mut out,
        "Files some tool has shown you",
        &evidence.paths(),
        STATE_NOTE_MAX_ITEMS,
    );

    // Freshness. The index is written by other processes while this run reads it,
    // so the transcript above can describe code that no longer exists — and the
    // transcript is the only memory the run has. Saying which parts went stale is
    // the whole remedy: nothing here holds the corpus still.
    state_note_line(
        &mut out,
        "Files that CHANGED in the index since you read them — anything above about \
         these files may describe code that no longer exists; re-read (read_chunks) \
         before citing them",
        &evidence.changed_paths(),
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "Files that LEFT the index during this run — do not cite them",
        &evidence.removed_paths(),
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "Files being reindexed right now — search may not reach them yet, while \
         outline/symbols/read_chunks still will",
        &evidence.in_flight_paths(),
        STATE_NOTE_MAX_ITEMS,
    );

    out.push_str(&format!(
        "\nBudget: step {}/{}, {}s of {}s, {} of {} tokens.\n",
        progress.steps,
        progress.max_steps,
        progress.elapsed_ms / 1000,
        progress.max_ms / 1000,
        progress.tokens,
        progress.max_tokens,
    ));
    out
}

fn format_search_results(query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!("No results for \"{query}\".");
    }
    let mut out = format!("Results for \"{query}\" ({} hits):\n", results.len());
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!(
            "\n[{}] {}:{}-{} (score {:.3})\n```\n{}\n```\n",
            i + 1,
            r.path,
            r.start_line,
            r.end_line,
            r.score,
            r.code
        ));
    }
    out
}

fn format_symbols_response(name: &str, resp: &SymbolsResponse, scope: &ToolScope) -> String {
    let hidden = resp.out_of_scope_definitions + resp.out_of_scope_references;
    if resp.total_definitions == 0 && resp.total_references == 0 {
        // "Not here" and "not anywhere" are different answers, and `/symbols` calls
        // the second one definitive — so a scope that hid every occurrence must say
        // so, or the model concludes the name does not exist.
        return if hidden > 0 {
            format!(
                "No symbol named \"{name}\" within this run's scope ({}), but {hidden} \
                 occurrence(s) exist outside it. The scope is fixed by whoever started \
                 this run and cannot be widened.",
                scope.describe()
            )
        } else {
            format!("No symbol named \"{name}\" in the index.")
        };
    }
    let mut out = format!(
        "Symbol \"{name}\": {} definition(s), {} reference(s).{}\n",
        resp.total_definitions,
        resp.total_references,
        if hidden > 0 {
            format!(" ({hidden} more are outside this run's scope.)")
        } else {
            String::new()
        }
    );
    for d in &resp.definitions {
        out.push_str(&format!(
            "def  {} {}:{}-{}{}\n",
            d.kind,
            d.path,
            d.start_line,
            d.end_line,
            d.parent_name
                .as_deref()
                .map(|p| format!(" (in {p})"))
                .unwrap_or_default()
        ));
    }
    for r in &resp.references {
        out.push_str(&format!(
            "ref  {} {}:{}{}\n",
            r.kind,
            r.path,
            r.start_line,
            r.parent_name
                .as_deref()
                .map(|p| format!(" (in {p})"))
                .unwrap_or_default()
        ));
    }
    out
}

/// What one run cost the local model, accumulated over its turns.
///
/// Ollama reports the counts per turn; a turn that reports none is counted in
/// `turns_unreported` rather than as zero, so the totals are never quietly
/// short. This is the raw material for the only question that matters
/// economically: local tokens read vs. report tokens returned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TokenTally {
    turns: usize,
    turns_unreported: usize,
    prompt_tokens: u64,
    eval_tokens: u64,
    /// The largest single prompt of the run. The sum says what the run cost; this
    /// says how close it came to the window — the number that decides whether
    /// evidence was silently dropped.
    peak_prompt_tokens: u64,
    /// The window actually available (`num_ctx` after per-model clamping).
    num_ctx: u64,
}

impl TokenTally {
    /// Fold one turn's counts in, passing the outcome through.
    fn record(&mut self, outcome: ChatOutcome) -> ChatOutcome {
        self.turns += 1;
        // A turn Ollama never finished reports no window either (`chat_turn` abandons
        // a runaway one and synthesises an empty reply). Overwriting a known `num_ctx`
        // with zero would make the whole run's context ratio unreadable, so the last
        // real window stands — "I could not measure it" is not "the window is nothing".
        if outcome.num_ctx > 0 {
            self.num_ctx = outcome.num_ctx;
        }
        match (outcome.prompt_tokens, outcome.eval_tokens) {
            (None, None) => self.turns_unreported += 1,
            (p, e) => {
                self.prompt_tokens += p.unwrap_or(0);
                self.eval_tokens += e.unwrap_or(0);
                self.peak_prompt_tokens = self.peak_prompt_tokens.max(p.unwrap_or(0));
            }
        }
        outcome
    }
}

/// One completed research run, as measured from inside the loop.
struct ResearchOutcome {
    steps: usize,
    reason: DoneReason,
    tally: TokenTally,
    citations: CitationReport,
    /// How far the index moved under the run.
    staleness: RunStaleness,
    /// Present only when the draft report failed its citation check and was sent
    /// back for correction.
    revalidation: Option<Revalidation>,
    /// What the run did with the scratchpad, `grep` and its scope.
    tools: RunTools,
    /// The finished report. Streamed already — carried back only so the run can
    /// be journalled; nothing re-sends it.
    report: String,
}

/// What the citation-revalidation phase found and cost.
///
/// Rides on the `citations` event beside the *final* report's counts, which is the
/// only way to tell a report that was right the first time from one that was
/// corrected — and the only way a harness can measure whether the phase pays for
/// itself. Absent when the draft's citations all checked out.
///
/// The third defect the gate catches — a draft that cites *nothing* checkable —
/// needs no field of its own and deliberately does not get one: it is exactly the
/// case where this struct is present with all three counts at zero, since a report
/// with no parseable citations has no failing ones either. That inference costs no
/// wire field, and a wire field here would have to be added in four places (the
/// handler doc, its `#[utoipa::path]`, the VS Code client and scout's
/// `_CITATION_KEYS`) to buy what the existing shape already says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Revalidation {
    /// Citations in the *draft* naming a path no tool had returned.
    pub draft_unverified: usize,
    /// Citations in the draft whose path was shown but whose range was not.
    pub draft_path_only: usize,
    /// Citations in the draft whose file the index changed (or dropped) after the
    /// run had read it.
    pub draft_stale: usize,
    /// Tool calls the phase executed. Zero when the run had already spent its
    /// budget and could only correct or drop the claim.
    pub steps: usize,
}

/// How far the index moved under one run, counted at the end.
///
/// Journalled so "was this report written over a corpus that held still?" is
/// answerable from the record rather than only from a stream nobody kept.
/// `in_flight` deliberately has no counterpart here: it is a momentary state, and
/// a count of it at one instant would say nothing about the run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunStaleness {
    pub changed_files: usize,
    pub removed_files: usize,
}

/// Where a finished run is recorded, so a run leaves a trace beyond the SSE
/// stream it was watched on.
///
/// Its own seam rather than a method on [`ResearchTools`]: that trait is the
/// loop's read access to the index, and writing a run record is neither a lookup
/// nor something a lookup failure should be confused with. Without this, only
/// runs somebody is watching can ever be measured — production traffic is
/// unobservable, and any quality comparison has to be re-run rather than queried.
#[async_trait]
pub trait ResearchJournal: Send + Sync {
    /// Record a finished run. **Best-effort by contract**: the report has already
    /// reached the client, so an implementation must log its own failures and
    /// return — never propagate, never panic.
    async fn record(&self, record: RunRecord);
}

/// A journal that keeps nothing, for the loop's own tests — they are about the
/// loop, not about persistence, and the journal has its own tests in
/// `db::research`. Test-only: in production a run that leaves no trace is the
/// problem this seam exists to fix, so there is no reason to offer it.
#[cfg(test)]
pub struct NoJournal;

#[cfg(test)]
#[async_trait]
impl ResearchJournal for NoJournal {
    async fn record(&self, _record: RunRecord) {}
}

/// One finished run, as handed to a [`ResearchJournal`].
///
/// Carries only what the loop itself produced. Everything the *request* decided —
/// project, effort level, seed — is known to whoever built the journal and is
/// closed over there, so this struct cannot drift out of step with the handler.
/// What a run did with the tools that were added on an argument rather than on a
/// measurement — the scratchpad, the literal search, the enforced scope.
///
/// One struct rather than a dozen loose fields on [`RunRecord`], because they are read
/// together or not at all: "did notes get used", "did grep find anything", "what did
/// the scope cost and what did it hide". Journalled onto the run's `research_runs` row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunTools {
    pub notes_written: usize,
    pub notes_rejected: usize,
    pub plan_revisions: usize,
    pub grep_calls: usize,
    pub grep_hits: usize,
    /// Path-keyed lookups refused for being outside the scope.
    pub out_of_scope_refusals: usize,
    /// Rows the scope hid from name- and text-keyed lookups, summed.
    pub out_of_scope_rows: usize,
    /// The report was written by the server, the window having expired first.
    pub forced_synthesis: bool,
    pub report_window_ms: u64,
    pub report_elapsed_ms: u64,
}

pub struct RunRecord {
    pub question: String,
    pub model: String,
    pub prompt_version: &'static str,
    pub budget: Budget,
    pub reason: DoneReason,
    pub steps: usize,
    pub turns: usize,
    pub elapsed_ms: u64,
    pub prompt_tokens: u64,
    pub eval_tokens: u64,
    pub peak_prompt_tokens: u64,
    pub num_ctx: u64,
    pub citations: CitationReport,
    pub staleness: RunStaleness,
    /// Present only when the draft report failed its citation check. Carried here
    /// because it is part of what the loop did; `research_runs` has no column for
    /// it (that would be another side table), so `insert_run` ignores it and the
    /// metrics journal is currently its only reader.
    pub revalidation: Option<Revalidation>,
    /// What the run did with the scratchpad, `grep` and its scope.
    pub tools: RunTools,
    pub report: String,
}

// ─── citation provenance ────────────────────────────────────────────────────
//
// scout tells the calling agent to trust a research report and *not* spot-check
// it, which is the right instruction — re-reading the files destroys the whole
// saving — but it is only defensible if the server checks what it mechanically
// can. A fabricated `path:12-30` is otherwise invisible and authoritative.
//
// What is checkable here is **provenance**, not existence: every tool result the
// loop returned already carries `(path, start_line, end_line)`, so "was this
// location ever actually shown to you?" needs no SQL and no migration. Whether a
// line range exists in the real file is deliberately *not* checked — the schema
// holds no line counts, so it would answer "unknown" for every file until a full
// reindex, for the smaller half of the value.

/// One location a tool put in front of the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

/// What one path showed the model, and whether it still says the same thing.
#[derive(Debug, Default)]
struct PathEvidence {
    spans: Vec<Span>,
    /// The index's hash for this file when the run first probed it, i.e. shortly
    /// after it was first shown. `None` until that first probe.
    baseline_sha: Option<String>,
    /// Sticky: the hash moved after the run had read the file. Sticky because the
    /// evidence in the transcript is *already* stale — a file reindexed back to
    /// its old content later does not un-mislead a note the model took from the
    /// intermediate version.
    changed: bool,
    /// Sticky: the file left the index during the run.
    removed: bool,
    /// **Not** sticky: whether a reindex is in flight right now. This drives an
    /// instruction about what `search` can currently reach, so a finished reindex
    /// must stop producing it — and a file that finished reindexing shows up as
    /// `changed` anyway.
    in_flight: bool,
}

/// Everything the tools showed the model this run, by path.
///
/// A path present with no usable span (a `list_files` hit) records no span, which
/// is why the buckets distinguish "path was shown" from "this range was shown":
/// knowing a file exists is not evidence about its line 40.
///
/// It doubles as the run's freshness ledger. The index is mutated by other
/// processes (`mindex-index`, `mindex-watch`) throughout a run that can last half
/// an hour, and nothing serializes them against research — deliberately, since the
/// writer is external and blocking it would make `mindex-watch` drop the debounced
/// change for the very file the user just edited. So the run does not try to hold
/// the corpus still; it keeps a baseline per path and reports what moved.
#[derive(Debug, Default)]
struct Evidence {
    by_path: std::collections::HashMap<String, PathEvidence>,
}

impl Evidence {
    fn record(&mut self, path: &str, span: Option<Span>) {
        let e = self.by_path.entry(path.to_string()).or_default();
        if let Some(s) = span
            && !e.spans.contains(&s)
        {
            e.spans.push(s);
        }
    }

    /// Every path the run has been shown — what a freshness probe asks about.
    fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.by_path.keys().cloned().collect();
        paths.sort();
        paths
    }

    /// Fold a probe result in.
    ///
    /// `asked` is what the probe covered, and "absent from `versions`" is only
    /// evidence of removal *within* it: the core chunks its query, and inferring
    /// removal from a path nobody asked about would invent staleness. The
    /// `baseline_sha` guard adds the other half — only a file the run has actually
    /// seen in the index can be said to have left it.
    fn apply_versions(&mut self, asked: &[String], versions: &[FileVersion]) {
        let mut found: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for v in versions {
            let Some(e) = self.by_path.get_mut(&v.path) else {
                continue;
            };
            found.insert(v.path.as_str());
            match &e.baseline_sha {
                None => e.baseline_sha = Some(v.sha256.clone()),
                Some(baseline) => {
                    if *baseline != v.sha256 {
                        e.changed = true;
                    }
                }
            }
            e.in_flight = v.in_flight;
        }
        for path in asked {
            if found.contains(path.as_str()) {
                continue;
            }
            if let Some(e) = self.by_path.get_mut(path)
                && e.baseline_sha.is_some()
            {
                e.removed = true;
                e.in_flight = false;
            }
        }
    }

    /// Whether a citation into this path describes code the index no longer holds.
    /// A reindex merely *in flight* is not staleness — nothing the run read has
    /// been contradicted yet.
    fn is_stale(&self, path: &str) -> bool {
        self.by_path
            .get(path)
            .is_some_and(|e| e.changed || e.removed)
    }

    fn paths_where(&self, pred: impl Fn(&PathEvidence) -> bool) -> Vec<String> {
        let mut paths: Vec<String> = self
            .by_path
            .iter()
            .filter(|(_, e)| pred(e))
            .map(|(p, _)| p.clone())
            .collect();
        paths.sort();
        paths
    }

    fn changed_paths(&self) -> Vec<String> {
        self.paths_where(|e| e.changed && !e.removed)
    }

    fn removed_paths(&self) -> Vec<String> {
        self.paths_where(|e| e.removed)
    }

    fn in_flight_paths(&self) -> Vec<String> {
        self.paths_where(|e| e.in_flight)
    }

    /// The run's own staleness counts, for the journal.
    fn staleness(&self) -> RunStaleness {
        RunStaleness {
            changed_files: self.changed_paths().len(),
            removed_files: self.removed_paths().len(),
        }
    }

    fn verdict(&self, c: &Citation) -> Verdict {
        let Some(spans) = self.by_path.get(&c.path).map(|e| &e.spans) else {
            return Verdict::Unverified;
        };
        // Overlap, not containment: the model legitimately cites a range it read
        // across two adjacent chunks, and a chunk boundary is not a fact about
        // the code.
        if spans.iter().any(|s| c.start <= s.end && s.start <= c.end) {
            Verdict::Verified
        } else {
            Verdict::PathOnly
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The path and an overlapping range were both shown.
    Verified,
    /// The path was shown, this range was not.
    PathOnly,
    /// No tool this run ever returned this path.
    Unverified,
}

/// A `path:start-end` reference parsed out of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Citation {
    path: String,
    start: usize,
    end: usize,
}

/// How many of a report's citations the run's own tool results support.
///
/// `cited_paths` is not on the wire — it is what the per-run record stores, so
/// the corpus can be queried by file later without re-parsing every report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CitationReport {
    pub total: usize,
    pub verified: usize,
    pub path_only: usize,
    pub unverified: usize,
    /// Citations whose file the index changed (or dropped) *after* this run read
    /// it. Orthogonal to the three verdicts above: provenance says whether the
    /// model was ever shown the location, staleness says whether what it was shown
    /// still holds. A citation can be impeccably verified and stale, and collapsing
    /// the two would hide which of the two defects it has.
    pub stale: usize,
    /// The fabricated ones, deduplicated and capped — this is a signal, and a
    /// model hallucinating a hundred paths does not need a hundred wire entries.
    pub unverified_paths: Vec<String>,
    /// The cited paths that moved under the run, deduplicated and capped the same
    /// way.
    pub stale_paths: Vec<String>,
    pub cited_paths: Vec<String>,
}

/// Distinct `unverified_paths` (and `stale_paths`) reported. Enough to act on,
/// small enough that a pathological run cannot bloat the event.
const MAX_UNVERIFIED_PATHS_REPORTED: usize = 10;

/// Below this length an uncited report is taken at its word rather than sent back.
///
/// A report that cites nothing is normally ungrounded, but there is one honest
/// shape of it: "I could not reach the answer from this scope". That report is
/// short by nature and is the *correct* outcome — a scoped run that cannot answer
/// must say so rather than confabulate — so demanding citations from it would be
/// demanding a fabrication. Sized from the measured corpus (2026-07-30, 24 runs):
/// the five real ungrounded reports ran 2018–4520 chars, while the server's own
/// `forced_synthesis` notice — already exempt, since a server-written report is
/// never sent back — is 471.
const MIN_GROUNDED_REPORT_CHARS: usize = 800;

/// Whether `c` can be part of a path token: the character class of a repo-relative
/// path. Deliberately excludes backticks, quotes, parens and whitespace, so a
/// citation wrapped in Markdown (`` `src/x.rs:1-2` ``) parses without stripping.
fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-' | '+')
}

/// Extract `path:start-end` references from a report.
///
/// Hand-rolled rather than a regex dependency: the grammar is one line and this
/// keeps the crate list unchanged. It scans for `:<digits>-<digits>` and walks
/// backwards for the path, requiring a file extension (a trailing `.ext`) — which
/// is what stops it matching prose like "step 3:10-20" or a bare `1-2`.
fn parse_citations(report: &str) -> Vec<Citation> {
    let b = report.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b':' {
            i += 1;
            continue;
        }
        // ── forward: <digits>-<digits>
        let mut j = i + 1;
        let ds = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == ds || j >= b.len() || b[j] != b'-' {
            i += 1;
            continue;
        }
        let start: usize = match report[ds..j].parse() {
            Ok(n) => n,
            Err(_) => {
                i += 1;
                continue;
            }
        };
        j += 1;
        let es = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == es {
            i += 1;
            continue;
        }
        let end: usize = match report[es..j].parse() {
            Ok(n) => n,
            Err(_) => {
                i += 1;
                continue;
            }
        };
        // ── backward: the longest run of path characters before the colon
        let mut k = i;
        while k > 0 && is_path_char(report[k - 1..k].chars().next().unwrap_or(' ')) {
            k -= 1;
        }
        let path = &report[k..i];
        // Two rules, and both earn their keep:
        //  - an extension is what separates a path from a word, so the last
        //    segment must carry one ("step 3:10-20" is prose, not a citation);
        //  - every indexed path is repo-relative, so a leading or doubled slash
        //    means this is not one — which is also what stops a bare URL
        //    ("http://example.com:8080-8090") scoring as a cited file.
        let relative = !path.starts_with('/') && !path.contains("//");
        let has_extension = path
            .rsplit('/')
            .next()
            .and_then(|seg| seg.rsplit_once('.'))
            .is_some_and(|(stem, ext)| {
                !stem.is_empty()
                    && !ext.is_empty()
                    && ext.chars().all(|c| c.is_ascii_alphanumeric())
            });
        let looks_like_path = relative && has_extension;
        if looks_like_path && start <= end {
            out.push(Citation {
                path: path.to_string(),
                start,
                end,
            });
        }
        i = j;
    }
    out
}

/// Score a finished report against what the run's tools actually returned.
fn check_citations(report: &str, evidence: &Evidence) -> CitationReport {
    let citations = parse_citations(report);
    let mut r = CitationReport {
        total: citations.len(),
        ..Default::default()
    };
    for c in &citations {
        match evidence.verdict(c) {
            Verdict::Verified => r.verified += 1,
            Verdict::PathOnly => r.path_only += 1,
            Verdict::Unverified => {
                r.unverified += 1;
                if !r.unverified_paths.contains(&c.path)
                    && r.unverified_paths.len() < MAX_UNVERIFIED_PATHS_REPORTED
                {
                    r.unverified_paths.push(c.path.clone());
                }
            }
        }
        // Independent of the verdict: a location the model really was shown can
        // still describe code that has since been reindexed away.
        if evidence.is_stale(&c.path) {
            r.stale += 1;
            if !r.stale_paths.contains(&c.path)
                && r.stale_paths.len() < MAX_UNVERIFIED_PATHS_REPORTED
            {
                r.stale_paths.push(c.path.clone());
            }
        }
        if !r.cited_paths.contains(&c.path) {
            r.cited_paths.push(c.path.clone());
        }
    }
    r
}

/// Re-probe every path the run has been shown and fold the result into `evidence`.
///
/// Best-effort by design: a failed probe leaves the previous verdicts standing
/// rather than inventing staleness, because "I could not check" and "this file
/// changed" are different claims and only one of them belongs in front of the
/// model. Costs no budget axis and emits no event — it is the server's own
/// bookkeeping, not a step the model asked for.
async fn probe_freshness(
    index: &dyn ResearchTools,
    evidence: &mut Evidence,
    token: &CancellationToken,
) {
    let paths = evidence.paths();
    if paths.is_empty() {
        return;
    }
    match index.file_versions(paths.clone(), token).await {
        Ok(versions) => evidence.apply_versions(&paths, &versions),
        // The client is gone; the next `send` reports it. Nothing to log.
        Err(ApiError::Cancelled) => {}
        Err(e) => warn!(
            error_code = %e.code(),
            "Could not check whether the files this run has read are still current; \
             this turn's staleness verdicts stand on the previous probe."
        ),
    }
}

/// What a path-keyed tool says when the run's scope does not admit the file.
///
/// An explicit refusal, never an empty result. The same lesson `outline`'s `indexed`
/// flag encodes: a model told "nothing here" concludes the file is empty and moves on,
/// while a model told the wall exists and where it runs can work inside it — or report
/// honestly that the answer lies outside what it was given. It also names the scope as
/// unwidenable, because the alternative is a model spending its budget probing for a
/// gap in it.
fn out_of_scope_reply(path: &str, scope: &ToolScope) -> String {
    format!(
        "\"{path}\" is outside this run's scope ({}). It was not read. The scope is \
         fixed by whoever started this run and cannot be widened, so do not look for a \
         way around it: work within the scope, or say in your report that the answer \
         lies outside it.",
        scope.describe()
    )
}

/// Literal matches, with the line each one is on.
fn format_grep(pattern: &str, resp: &GrepResponse, scope: &ToolScope) -> String {
    if resp.matches.is_empty() {
        return if resp.out_of_scope > 0 {
            format!(
                "No occurrence of \"{pattern}\" within this run's scope ({}), though {} \
                 exist outside it.",
                scope.describe(),
                resp.out_of_scope
            )
        } else {
            format!(
                "No indexed chunk contains \"{pattern}\". The match is literal and \
                 case-insensitive, so check the spelling — or the text may live in a \
                 part of a file the slicer left out of every chunk."
            )
        };
    }
    let mut out = format!(
        "{} chunk(s) contain \"{pattern}\"{}{}:\n",
        resp.total,
        if resp.matches.len() as u64 == resp.total {
            String::new()
        } else {
            format!(", showing the first {}", resp.matches.len())
        },
        if resp.out_of_scope > 0 {
            format!(" ({} more are outside this run's scope)", resp.out_of_scope)
        } else {
            String::new()
        }
    );
    for m in &resp.matches {
        // The matching line for the reader, the chunk span for a citation — see
        // `GrepMatch`.
        out.push_str(&format!(
            "{}:{}  {}   [chunk {}-{}]\n",
            m.path, m.match_line, m.excerpt, m.start_line, m.end_line
        ));
    }
    out
}

fn format_outline(resp: &OutlineResponse, scope: &ToolScope) -> String {
    if !resp.in_scope {
        return out_of_scope_reply(&resp.path, scope);
    }
    if !resp.indexed {
        return format!(
            "\"{}\" is not an indexed file in this project. Use list_files to find the \
             real path.",
            resp.path
        );
    }
    if resp.symbols.is_empty() {
        return format!(
            "\"{}\" is indexed but declares no symbols (its language may have no symbol \
             extraction). Use search for its contents.",
            resp.path
        );
    }
    let mut out = format!(
        "Outline of {} [{}] ({} definition(s){}):\n",
        resp.path,
        resp.programming_language.map_or("?", |l| l.name()),
        resp.total_definitions,
        if resp.symbols.len() as u64 == resp.total_definitions {
            String::new()
        } else {
            format!(", showing the first {}", resp.symbols.len())
        }
    );
    for sym in &resp.symbols {
        out.push_str(&format!(
            "{} {} :{}-{}{}{}\n",
            sym.kind,
            sym.name,
            sym.start_line,
            sym.end_line,
            sym.parent_name
                .as_deref()
                .map(|p| format!(" (in {p})"))
                .unwrap_or_default(),
            sym.doc
                .as_deref()
                .map(|d| format!("  // {}", d.lines().next().unwrap_or("").trim()))
                .unwrap_or_default(),
        ));
    }
    out
}

/// Render a call-graph lookup, keeping the two ways it can come back empty apart.
///
/// "Nothing calls this" and "no such identifier" are different findings and the
/// model acts differently on them — the first invites reading the definition, the
/// second means the name was guessed wrong. Collapsing them into one empty list
/// tells the model its name was right, which is the failure `outline`'s `indexed`
/// flag already guards against.
///
/// The lexical caveat is repeated here, not only in the tool description: by the
/// time this text is read the description is thousands of tokens back, and a list
/// of file:line pairs reads as resolved unless it says otherwise.
fn format_callers(resp: &CallersResponse, scope: &ToolScope) -> String {
    let (subject, relation) = match resp.direction {
        CallDirection::In => ("references to", "referenced in"),
        CallDirection::Out => ("references made by", "referenced by"),
    };
    if resp.sites.is_empty() {
        if resp.out_of_scope_sites > 0 {
            return format!(
                "No {subject} \"{}\" within this run's scope ({}), though {} site(s) \
                 exist outside it. The scope is fixed by whoever started this run and \
                 cannot be widened.",
                resp.name,
                scope.describe(),
                resp.out_of_scope_sites
            );
        }
        return if resp.defined {
            format!(
                "\"{}\" is defined in this project, but there are no {subject} it \
                 anywhere in the index. Read its definition (symbols, then \
                 read_chunks) rather than looking for callers.",
                resp.name
            )
        } else {
            format!(
                "No symbol named \"{}\" is defined or referenced anywhere in the \
                 index — the name is probably wrong. Use outline on a likely file, \
                 or search, to find the real one.",
                resp.name
            )
        };
    }
    let mut out = format!(
        "{} {} \u{2014} {} occurrence(s) across {} site(s){}. These edges are \
         LEXICAL: they match the name only, so a common name mixes unrelated \
         definitions and an aliased import is missed entirely. Confirm with \
         read_chunks before relying on one.\n",
        resp.total_references,
        subject,
        resp.total_references,
        resp.total_sites,
        if resp.sites.len() as u64 == resp.total_sites {
            String::new()
        } else {
            format!(", showing the first {}", resp.sites.len())
        }
    );
    if resp.out_of_scope_sites > 0 {
        out.push_str(&format!(
            "A further {} site(s) are outside this run's scope and are not listed.\n",
            resp.out_of_scope_sites
        ));
    }
    for site in &resp.sites {
        out.push_str(&format!(
            "{} :{}{}{}\n",
            site.path,
            site.first_line,
            match (&site.symbol, &site.kind) {
                // A reference with no enclosing definition: file scope, an import,
                // a top-level call. Named as such rather than dropped.
                (None, _) => format!(" ({relation} at top level)"),
                (Some(sym), None) => format!(" ({relation} {sym})"),
                (Some(sym), Some(kind)) => format!(" ({relation} {kind} {sym})"),
            },
            if site.occurrences > 1 {
                format!(" \u{d7}{}", site.occurrences)
            } else {
                String::new()
            }
        ));
    }
    out
}

/// Render a chunk read, naming the lines that have **no** chunk.
///
/// The gap report is the point. Chunk coverage is sparse by construction, so a
/// silent empty answer would read as "those lines are empty" — the model would
/// then conclude a helper does not exist rather than that it was too short to
/// index, and go looking for it somewhere else.
fn format_read_chunks(
    path: &str,
    start_line: usize,
    end_line: usize,
    resp: &crate::backend::v0::models::ReadChunksResponse,
    scope: &ToolScope,
) -> String {
    if !resp.in_scope {
        return out_of_scope_reply(path, scope);
    }
    if !resp.indexed {
        return format!(
            "\"{path}\" is not an indexed file in this project. Use list_files to find \
             the real path."
        );
    }
    if resp.chunks.is_empty() {
        return format!(
            "No indexed chunk covers {path}:{start_line}-{end_line}. The file IS \
             indexed — the index stores chunks, not whole files, and definitions \
             shorter than the slicer's minimum (imports, consts, type aliases, small \
             helpers) get none. Do not read this as \"those lines are empty\": use \
             outline to see what the file defines, or read a wider range."
        );
    }
    let mut out = format!(
        "{} indexed chunk(s) covering {path}:{start_line}-{end_line}:\n",
        resp.chunks.len()
    );
    for c in &resp.chunks {
        out.push_str(&format!(
            "\n{}:{}-{}\n```\n{}\n```\n",
            path, c.start_line, c.end_line, c.code
        ));
    }
    // Coverage is per chunk, so the requested range can be answered in part. Say
    // which part, or the model cannot tell a complete read from a partial one.
    let covered_from = resp.chunks.first().map_or(start_line, |c| c.start_line);
    let covered_to = resp.chunks.last().map_or(end_line, |c| c.end_line);
    if covered_from > start_line || covered_to < end_line {
        out.push_str(&format!(
            "\n(Chunks cover lines {covered_from}-{covered_to}; the rest of \
             {start_line}-{end_line} has no indexed chunk.)\n"
        ));
    }
    out
}

fn format_list_files(glob: &str, resp: &ListFilesResponse) -> String {
    if resp.files.is_empty() {
        return format!(
            "No indexed file matches \"{glob}\". Note `*` matches across directories \
             here, so try a broader pattern like \"*name*\"."
        );
    }
    let mut out = format!(
        "{} file(s) matching \"{}\"{}:\n",
        resp.total,
        glob,
        if resp.files.len() as u64 == resp.total {
            String::new()
        } else {
            format!(", showing the first {}", resp.files.len())
        }
    );
    for f in &resp.files {
        out.push_str(&format!("{} [{}]\n", f.path, f.programming_language.name()));
    }
    out
}

/// The prompt-size ceiling for this run, or `None` until a turn has reported the
/// window in use.
fn context_ceiling(tally: &TokenTally, fraction: f64) -> Option<u64> {
    (tally.num_ctx > 0).then_some((tally.num_ctx as f64 * fraction) as u64)
}

/// What the run has spent so far, in the shape both `progress` and `done` carry.
fn snapshot(budget: Budget, steps: usize, elapsed: Duration, tally: &TokenTally) -> RunProgress {
    RunProgress {
        steps,
        max_steps: budget.max_steps,
        elapsed_ms: elapsed.as_millis() as u64,
        max_ms: budget.max_seconds.saturating_mul(1000),
        tokens: tally.prompt_tokens + tally.eval_tokens,
        max_tokens: budget.max_tokens,
        prompt_tokens: tally.prompt_tokens,
        eval_tokens: tally.eval_tokens,
        peak_prompt_tokens: tally.peak_prompt_tokens,
        num_ctx: tally.num_ctx,
        turns: tally.turns,
    }
}

/// What one executed action produced.
struct Executed {
    /// The `step` event's typed call.
    call: StepCall,
    hits: usize,
    /// The tool's reply, fed back into the transcript.
    text: String,
    /// Every location this result put in front of the model, for citation
    /// provenance. A path with no usable span (a `list_files` hit) carries `None`.
    shown: Vec<(String, Option<Span>)>,
}

/// Run one action against the index and format its result for the model.
///
/// Returns the `step` event's typed call and hit count, the text fed back as the
/// tool's reply, and the locations shown — so the loop stays control flow and the
/// per-action detail lives here.
async fn execute(
    tools: &dyn ResearchTools,
    params: &ResearchParams,
    action: &Action,
    token: &CancellationToken,
) -> Result<Executed, ResearchAbort> {
    match action {
        Action::Finalize => unreachable!("finalize never reaches execute"),
        Action::Note { .. } | Action::RevisePlan { .. } => {
            unreachable!("local actions go through apply_local")
        }
        Action::Search { query, path_prefix } => {
            let top_k = params.budget.search_top_k as usize;
            // A model-set prefix is a **post-filter over a widened result set**,
            // never an extra `include`. `include` is a widening list: appending to
            // it on a scoped run would let the model search its way *out* of the
            // caller's scope, which is the one thing this must not do. Over-fetch
            // and drop instead — one slightly larger Qdrant query, correct by
            // construction.
            let req = SearchRequest {
                query: query.clone(),
                top_k: Some(if path_prefix.is_some() {
                    top_k * PREFIX_OVERFETCH
                } else {
                    top_k
                }),
                include: params.scope.include.clone(),
                exclude: params.scope.exclude.clone(),
            };
            let mut result = match tools.search(req, token).await {
                Ok(results) => results,
                // An empty index answer is a finding, not a failure.
                Err(ApiError::NoMatch) => Vec::new(),
                Err(e) => return Err(e.into()),
            };
            if let Some(prefix) = path_prefix {
                result.retain(|r| r.path.starts_with(prefix.as_str()));
                result.truncate(top_k);
            }
            let text = format_search_results(query, &result);
            let shown = result
                .iter()
                .map(|r| {
                    (
                        r.path.clone(),
                        Some(Span {
                            start: r.start_line,
                            end: r.end_line,
                        }),
                    )
                })
                .collect();
            Ok(Executed {
                call: StepCall::Search {
                    query: query.clone(),
                },
                hits: result.len(),
                text,
                shown,
            })
        }
        Action::Symbols {
            name,
            role,
            kind,
            anchor_path,
        } => {
            let req = SymbolsRequest {
                name: name.clone(),
                role: *role,
                kind: kind.clone(),
                anchor_path: anchor_path.clone(),
                limit: Some(SYMBOLS_LIMIT),
                include: params.scope.include.clone(),
                exclude: params.scope.exclude.clone(),
            };
            let resp = tools
                .symbols(req, token)
                .await
                .map_err(ResearchAbort::from)?;
            let hits = (resp.total_definitions + resp.total_references) as usize;
            let text = format_symbols_response(name, &resp, &params.scope);
            // Both roles: a reference is as much "shown to you" as a definition.
            let shown = resp
                .definitions
                .iter()
                .chain(resp.references.iter())
                .map(|s| {
                    (
                        s.path.clone(),
                        Some(Span {
                            start: s.start_line,
                            end: s.end_line,
                        }),
                    )
                })
                .collect();
            Ok(Executed {
                call: StepCall::Symbols { name: name.clone() },
                hits,
                text,
                shown,
            })
        }
        Action::Outline { path } => {
            let resp = tools
                .outline(path.clone(), &params.scope, token)
                .await
                .map_err(ResearchAbort::from)?;
            let hits = resp.symbols.len();
            let text = format_outline(&resp, &params.scope);
            // The file itself, plus every definition's span. An outline of a file
            // that turned out not to be indexed shows nothing at all.
            let mut shown: Vec<(String, Option<Span>)> = Vec::new();
            if resp.indexed {
                shown.push((resp.path.clone(), None));
                shown.extend(resp.symbols.iter().map(|s| {
                    (
                        resp.path.clone(),
                        Some(Span {
                            start: s.start_line,
                            end: s.end_line,
                        }),
                    )
                }));
            }
            Ok(Executed {
                call: StepCall::Outline { path: path.clone() },
                hits,
                text,
                shown,
            })
        }
        Action::Callers { name, direction } => {
            let direction = direction.unwrap_or(CallDirection::In);
            let resp = tools
                .callers(name.clone(), direction, &params.scope, token)
                .await
                .map_err(ResearchAbort::from)?;
            let hits = resp.sites.len();
            let text = format_callers(&resp, &params.scope);
            // Each site is a real location the model was shown, so a citation to
            // it must verify. The span is a point: grouping collapses a pair's
            // occurrences to the first line, and claiming a range would assert
            // extent the query never measured.
            let shown = resp
                .sites
                .iter()
                .map(|s| {
                    (
                        s.path.clone(),
                        Some(Span {
                            start: s.first_line,
                            end: s.first_line,
                        }),
                    )
                })
                .collect();
            Ok(Executed {
                call: StepCall::Callers { name: name.clone() },
                hits,
                text,
                shown,
            })
        }
        Action::ListFiles { glob } => {
            let resp = tools
                .list_files(glob.clone(), &params.scope, token)
                .await
                .map_err(ResearchAbort::from)?;
            let hits = resp.files.len();
            let text = format_list_files(glob, &resp);
            // Paths only, no spans: learning that a file exists is not evidence
            // about any line in it, and the buckets must keep those apart.
            let shown = resp.files.iter().map(|f| (f.path.clone(), None)).collect();
            Ok(Executed {
                call: StepCall::ListFiles { glob: glob.clone() },
                hits,
                text,
                shown,
            })
        }
        Action::ReadChunks {
            path,
            start_line,
            end_line,
        } => {
            let resp = tools
                .read_chunks(path.clone(), *start_line, *end_line, &params.scope, token)
                .await
                .map_err(ResearchAbort::from)?;
            let hits = resp.chunks.len();
            let text = format_read_chunks(path, *start_line, *end_line, &resp, &params.scope);
            let shown = resp
                .chunks
                .iter()
                .map(|c| {
                    (
                        resp.path.clone(),
                        Some(Span {
                            start: c.start_line,
                            end: c.end_line,
                        }),
                    )
                })
                .collect();
            Ok(Executed {
                call: StepCall::ReadChunks { path: path.clone() },
                hits,
                text,
                shown,
            })
        }
        Action::Grep { pattern, glob } => {
            // Refused here rather than in the core, so the model is told *why* in the
            // vocabulary of the tool it called. Priced as a step like every other
            // refusal: a mistake that costs nothing is one a model will repeat.
            if pattern.trim().chars().count() < GREP_MIN_PATTERN_CHARS {
                return Ok(Executed {
                    call: StepCall::Grep {
                        pattern: pattern.clone(),
                    },
                    hits: 0,
                    text: format!(
                        "Not searched: \"{pattern}\" is shorter than \
                         {GREP_MIN_PATTERN_CHARS} characters, which would match \
                         everywhere and tell you nothing. Give a longer literal."
                    ),
                    shown: vec![],
                });
            }
            let resp = tools
                .grep(pattern.clone(), glob.clone(), &params.scope, token)
                .await
                .map_err(ResearchAbort::from)?;
            let hits = resp.matches.len();
            let text = format_grep(pattern, &resp, &params.scope);
            // The **chunk's** span, not the matching line: the chunk is what was
            // shown, and a one-line span would make a citation to the surrounding
            // code read as a range no tool returned.
            let shown = resp
                .matches
                .iter()
                .map(|m| {
                    (
                        m.path.clone(),
                        Some(Span {
                            start: m.start_line,
                            end: m.end_line,
                        }),
                    )
                })
                .collect();
            Ok(Executed {
                call: StepCall::Grep {
                    pattern: pattern.clone(),
                },
                hits,
                text,
                shown,
            })
        }
    }
}

/// Drives one research job to completion, pushing events into `tx`. Never
/// panics; every failure path emits an `Error` event (except cancellation,
/// which ends the stream quietly — the client is gone anyway).
pub async fn run_research(
    ollama: Arc<dyn OllamaModel>,
    tools: Arc<dyn ResearchTools>,
    journal: Arc<dyn ResearchJournal>,
    params: ResearchParams,
    tx: UnboundedSender<ResearchEvent>,
    token: CancellationToken,
) {
    let started = Instant::now();
    let ResearchOutcome {
        steps,
        reason,
        tally,
        citations,
        staleness,
        revalidation,
        tools: run_tools,
        report,
    } = match research_inner(&*ollama, &*tools, &params, &tx, &token).await {
        Ok(outcome) => outcome,
        Err(ResearchAbort::Cancelled) => {
            info!("Research cancelled (client disconnected).");
            return;
        }
        Err(ResearchAbort::Failed { code, detail }) => {
            warn!(%code, %detail, "Research job failed; emitting an error event.");
            let _ = tx.send(ResearchEvent::Error { code, detail });
            return;
        }
    };
    let progress = snapshot(params.budget, steps, started.elapsed(), &tally);
    // The per-run record: without it a research run leaves no trace beyond the
    // SSE stream, so only runs someone is watching can ever be measured.
    info!(
        model = %params.model,
        prompt_version = PROMPT_VERSION,
        steps,
        elapsed_ms = progress.elapsed_ms,
        reason = reason.as_str(),
        // A boolean field so a log query can count cut-short runs without
        // enumerating the reasons.
        truncated = reason.is_truncated(),
        turns = tally.turns,
        turns_unreported = tally.turns_unreported,
        prompt_tokens = tally.prompt_tokens,
        eval_tokens = tally.eval_tokens,
        peak_prompt_tokens = tally.peak_prompt_tokens,
        num_ctx = tally.num_ctx,
        context_used_pct = progress.context_pct(),
        binding = progress.binding(params.budget.context_fraction).as_str(),
        citations = citations.total,
        citations_verified = citations.verified,
        citations_unverified = citations.unverified,
        citations_stale = citations.stale,
        changed_files = staleness.changed_files,
        removed_files = staleness.removed_files,
        "Research run finished."
    );
    if staleness.changed_files + staleness.removed_files > 0 {
        // Not a failure: indexing has priority over research by design, and a run
        // that read a file which was then reindexed is the expected outcome of that
        // choice. Audible because it is the one thing that makes a report describe
        // code that no longer exists.
        info!(
            changed_files = staleness.changed_files,
            removed_files = staleness.removed_files,
            citations_stale = citations.stale,
            paths = ?citations.stale_paths,
            "The index changed under this run while it was reading."
        );
    }
    if citations.unverified > 0 {
        // Not an error — the report still ships — but the one failure mode scout's
        // "trust the report" instruction cannot absorb, so it must be audible.
        warn!(
            model = %params.model,
            unverified = citations.unverified,
            paths = ?citations.unverified_paths,
            "Research report cited paths no tool returned during the run; the model \
             invented them. Treat those citations as unsupported."
        );
    }
    if let Some(r) = revalidation {
        info!(
            draft_unverified = r.draft_unverified,
            draft_path_only = r.draft_path_only,
            draft_stale = r.draft_stale,
            revalidation_steps = r.steps,
            unverified = citations.unverified,
            path_only = citations.path_only,
            "The draft report's citations did not all check out; it was sent back \
             for correction."
        );
    }
    let _ = tx.send(ResearchEvent::Citations {
        report: citations.clone(),
        revalidation,
    });
    // Journalled after the events are queued, not before: the client's report must
    // never wait on a database write, and a write failure must not change what the
    // client saw.
    journal
        .record(RunRecord {
            question: params.question.clone(),
            model: params.model.clone(),
            prompt_version: PROMPT_VERSION,
            budget: params.budget,
            reason,
            steps,
            turns: tally.turns,
            elapsed_ms: progress.elapsed_ms,
            prompt_tokens: tally.prompt_tokens,
            eval_tokens: tally.eval_tokens,
            peak_prompt_tokens: tally.peak_prompt_tokens,
            num_ctx: tally.num_ctx,
            citations,
            staleness,
            revalidation,
            tools: run_tools,
            report,
        })
        .await;
    let _ = tx.send(ResearchEvent::Done {
        progress,
        context_fraction: params.budget.context_fraction,
        reason,
    });
}

enum ResearchAbort {
    Cancelled,
    Failed { code: String, detail: String },
}

impl ResearchAbort {
    /// What to log when this failure is being absorbed rather than raised.
    fn reason(&self) -> String {
        match self {
            ResearchAbort::Cancelled => "the run was cancelled".to_string(),
            ResearchAbort::Failed { code, detail } => format!("{code}: {detail}"),
        }
    }
}

impl From<OllamaError> for ResearchAbort {
    fn from(e: OllamaError) -> Self {
        match e {
            OllamaError::Cancelled => ResearchAbort::Cancelled,
            other => ResearchAbort::Failed {
                code: "ollama.unavailable".into(),
                detail: format!("The Ollama chat call failed: {other}"),
            },
        }
    }
}

impl From<ApiError> for ResearchAbort {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::Cancelled => ResearchAbort::Cancelled,
            other => ResearchAbort::Failed {
                code: other.code().to_string(),
                detail: format!("An index lookup failed during research ({}).", other.code()),
            },
        }
    }
}

/// Send an event; a closed channel means the SSE stream (and client) is gone.
fn send(
    tx: &UnboundedSender<ResearchEvent>,
    token: &CancellationToken,
    event: ResearchEvent,
) -> Result<(), ResearchAbort> {
    if tx.send(event).is_err() {
        token.cancel();
        return Err(ResearchAbort::Cancelled);
    }
    Ok(())
}

/// A child token that cancels itself after a fixed delay.
///
/// This is how a wall-clock budget becomes a *hard* deadline rather than a poll.
/// Cancellation is the one mechanism that reaches all the way down: `chat_stream`
/// selects on it both before the request and per streamed chunk, so dropping the
/// reqwest body aborts Ollama's generation (which is also what makes it unload the
/// model), and every `*_core` passes a child of it into its SQLite transaction and
/// its embedder call. A timeout local to one `await` would abort the same request at
/// the same instant and reach none of the rest.
///
/// A child of the parent, never a replacement for it: cancelling this leaves the
/// job's own token untouched, which is what lets the caller tell "my deadline fired"
/// apart from "the client left" (see [`stopped_by`]).
struct DeadlineToken {
    token: CancellationToken,
    /// Aborted on drop, so a run that finishes early does not leave an hour-long
    /// `sleep` parked on the research runtime for every request ever made.
    timer: tokio::task::JoinHandle<()>,
}

impl DeadlineToken {
    fn after(parent: &CancellationToken, delay: Duration) -> Self {
        let token = parent.child_token();
        let fire = token.clone();
        let parent = parent.clone();
        let timer = tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(delay) => fire.cancel(),
                // The client left first: nothing to fire, and nothing to wait for.
                _ = parent.cancelled() => {}
            }
        });
        Self { token, timer }
    }
}

impl Drop for DeadlineToken {
    fn drop(&mut self) {
        self.timer.abort();
    }
}

/// True when `which` fired on its own deadline rather than because the client left.
///
/// Order matters: a disconnect cancels the whole tree, so a cancelled child proves
/// nothing on its own — `job` has to be ruled out first.
fn stopped_by(job: &CancellationToken, which: &CancellationToken) -> bool {
    !job.is_cancelled() && which.is_cancelled()
}

/// Was this abort our own deadline expiring, rather than a real failure?
///
/// A deadline stop is not an error: the run keeps whatever it has found and moves on
/// to write about it. Only cancellation can be one — an Ollama or index failure is a
/// failure whatever the clock says.
fn is_deadline_stop(
    job: &CancellationToken,
    deadline: &CancellationToken,
    e: &ResearchAbort,
) -> bool {
    matches!(e, ResearchAbort::Cancelled) && stopped_by(job, deadline)
}

async fn research_inner(
    ollama: &dyn OllamaModel,
    index: &dyn ResearchTools,
    params: &ResearchParams,
    tx: &UnboundedSender<ResearchEvent>,
    token: &CancellationToken,
) -> Result<ResearchOutcome, ResearchAbort> {
    let started = Instant::now();
    let time_budget = Duration::from_secs(params.budget.max_seconds);
    // The investigation's deadline, as a token rather than only as a poll. The poll
    // below is still the graceful stop — it leaves a well-formed transcript with an
    // explicit "proceed to the report" turn — but a poll between turns cannot end a
    // turn that never returns, and until this existed nothing could: one Ollama call
    // may retry internally, and every phase after the tool loop used to run
    // unbudgeted.
    let budget_deadline = DeadlineToken::after(token, time_budget);
    let work = &budget_deadline.token;
    let mut messages = vec![
        ChatMessage::system(system_prompt(params.budget, &params.scope)),
        ChatMessage::user(format!("Research question:\n{}", params.question)),
    ];

    let mut steps = 0usize;
    let mut parse_retries = 0usize;
    let mut duplicate_calls = 0usize;
    // Tool-loop re-entries after the sufficiency check, capped by `MAX_REOPENS` —
    // the phase-level equivalent of the counters above.
    let mut reopens = 0usize;
    // Overwritten by whichever `break` below fires; `Finalized` is the only one
    // the model chooses on purpose.
    let mut reason = DoneReason::Finalized;
    let mut tally = TokenTally::default();
    let mut seen_calls: HashSet<String> = HashSet::new();
    // What the run has done, handed back to the model each turn.
    let mut state = RunState::default();
    // Where the run-state note currently sits, so it can be lifted out and
    // re-pinned rather than accumulating one stale copy per turn.
    let mut state_note_idx: Option<usize> = None;
    // Everything the tools show the model, so the report's citations can be
    // checked against what it was actually given rather than taken on trust.
    let mut evidence = Evidence::default();
    // The measurable half of this generation's three features; journalled per run.
    let mut run_tools = RunTools {
        report_window_ms: params.report_timeout_ms,
        ..RunTools::default()
    };
    // Search queries in arrival order, for near-duplicate rejection. A Vec, not a
    // set: the check is pairwise similarity, not membership.
    let mut seen_queries: Vec<String> = Vec::new();

    let tools = tool_specs();

    // Announce the budget before any work: a client can then render its meters
    // (and the caller can see what the run was actually granted) without waiting
    // for the first turn, which may take a minute.
    let emit_progress = |steps: usize, tally: &TokenTally| ResearchEvent::Progress {
        progress: snapshot(params.budget, steps, started.elapsed(), tally),
        context_fraction: params.budget.context_fraction,
    };
    send(tx, token, emit_progress(0, &tally))?;

    // ── plan turn ────────────────────────────────────────────────────────────
    //
    // No tools: the field is omitted entirely (`NO_TOOLS`), so there is
    // structurally nothing to call and the model can only answer in prose. Cheap
    // — the transcript is two messages long here, which is the other reason this
    // is the *first* turn rather than a mid-run checkpoint.
    //
    // Degrades to a plan-less run rather than failing: a plan is an aid, not a
    // contract, and a model that answers this badly can still answer the
    // question. `state.plan` staying `None` simply removes it from the note.
    messages.push(ChatMessage::user(PLAN_REQUEST.to_string()));
    let planned = chat_turn(
        ollama,
        params,
        &messages,
        NO_TOOLS,
        tx,
        work,
        TurnOpts {
            stream_content: false,
            sampling: params.sampling,
        },
    )
    .await;
    let plan = match planned {
        Ok(outcome) => tally.record(outcome).content,
        // The deadline fired before the run even had a plan — a pathological case
        // (this turn sees a two-message transcript), but it must not read as a
        // failure. An empty plan is already a supported state, and the tool loop's
        // own clock check turns the next iteration straight into the report.
        Err(e) if is_deadline_stop(token, work, &e) => String::new(),
        Err(e) => return Err(e),
    };
    if plan.trim().is_empty() {
        // Leave no dangling request: an unanswered question in the transcript
        // reads as one the model refused, which is not what happened.
        messages.pop();
        warn!("The model returned no plan; continuing without one.");
    } else {
        state.plan = Some(plan.clone());
        messages.push(ChatMessage::assistant(plan));
    }
    // No `progress` of its own: the cadence contract is "once before the first
    // turn, then after every executed step and every completed turn", and the
    // plan turn's cost lands in the first in-loop event. One more event here
    // would be a wire change bought for nothing.

    // ── tool loop ────────────────────────────────────────────────────────────
    //
    // One iteration = one model turn, which may ask for SEVERAL tools (native
    // tool calling allows it, and models use it).
    //
    // Termination rests on counters, not only on the clock: every iteration either
    // breaks or increments one of `steps`, `parse_retries`, `duplicate_calls`, each
    // capped. That is still the primary guarantee even though `max_seconds` is now a
    // hard deadline — the budgets stop a *productive* run that has spent enough,
    // while the counters stop a degenerate one, and a degenerate run that spins
    // inside its budget should be reported as `repeated_calls` rather than as a
    // timeout. A new `continue` still needs a new bounded counter; a refusal is
    // priced as a step instead, so that a mistake the model can repeat is never
    // free.
    //
    // The hard invariant: **every native tool call gets exactly one `role: "tool"`
    // reply**, in order — including calls that were rejected as duplicates or
    // skipped for budget. An assistant turn that announced N calls followed by
    // fewer results is a malformed transcript, and chat templates that pair them
    // up behave unpredictably on it.
    //
    // The outer `'phases` loop runs the tool loop, then the sufficiency check, and
    // re-enters at most `MAX_REOPENS` times when the model's own plan still has an
    // open item and there is budget left to close it. Its `continue` is bounded by
    // `reopens` for the same reason every `continue` inside is bounded.
    'phases: loop {
        'turns: loop {
            if steps >= params.budget.max_steps {
                messages.push(ChatMessage::user(
                    "Tool budget exhausted. Proceed to the final report.".to_string(),
                ));
                reason = DoneReason::BudgetExhausted;
                break;
            }
            if started.elapsed() >= time_budget {
                info!(
                    elapsed_s = started.elapsed().as_secs(),
                    max_seconds = params.budget.max_seconds,
                    steps,
                    "Research hit its wall-clock budget; asking for the report."
                );
                messages.push(ChatMessage::user(
                    "Time budget exhausted. Proceed to the final report.".to_string(),
                ));
                reason = DoneReason::TimeExhausted;
                break;
            }
            // The cost axis. Checked between turns like the others, and against the
            // *cumulative* sum on purpose: the whole transcript is resent every turn,
            // so this is what the run has actually made the GPU do.
            if tally.prompt_tokens + tally.eval_tokens >= params.budget.max_tokens {
                info!(
                    tokens = tally.prompt_tokens + tally.eval_tokens,
                    max_tokens = params.budget.max_tokens,
                    turns = tally.turns,
                    steps,
                    "Research hit its local-token budget; asking for the report."
                );
                messages.push(ChatMessage::user(
                    "Token budget exhausted. Proceed to the final report.".to_string(),
                ));
                reason = DoneReason::TokensExhausted;
                break;
            }
            // Checked *before* the next turn: `peak_prompt_tokens` is what the last
            // prompt actually measured, so this stops one turn short of the window
            // rather than after Ollama has already trimmed the transcript.
            if let Some(ceiling) = context_ceiling(&tally, params.budget.context_fraction)
                && tally.peak_prompt_tokens >= ceiling
            {
                info!(
                    peak_prompt_tokens = tally.peak_prompt_tokens,
                    ceiling,
                    num_ctx = tally.num_ctx,
                    "Research reached its share of the context window; asking for the report."
                );
                messages.push(ChatMessage::user(
                    "Context budget exhausted. Proceed to the final report.".to_string(),
                ));
                reason = DoneReason::ContextExhausted;
                break;
            }

            // Has the index moved under the evidence gathered so far? Probed here,
            // immediately before the note is rebuilt, so the freshness the model
            // reads is the freshness of this turn. Not a `continue` path and not a
            // step: it cannot affect loop termination or the budget.
            probe_freshness(index, &mut evidence, work).await;

            // Re-pin the run-state note: lift the previous copy out, then push a fresh
            // one as the last message before the turn. Pinning rather than appending
            // is the whole point — one copy, always adjacent to the generation point,
            // instead of a trail of stale ones decaying in the middle of a long
            // context. Removing it here (not after the turn) keeps it in front of the
            // model through the tool replies it explains.
            if let Some(i) = state_note_idx.take() {
                messages.remove(i);
            }
            messages.push(ChatMessage::user(format_state_note(
                &state,
                &evidence,
                &params.scope,
                &snapshot(params.budget, steps, started.elapsed(), &tally),
            )));
            state_note_idx = Some(messages.len() - 1);

            let turn = chat_turn(
                ollama,
                params,
                &messages,
                &tools,
                tx,
                work,
                TurnOpts {
                    stream_content: false,
                    sampling: params.sampling,
                },
            )
            .await;
            let outcome = match turn {
                Ok(o) => tally.record(o),
                // The deadline landed mid-turn, so this reply is gone. Nothing was
                // pushed for it, which is what makes stopping here safe: the
                // transcript is exactly as it was before the turn, and the report
                // phase reads it unchanged.
                Err(e) if is_deadline_stop(token, work, &e) => {
                    info!(
                        elapsed_s = started.elapsed().as_secs(),
                        max_seconds = params.budget.max_seconds,
                        steps,
                        "Research was cut off mid-turn by its wall-clock deadline."
                    );
                    reason = DoneReason::TimeExhausted;
                    break;
                }
                Err(e) => return Err(e),
            };
            // Per completed turn: this is where the token counts move.
            send(tx, token, emit_progress(steps, &tally))?;

            if outcome.tool_calls.is_empty() {
                // No tool call. Three cases, and telling them apart is the whole
                // diagnosis:
                if looks_like_tool_call_attempt(&outcome.content) {
                    // The model tried to call a tool in prose. Its Ollama template has
                    // no working tool support — an operator problem, so it must read
                    // like one instead of degrading into mysteriously poor reports.
                    return Err(ResearchAbort::Failed {
                        code: "research.model_lacks_tools".into(),
                        detail: format!(
                            "The model \"{}\" wrote a tool call as text instead of using the \
                         tool-calling API, which means its Ollama template does not \
                         support tools. Pick a model whose template does (check \
                         `ollama show <model>`); a `tools` capability alone is not \
                         proof, it is the template that matters.",
                            params.model
                        ),
                    });
                }
                if !outcome.content.trim().is_empty() {
                    // Prose. In a tool-calling loop that means "I am answering now",
                    // which is exactly what finalize means.
                    messages.push(ChatMessage::assistant(outcome.content.clone()));
                    reason = DoneReason::Finalized;
                    break;
                }
                // Empty: nothing to act on and nothing to report.
                messages.push(ChatMessage::assistant(outcome.content.clone()));
                if parse_retries < MAX_PARSE_RETRIES {
                    parse_retries += 1;
                    messages.push(ChatMessage::user(
                        "That reply was empty. Call one of the tools, or answer in prose \
                     if you already have enough evidence."
                            .to_string(),
                    ));
                    continue;
                }
                warn!("Model produced an empty reply repeatedly; forcing finalize.");
                reason = DoneReason::Unparseable;
                break;
            }

            messages.push(ChatMessage::assistant_calls(
                outcome.content.clone(),
                outcome.tool_calls.clone(),
            ));
            let batch: Vec<Option<Action>> =
                outcome.tool_calls.iter().map(action_from_call).collect();

            for (call_index, maybe_action) in batch.iter().enumerate() {
                // Every call must be answered, in order — see the invariant above.
                let reply = |messages: &mut Vec<ChatMessage>, text: String| {
                    let name = outcome.tool_calls[call_index].function.name.clone();
                    messages.push(ChatMessage::tool(name, text));
                };

                let Some(action) = maybe_action else {
                    // A call we could not map to an action: name unknown, or arguments
                    // of the wrong shape. Say so in the tool's own reply so the model
                    // can correct itself, and bound it like any other parse failure.
                    parse_retries += 1;
                    let asked = outcome.tool_calls[call_index].function.name.clone();
                    warn!(tool = %asked, "Model asked for a tool that does not exist or with bad arguments.");
                    reply(
                        &mut messages,
                        format!(
                            "No such tool, or wrong arguments: \"{asked}\". Available: search, \
                         grep, symbols, outline, callers, list_files, read_chunks, note, \
                         revise_plan, finalize."
                        ),
                    );
                    if parse_retries > MAX_PARSE_RETRIES {
                        reason = DoneReason::Unparseable;
                        break 'turns;
                    }
                    continue;
                };

                if matches!(action, Action::Finalize) {
                    reply(&mut messages, "Acknowledged.".to_string());
                    reason = DoneReason::Finalized;
                    break 'turns;
                }

                let call_key = match action {
                    Action::Finalize => unreachable!("handled above"),
                    Action::Grep { pattern, glob } => {
                        format!("grep\u{0}{}\u{0}{glob:?}", pattern.trim())
                    }
                    // An identical note is a wasted turn, so it is a duplicate like any
                    // other. A *revised* plan is not: replacing it with the same text is
                    // pointless but harmless, and the trimmed key catches that too.
                    Action::Note { text } => format!("note\u{0}{}", text.trim()),
                    Action::RevisePlan { plan } => format!("revise_plan\u{0}{}", plan.trim()),
                    Action::Search { query, path_prefix } => {
                        format!("search\u{0}{}\u{0}{path_prefix:?}", normalize_query(query))
                    }
                    Action::Symbols {
                        name,
                        role,
                        kind,
                        anchor_path,
                    } => {
                        format!("symbols\u{0}{name}\u{0}{role:?}\u{0}{kind:?}\u{0}{anchor_path:?}")
                    }
                    Action::Outline { path } => format!("outline\u{0}{}", path.trim()),
                    Action::Callers { name, direction } => {
                        format!("callers\u{0}{}\u{0}{direction:?}", name.trim())
                    }
                    Action::ListFiles { glob } => format!("list_files\u{0}{}", glob.trim()),
                    Action::ReadChunks {
                        path,
                        start_line,
                        end_line,
                    } => format!(
                        "read_chunks\u{0}{}\u{0}{start_line}\u{0}{end_line}",
                        path.trim()
                    ),
                };
                // Exact repeats are the liveness hazard; *near* repeats are the common
                // and expensive case. Measured: a run spent its whole budget asking for
                // `research_inner` six different ways ("fn research_inner", "fn
                // research_inner impl", …) — every key distinct, so nothing fired, and
                // every search cost a GPU embed to return almost the same five chunks.
                let near_duplicate = match action {
                    Action::Search { query, .. } => seen_queries
                        .iter()
                        .any(|prev| is_near_duplicate(prev, query)),
                    _ => false,
                };
                if !seen_calls.insert(call_key) || near_duplicate {
                    duplicate_calls += 1;
                    let detail = match action {
                        Action::Search { query, .. } if near_duplicate => format!(
                            "You already searched for something equivalent to \"{query}\" — its \
                         results are above. Rephrasing does not find new code. Get a real \
                         name with outline/list_files and search for that, read a location \
                         with read_chunks, or finalize."
                        ),
                        _ => "You already made exactly this call — its results are above. Ask \
                          something new, or finalize."
                            .to_string(),
                    };
                    reply(&mut messages, detail);
                    if duplicate_calls > MAX_DUPLICATE_CALLS {
                        warn!(
                            duplicate_calls,
                            "Model kept repeating tool calls it had already made; forcing finalize."
                        );
                        reason = DoneReason::RepeatedCalls;
                        break 'turns;
                    }
                    continue;
                }

                if steps >= params.budget.max_steps {
                    // Mid-batch exhaustion: answer the remaining calls honestly rather
                    // than leaving them unpaired, then stop.
                    reply(
                        &mut messages,
                        "Not executed: the tool budget is exhausted. Write the report from \
                     the evidence you have."
                            .to_string(),
                    );
                    reason = DoneReason::BudgetExhausted;
                    continue;
                }
                // The same shape for the clock. A turn may announce several calls and
                // the deadline can land between two of them, so the check belongs
                // here as well as at the top of the loop — and it must answer the
                // call rather than abandon it, or the assistant turn above ends up
                // with more calls than replies.
                if started.elapsed() >= time_budget || work.is_cancelled() {
                    reply(
                        &mut messages,
                        "Not executed: the time budget is exhausted. Write the report from \
                     the evidence you have."
                            .to_string(),
                    );
                    reason = DoneReason::TimeExhausted;
                    continue;
                }

                if let Action::Search { query, .. } = action {
                    seen_queries.push(query.clone());
                }

                // A local action needs no lookup and cannot fail, so it never touches
                // the deadline arms below. It is still charged a step: it is a real
                // decision the reader should see, and pricing it is what stops a model
                // from churning notes instead of investigating.
                if matches!(action, Action::Note { .. } | Action::RevisePlan { .. }) {
                    let Executed {
                        call,
                        hits,
                        text: result_text,
                        ..
                    } = apply_local(&mut state, action);
                    tally_tool_use(&mut run_tools, action, &result_text, hits);
                    steps += 1;
                    send(
                        tx,
                        token,
                        ResearchEvent::Step {
                            n: steps,
                            call,
                            hits,
                        },
                    )?;
                    send(tx, token, emit_progress(steps, &tally))?;
                    reply(&mut messages, result_text);
                    continue;
                }

                let executed = match execute(index, params, action, work).await {
                    Ok(x) => x,
                    // Cut off *inside* a lookup. Same treatment: reply, then stop
                    // after the batch. The step is deliberately not counted — nothing
                    // was returned, so charging it would report evidence the model
                    // never saw.
                    Err(e) if is_deadline_stop(token, work, &e) => {
                        reply(
                            &mut messages,
                            "Not executed: the time budget ran out during that lookup. \
                             Write the report from the evidence you have."
                                .to_string(),
                        );
                        reason = DoneReason::TimeExhausted;
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                steps += 1;
                state.record(action);
                let Executed {
                    call,
                    hits,
                    text: result_text,
                    shown,
                } = executed;
                tally_tool_use(&mut run_tools, action, &result_text, hits);
                for (path, span) in shown {
                    evidence.record(&path, span);
                }
                send(
                    tx,
                    token,
                    ResearchEvent::Step {
                        n: steps,
                        call,
                        hits,
                    },
                )?;
                // Per executed step: within a multi-call turn this is the only thing
                // that moves, and a long turn otherwise looks stalled.
                send(tx, token, emit_progress(steps, &tally))?;
                reply(&mut messages, result_text);
            }

            // A mid-batch exhaustion of either kind: the batch was answered in full,
            // so the transcript is well-formed and the run can stop.
            if matches!(
                reason,
                DoneReason::BudgetExhausted | DoneReason::TimeExhausted
            ) {
                break;
            }
        }

        // ── sufficiency check ────────────────────────────────────────────────────
        //
        // Only when there is a plan to check against: without one the question
        // "which sub-questions are still open?" has no referent, and asking it costs
        // a full transcript resend for a paragraph of invention.
        //
        // Two outcomes, both useful. If the model stopped on its own with an item
        // still open and a budget still to spend, the loop re-opens — that is the
        // measured failure of the models that finalize at four steps. Otherwise the
        // answer simply rides into the report, where "the evidence was insufficient"
        // becomes a specific list instead of a formality.
        //
        // Skipped entirely by a run a budget stopped. Both of its outcomes are
        // pointless there: re-entry is refused for a run with nothing left to spend,
        // and the list of open items is something the report turn is told to produce
        // from the plan anyway. It used to run unconditionally *and* unbudgeted,
        // which made it a full transcript resend charged to nobody.
        if state.plan.is_some() && reason == DoneReason::Finalized {
            messages.push(ChatMessage::user(SUFFICIENCY_REQUEST.to_string()));
            let checked = chat_turn(
                ollama,
                params,
                &messages,
                NO_TOOLS,
                tx,
                work,
                TurnOpts {
                    stream_content: false,
                    sampling: params.sampling,
                },
            )
            .await;
            let verdict = match checked {
                Ok(o) => tally.record(o).content,
                // The deadline caught the self-assessment. `reason` stays
                // `Finalized`: the model did decide it was done, and reporting a
                // time stop because its *postscript* was cut off would misdescribe
                // the run. An empty verdict is already handled below.
                Err(e) if is_deadline_stop(token, work, &e) => String::new(),
                Err(e) => return Err(e),
            };
            if verdict.trim().is_empty() {
                messages.pop();
            } else {
                messages.push(ChatMessage::assistant(verdict.clone()));
                let elapsed = started.elapsed();
                let spent = tally.prompt_tokens + tally.eval_tokens;
                // Re-entry only from a run that *chose* to stop. A run stopped by a
                // budget has nothing left to spend on the gap it just described, and
                // every axis is re-checked so a `Finalized` run that is nonetheless
                // out of road does not take it either.
                let room = steps < params.budget.max_steps
                    && elapsed < time_budget
                    && spent < params.budget.max_tokens;
                if reason == DoneReason::Finalized
                    && reopens < MAX_REOPENS
                    && room
                    && declares_unanswered(&verdict)
                {
                    reopens += 1;
                    info!(
                        steps,
                        reopens,
                        "The model finalized with its own plan unfinished; re-opening the tools."
                    );
                    // The one sentence about `revise_plan`, and this is the only place
                    // it belongs. The tool went unused in 28 measured runs, and the
                    // likelier reason than "never needed" is that nothing asked for it
                    // at the moment it fits: the plan is the run's only sufficiency
                    // criterion, so the turn that has just found the plan unfinished
                    // is exactly where "the plan asked the wrong question" becomes
                    // visible. Naming it here also makes the two explanations
                    // distinguishable on the next corpus. The "and only those" clause
                    // stays — this is still a nudge to close a gap, not a licence for
                    // a second investigation.
                    messages.push(ChatMessage::user(
                        "You stopped early: the items you just marked UNANSWERED \
                         are still open and you have budget left. The tools are \
                         open again. Close those items — and only those — then \
                         finalize. If an item is open because the plan asked the \
                         wrong question rather than because you ran out of \
                         evidence, replace the plan with `revise_plan` first, and \
                         close the item you meant to ask about."
                            .to_string(),
                    ));
                    continue 'phases;
                }
            }
        }
        break;
    }

    // ── draft report ─────────────────────────────────────────────────────────
    //
    // The report turn *replaces* the system prompt instead of appending one more
    // user message. Every prior turn demanded "exactly ONE JSON object, no prose",
    // and that conditioning wins: measured across three different models, roughly
    // one run in five answered the report request with another tool call. Ending
    // the tool protocol explicitly — a different role, with the tools declared
    // closed — is the fix; `parse_action` on the result is the backstop.
    //
    // This first pass is written with the content gate closed (`stream_content:
    // false`), so nothing reaches the client until its citations have been
    // checked. That costs the reader a silence where a stream used to be, and buys
    // the ability to send the report *back*: a claim cited to a location no tool
    // ever returned is otherwise fluent, authoritative and unchallengeable, which
    // is the one failure scout's "trust the report" instruction cannot absorb.
    // When the citations check out — the common case — the draft ships as-is, in
    // one event, and nothing is generated twice.
    // The last probe of the run: the citation check below, and the complaint it may
    // produce, must judge the report against the index as it is *now*, not as it was
    // when the last tool turn happened. The run-state note is deliberately not
    // rebuilt here — the report turn's instructions are fixed, and a change caught
    // this late is handled by sending the draft back rather than by an instruction
    // the model reads while writing.
    // The report phase gets a window of its own, and it is rooted in the **job**
    // token rather than in the deadline that may just have fired — a window parented
    // to the investigation's deadline would be dead before it opened, and every long
    // run would end in the server-written notice below. It exists because the run
    // that most needs to synthesise what it found is precisely the one that ran out
    // of time: taking the report out of `max_seconds` is what makes the deadline
    // safe to enforce.
    let report_started = Instant::now();
    let report_deadline =
        DeadlineToken::after(token, Duration::from_millis(params.report_timeout_ms));
    let writing = &report_deadline.token;
    probe_freshness(index, &mut evidence, writing).await;

    debug_assert_eq!(
        messages[0].role, "system",
        "the report turn replaces the system prompt in place; index 0 must be it"
    );
    messages[0] = ChatMessage::system(REPORT_SYSTEM_PROMPT);
    // The notes, pinned once for the report. The run-state *note* is deliberately not
    // rebuilt here, but these are different in kind: they are the conclusions the model
    // reached and chose to keep, and the report is what they were kept for. A message of
    // their own rather than folded into the instruction below, because that instruction
    // is fixed text under `PROMPT_VERSION` while this is the run's own content — and
    // because it must survive the rewrite turn's second instruction unchanged.
    if let Some(notes) = format_notes_note(&state) {
        messages.push(ChatMessage::user(notes));
    }
    messages.push(ChatMessage::user(report_request(
        reason,
        &state,
        params.budget,
        started.elapsed(),
    )));
    let drafted = write_report(
        ollama,
        params,
        &mut messages,
        tx,
        token,
        writing,
        &mut tally,
        false,
    )
    .await;
    // Whether the report the caller receives was written by the server rather than
    // by the model. Skips the citation-repair phase (there is neither time nor a
    // model left to ask) and is journalled, since "how often is the report window
    // too tight?" is not answerable from anything else.
    let mut forced = false;
    let mut summary = match drafted {
        Ok(ReportOutcome::Written(text)) => text,
        // The window expired before any report existed. The run still owes the caller
        // an answer, so the server writes an honest one from what the run actually
        // saw — a 200 stream that simply ends without a `summary` is the worst of the
        // available outcomes.
        Err(e) if is_deadline_stop(token, writing, &e) => {
            warn!(
                report_timeout_ms = params.report_timeout_ms,
                "The report window expired before the model produced a report; \
                 shipping a server-written account of the run instead."
            );
            forced = true;
            run_tools.forced_synthesis = true;
            forced_synthesis(
                params,
                &state,
                &evidence,
                reason,
                report_started.duration_since(started),
            )
        }
        Ok(failed) => return Err(failed.into_abort()),
        Err(e) => return Err(e),
    };

    // ── citation revalidation ────────────────────────────────────────────────
    let mut citations = check_citations(&summary, &evidence);
    let mut revalidation = None;
    // A report that grounds *nothing* is the third defect, and it was the one the
    // gate could not see: `citations: {total: 0}` is emitted for an ungrounded report
    // and is byte-for-byte what a clean one emits, in exactly the place a caller is
    // told to trust the report and check `unverified_paths` — which is empty here.
    // Measured 2026-07-30: 5 of 24 runs shipped one, and only one of the five used a
    // form (`(lines N-M)`) that widening `parse_citations` could have caught, so the
    // defect is the missing route into this gate rather than the parser.
    //
    // Two exemptions, both of which would otherwise turn the gate into a demand for
    // a fabrication:
    //  - a run no tool showed a single file **cannot** cite anything, and its "the
    //    question cannot be answered from this scope" report is the correct outcome,
    //    not a defect;
    //  - a report under `MIN_GROUNDED_REPORT_CHARS` is the short honest version of
    //    the same answer.
    let ungrounded = !forced
        && citations.total == 0
        && !evidence.paths().is_empty()
        && summary.chars().count() >= MIN_GROUNDED_REPORT_CHARS;
    // Staleness joins the two provenance defects in the gate: a claim cited to a
    // file the index has since rewritten is as unsupported as one cited nowhere,
    // and the remedy is the same — re-read it, then correct or drop the claim.
    if !forced && (ungrounded || citations.unverified + citations.path_only + citations.stale > 0) {
        if ungrounded {
            info!(
                report_chars = summary.chars().count(),
                shown_paths = evidence.paths().len(),
                "The draft report cites nothing checkable; sending it back to be grounded."
            );
        }
        let mut rv = Revalidation {
            draft_unverified: citations.unverified,
            draft_path_only: citations.path_only,
            draft_stale: citations.stale,
            steps: 0,
        };
        messages.push(ChatMessage::assistant(summary.clone()));

        // Tools re-open only for a run that chose to stop: one stopped by a budget
        // has nothing left to spend, and re-opening would buy an exhausted run a
        // second helping the budget axes exist to refuse. It can still correct or
        // drop the claim, which is the honest half of the fix.
        let tools_reopen = reason == DoneReason::Finalized;
        // The complaint goes out either way. Which citations failed is the whole
        // content of the instruction: told only "some did not check out", a model
        // can do nothing but guess, and guessing means rewriting the ones that
        // were right — the same reason the complaint names locations rather than
        // counts.
        if tools_reopen {
            messages[0] = ChatMessage::system(REVALIDATION_SYSTEM_PROMPT);
        }
        messages.push(ChatMessage::user(format_citation_complaint(
            &summary,
            &evidence,
            tools_reopen,
        )));
        if tools_reopen {
            let mut turns = 0usize;
            // Terminates on counters, like the tool loop: every iteration either
            // breaks or increments `turns`, and executed calls are capped
            // separately so a turn that executes nothing still costs one.
            while rv.steps < MAX_REVALIDATION_STEPS && turns < MAX_REVALIDATION_TURNS {
                turns += 1;
                // From here on a draft exists, so this phase's failures are
                // absorbed rather than raised: a repair is an improvement, and an
                // Ollama or index failure during it must cost the repair, not the
                // report. Cancellation is the exception — the client is gone, so
                // there is nobody left to ship to.
                let turn = chat_turn(
                    ollama,
                    params,
                    &messages,
                    &tools,
                    tx,
                    writing,
                    TurnOpts {
                        stream_content: false,
                        sampling: params.sampling,
                    },
                )
                .await;
                let outcome = match turn {
                    Ok(o) => tally.record(o),
                    // The report window expiring here is not a lost run: a draft
                    // exists, so stop repairing and go straight to shipping it.
                    Err(e) if is_deadline_stop(token, writing, &e) => {
                        warn!(
                            "The report window expired during the citation check; \
                               shipping what the draft says."
                        );
                        break;
                    }
                    Err(ResearchAbort::Cancelled) => return Err(ResearchAbort::Cancelled),
                    Err(e) => {
                        warn!(
                            reason = %e.reason(),
                            "The citation check could not finish; rewriting from what it has."
                        );
                        break;
                    }
                };
                if outcome.tool_calls.is_empty() {
                    // Prose here means "I have nothing to look up" — the same
                    // reading as in the tool loop. Push it so the rewrite sees
                    // whatever conclusion it reached.
                    if !outcome.content.trim().is_empty() {
                        messages.push(ChatMessage::assistant(outcome.content.clone()));
                    }
                    break;
                }
                messages.push(ChatMessage::assistant_calls(
                    outcome.content.clone(),
                    outcome.tool_calls.clone(),
                ));
                // The same hard invariant as the tool loop: one `role: "tool"`
                // reply per call, in order, including the ones that execute
                // nothing.
                for call in &outcome.tool_calls {
                    let name = call.function.name.clone();
                    let Some(action) = action_from_call(call) else {
                        messages.push(ChatMessage::tool(
                            name,
                            format!(
                                "No such tool, or wrong arguments: \"{}\".",
                                call.function.name
                            ),
                        ));
                        continue;
                    };
                    if matches!(action, Action::Finalize) || rv.steps >= MAX_REVALIDATION_STEPS {
                        messages.push(ChatMessage::tool(
                            name,
                            "Not executed: the check is over. Rewrite the report now.".to_string(),
                        ));
                        continue;
                    }
                    // A local action here reaches `execute`'s `unreachable!` otherwise —
                    // and the model can legitimately want to note what the re-read
                    // showed before rewriting. Free of the step budget, since this phase
                    // counts only lookups.
                    if matches!(action, Action::Note { .. } | Action::RevisePlan { .. }) {
                        let done = apply_local(&mut state, &action);
                        messages.push(ChatMessage::tool(name, done.text));
                        continue;
                    }
                    rv.steps += 1;
                    // The pairing invariant holds even here: a lookup that failed
                    // still owes this call a `role: "tool"` reply, and saying so is
                    // better than a transcript the chat template cannot read.
                    let executed = match execute(index, params, &action, writing).await {
                        Ok(x) => x,
                        Err(e) if is_deadline_stop(token, writing, &e) => {
                            messages.push(ChatMessage::tool(
                                name,
                                "Not executed: the report window is over. Rewrite the \
                                 report now."
                                    .to_string(),
                            ));
                            continue;
                        }
                        Err(ResearchAbort::Cancelled) => return Err(ResearchAbort::Cancelled),
                        Err(e) => {
                            warn!(
                                reason = %e.reason(),
                                "A citation-check lookup failed; the claim stays unverified."
                            );
                            messages.push(ChatMessage::tool(
                                name,
                                "Not executed: that lookup failed. Treat the citation as \
                                 unsupported."
                                    .to_string(),
                            ));
                            continue;
                        }
                    };
                    let Executed {
                        call: step_call,
                        hits,
                        text,
                        shown,
                    } = executed;
                    for (path, span) in shown {
                        evidence.record(&path, span);
                    }
                    // Numbered on from the investigation's steps so the client's
                    // step list stays monotonic, while the run's `steps` — the
                    // number the budget is measured against — stays within what it
                    // was granted.
                    send(
                        tx,
                        token,
                        ResearchEvent::Step {
                            n: steps + rv.steps,
                            call: step_call,
                            hits,
                        },
                    )?;
                    messages.push(ChatMessage::tool(name, text));
                }
            }
        }

        // ── rewrite ──────────────────────────────────────────────────────────
        messages[0] = ChatMessage::system(REPORT_SYSTEM_PROMPT);
        messages.push(ChatMessage::user(
            "Now write the final report. It replaces the draft entirely, so repeat \
             everything that should survive. Keep every claim the evidence supports, \
             fix the citations that did not check out — point them at a location a \
             tool actually returned — and drop any claim you could not ground. \
             Markdown only, no preamble, no JSON."
                .to_string(),
        ));
        // A failed rewrite must not lose a draft that was merely mis-cited: the
        // draft is still the better of the two answers, and its citation report
        // already tells the caller which parts to distrust. That covers a rewrite
        // that came back empty *and* one that could not be asked for at all.
        let rewritten = match write_report(
            ollama,
            params,
            &mut messages,
            tx,
            token,
            writing,
            &mut tally,
            true,
        )
        .await
        {
            Ok(ReportOutcome::Written(text)) => Some(text),
            Ok(_) => {
                warn!("The rewrite turn produced no report; shipping the draft as it stood.");
                None
            }
            // The window expiring mid-rewrite is the case this whole arm was
            // written for: a mis-cited report still beats none, and its
            // `citations` event says which parts to distrust.
            Err(e) if is_deadline_stop(token, writing, &e) => {
                warn!("The report window expired during the rewrite; shipping the draft.");
                None
            }
            Err(ResearchAbort::Cancelled) => return Err(ResearchAbort::Cancelled),
            Err(e) => {
                warn!(
                    reason = %e.reason(),
                    "The rewrite turn failed; shipping the draft as it stood."
                );
                None
            }
        };
        match rewritten {
            Some(text) => {
                summary = text;
                citations = check_citations(&summary, &evidence);
            }
            None => send(
                tx,
                token,
                ResearchEvent::Summary {
                    text: summary.clone(),
                },
            )?,
        }
        revalidation = Some(rv);
    } else {
        // Nothing streamed it — the gate was closed for the draft — so send it
        // whole.
        send(
            tx,
            token,
            ResearchEvent::Summary {
                text: summary.clone(),
            },
        )?;
    }

    run_tools.report_elapsed_ms = report_started.elapsed().as_millis() as u64;
    Ok(ResearchOutcome {
        steps,
        reason,
        tally,
        citations,
        staleness: evidence.staleness(),
        revalidation,
        tools: run_tools,
        report: summary,
    })
}

/// The message that sends a draft back: which of its citations failed, and why.
///
/// Names the offending locations rather than the counts. A model told "3 citations
/// are unverified" can only guess which, and guessing here means rewriting the
/// citations that were right.
///
/// `tools_open` only changes the closing instruction: a run stopped by a budget
/// gets the same list, since knowing *which* claims failed is what makes correcting
/// or dropping them possible, but it must not be told to go and look them up.
///
/// A report with no parseable citations at all takes the branch below rather than a
/// complaint of its own: there is no failing location to name, so the message has to
/// say what a citation *is* and which files this run may cite. Same function, so
/// there is one place a complaint is composed and one unit under [`PROMPT_VERSION`].
fn format_citation_complaint(report: &str, evidence: &Evidence, tools_open: bool) -> String {
    let parsed = parse_citations(report);
    if parsed.is_empty() {
        return format_ungrounded_complaint(evidence, tools_open);
    }
    let mut unverified = Vec::new();
    let mut path_only = Vec::new();
    let mut stale = Vec::new();
    for c in parsed {
        let entry = format!("{}:{}-{}", c.path, c.start, c.end);
        // Provenance first, then freshness: a citation that fails both is listed
        // once, under the defect that came first in the run. Listing it twice would
        // read as two separate problems with one claim.
        match evidence.verdict(&c) {
            Verdict::Verified => {
                if evidence.is_stale(&c.path) && !stale.contains(&entry) {
                    stale.push(entry);
                }
            }
            Verdict::PathOnly => {
                if !path_only.contains(&entry) {
                    path_only.push(entry);
                }
            }
            Verdict::Unverified => {
                if !unverified.contains(&entry) {
                    unverified.push(entry);
                }
            }
        }
    }
    let mut out = String::from(
        "Your draft is not published yet. Its citations were checked against the \
         locations your own tools returned in this run, and against the index as it \
         stands now. These did not pass:\n",
    );
    state_note_line(
        &mut out,
        "No tool this run returned this path at all — check whether the file exists \
         (list_files) and whether the claim is real",
        &unverified,
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "The file was shown, but not these lines — read them (read_chunks/outline) \
         or cite the range you actually saw",
        &path_only,
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "You were shown these lines, but the file has been reindexed since — the \
         code you read there may be gone, so the line numbers can now point at \
         something else",
        &stale,
        STATE_NOTE_MAX_ITEMS,
    );
    out.push_str(if tools_open {
        "\nUse the tools to settle them, then you will be asked to rewrite the \
         report. A claim you cannot ground must be dropped, not re-cited elsewhere.\n"
    } else {
        "\nThe budget is spent, so there is nothing left to look them up with. When \
         you are asked to rewrite the report, point each of these at a location a \
         tool did return in this run, or drop the claim it supports — do not \
         re-cite it elsewhere. For a file that was reindexed under you, say in the \
         report that the location may have moved rather than presenting it as \
         current.\n"
    });
    out
}

/// The message that sends back a draft which cites nothing a check can see.
///
/// Its content is different in kind from [`format_citation_complaint`]'s: there is no
/// failing location to name, so what the model needs is the *form* a citation takes
/// and the list of files it is entitled to cite. Both halves come from the measured
/// corpus (2026-07-30): of five ungrounded reports, four named real paths with no
/// line ranges at all and one wrote `(lines N-M)` beside the path — so this is
/// usually a formatting failure over evidence the transcript already holds, not a
/// model that looked at nothing, and it can be fixed without a single tool call.
fn format_ungrounded_complaint(evidence: &Evidence, tools_open: bool) -> String {
    let mut out = String::from(
        "Your draft is not published yet, and it grounds nothing: it contains no \
         citation in the one form that can be checked. A citation is \
         `path/to/file.rs:START-END` — the repo-relative path, a colon, and the line \
         range, in that exact shape. A bare filename, a range with no file, and \
         `(lines 12-30)` beside a path are all unverifiable, so a report built from \
         them reads as authoritative and supports nothing.\n\nEvery substantive claim \
         needs one. These are the files some tool actually showed you this run, and \
         the only ones you may cite — the line ranges you were given for them are in \
         this transcript:\n",
    );
    state_note_line(
        &mut out,
        "Shown to you this run",
        &evidence.paths(),
        STATE_NOTE_MAX_ITEMS,
    );
    out.push_str(if tools_open {
        "\nThe tools are open again for a few calls if you need to recover a line \
         range you no longer have. Then you will be asked to rewrite the report, with \
         each claim carrying the `path:START-END` it rests on; drop any claim you \
         cannot ground that way.\n"
    } else {
        "\nThe budget is spent, so there is nothing left to look anything up with — \
         the ranges above are already in this transcript. When you are asked to \
         rewrite the report, attach the `path:START-END` you were actually shown to \
         each claim, and drop any claim you cannot ground that way rather than \
         inventing a location for it.\n"
    });
    out
}

/// What a report turn produced.
///
/// The two failures stay apart because they mean different things to whoever
/// re-asks the question: a model that never filled its `final` channel and a model
/// that answered with one more tool call are different defects, and
/// `research.no_report` carries the distinction in its detail.
enum ReportOutcome {
    Written(String),
    /// Still empty after [`MAX_EMPTY_REPORT_RETRIES`].
    Empty,
    /// A tool call, twice, with no tools on offer.
    ToolCall,
}

impl ReportOutcome {
    /// The failure to raise when there is no earlier report to fall back on.
    fn into_abort(self) -> ResearchAbort {
        let detail = match self {
            // Unreachable in practice — the caller matches `Written` first — but a
            // panic here would cost a finished run, so it degrades instead.
            ReportOutcome::Written(_) | ReportOutcome::Empty => {
                "The model produced an empty final report."
            }
            ReportOutcome::ToolCall => {
                "The model emitted a tool call instead of the final report, twice. \
                 Re-ask the question."
            }
        };
        ResearchAbort::Failed {
            code: "research.no_report".into(),
            detail: detail.into(),
        }
    }
}

/// Ask for a report.
///
/// `stream_content` decides whether the reader sees it arrive: false for the draft
/// (nothing may reach the client before its citations are checked), true for the
/// rewrite that replaces it. Because the caller knows which, it also knows whether
/// it must send the text itself — see `is_withheld`.
#[allow(clippy::too_many_arguments)]
/// `job` is the run's own token — what a dead channel cancels, so a disconnect stops
/// the whole run. `writing` is the report window's, which bounds the model turns:
/// keeping them apart is what lets the caller tell "the window expired" (ship what we
/// have) from "the client left" (there is nobody to ship to).
async fn write_report(
    ollama: &dyn OllamaModel,
    params: &ResearchParams,
    messages: &mut Vec<ChatMessage>,
    tx: &UnboundedSender<ResearchEvent>,
    job: &CancellationToken,
    writing: &CancellationToken,
    tally: &mut TokenTally,
    stream_content: bool,
) -> Result<ReportOutcome, ResearchAbort> {
    // An empty reply to the report turn is not a model that has nothing to say: it
    // generated 1157-2273 tokens in the four measured cases and left `content`
    // empty, having written the whole report into its analysis channel (gpt-oss's
    // harmony format puts that in `thinking`) and never switched to `final`. The
    // work is done and thrown away at the last step, so retry it — same transcript,
    // next seed, exactly as the tool-call parse failure is handled a layer down.
    // Safe for the same reason: an empty content streamed nothing, so the client saw
    // no partial report. Thinking deltas *are* re-sent, which is a live-view
    // cosmetic, not a corrupted result.
    let mut report_attempt = 0;
    let mut summary = loop {
        let sampling = Sampling {
            seed: params
                .sampling
                .seed
                .map(|s| s.wrapping_add(report_attempt as i64)),
            ..params.sampling
        };
        let opts = TurnOpts {
            stream_content,
            sampling,
        };
        let content = tally
            .record(chat_turn(ollama, params, messages, NO_TOOLS, tx, writing, opts).await?)
            .content;
        if !content.trim().is_empty() || report_attempt >= MAX_EMPTY_REPORT_RETRIES {
            break content;
        }
        // The retry cap alone used to bound this loop, which meant six full report
        // turns could run after the run's time was already spent. The window is the
        // real bound; the cap is what stops a model that will never fill `final`.
        if writing.is_cancelled() {
            warn!("The report window expired between empty report attempts; giving up on a retry.");
            break content;
        }
        report_attempt += 1;
        warn!(
            attempt = report_attempt,
            seed = ?sampling.seed,
            "The model answered the report turn with an empty final channel; \
             asking again at the next seed."
        );
    };

    // A model sometimes answers the report turn with one more tool call — measured
    // at roughly one run in five across three different models, so this is the
    // protocol's fault, not one model's quirk. Recovery needs to know nothing
    // streamed: with the gate closed that is unconditional, and with it open the
    // content gate withheld it exactly when it opens with `{`.
    if (!stream_content || is_withheld(&summary)) && looks_like_tool_call_attempt(&summary) {
        warn!("Model answered the report turn with a tool call; asking once more.");
        messages.push(ChatMessage::assistant(summary.clone()));
        messages.push(ChatMessage::user(
            "That was a tool call. There are no tools left — it was not executed and \
             nothing more will be. Write the Markdown report now, starting with a \
             heading."
                .to_string(),
        ));
        summary = tally
            .record(
                chat_turn(
                    ollama,
                    params,
                    messages,
                    NO_TOOLS,
                    tx,
                    writing,
                    TurnOpts {
                        stream_content,
                        sampling: params.sampling,
                    },
                )
                .await?,
            )
            .content;
    }

    if summary.trim().is_empty() {
        return Ok(ReportOutcome::Empty);
    }
    if looks_like_tool_call_attempt(&summary) {
        warn!("Model answered the report turn with a tool call twice; giving up.");
        return Ok(ReportOutcome::ToolCall);
    }
    // Withheld but not a tool call — a report that genuinely opens with `{` (a JSON
    // code fence, say). The gate never streamed it, so emit it in one go rather
    // than losing it. Only meaningful when streaming was on: otherwise the caller
    // sends the whole thing itself.
    if stream_content && is_withheld(&summary) {
        send(
            tx,
            job,
            ResearchEvent::Summary {
                text: summary.clone(),
            },
        )?;
    }
    Ok(ReportOutcome::Written(summary))
}

/// One chat turn. Thinking deltas always stream out; content deltas stream out
/// only for the final report (`stream_content`).
/// What differs between turns of one run. `sampling` is a parameter rather than
/// read from [`ResearchParams`] because the report turn may be retried at a shifted
/// seed — the transcript stays identical, only the sampling moves.
#[derive(Clone, Copy)]
struct TurnOpts {
    /// Stream assistant content to the client as it arrives. False for tool turns,
    /// whose content is a decision, not prose for the reader.
    stream_content: bool,
    sampling: Sampling,
}

/// One chat turn, abandoned if the model runs away in its thinking channel.
///
/// The guard is a *volume* bound, not a time one, and that is the whole design. The
/// pathology — measured twice on `glm-4.7-flash`, once reproducibly — is a turn that
/// never leaves the thinking channel: tokens arrive fast and steadily for the entire
/// run, so the socket is healthy, `turn_timeout_ms` has nothing to expire on, and the
/// only thing that stops it is the deadline, by which point the run is spent. A
/// per-turn *time* bound was the obvious alternative and is the one that has already
/// failed twice: a cold opening turn (model load plus a ~98k-token KV allocation)
/// takes minutes while producing nothing at all, so a timer kills the healthy case and
/// a volume counter cannot. What that costs is earliness — the guard trips only once
/// the model has produced its way to the limit.
///
/// An abandoned turn is returned as an **empty** [`ChatOutcome`], which every caller
/// already handles, each in the way that suits it: the plan turn degrades to a
/// plan-less run, the tool loop charges a bounded parse retry, the sufficiency turn
/// drops its question, the report turn re-asks at a shifted seed. Inventing a sixth
/// recovery path would have been inventing five that already exist, and control-flow
/// wise a turn that was abandoned *is* a turn that produced nothing usable. It is not
/// silent: `warn!` plus a counter, because the return value cannot show it.
///
/// Its cost is invisible to `max_tokens`, unavoidably — Ollama's `done` line never
/// arrives for a cancelled turn, so `prompt_eval_count`/`eval_count` come back `None`
/// and the turn lands in `turns_unreported` having really made the GPU work.
async fn chat_turn(
    ollama: &dyn OllamaModel,
    params: &ResearchParams,
    messages: &[ChatMessage],
    tools: &[ToolSpec],
    tx: &UnboundedSender<ResearchEvent>,
    token: &CancellationToken,
    opts: TurnOpts,
) -> Result<ChatOutcome, ResearchAbort> {
    let stream_content = opts.stream_content;
    let mut send_failed = false;
    // A child of the caller's token, so the deadline and a client disconnect still
    // reach the turn — and so cancelling for runaway thinking stops *this* turn
    // without touching the run.
    let turn_token = token.child_token();
    let thinking_limit = params.max_turn_thinking_chars;
    let mut thinking_chars = 0usize;
    let mut runaway = false;
    // Content-gate state for the report turn. A reply whose first non-whitespace
    // character is `{` is almost certainly one more tool call, so it is buffered
    // instead of streamed: nothing reaches the client until the shape is known,
    // which is what makes a re-ask possible (see `is_withheld`). Prose streams
    // from the first delta, as before.
    let mut decided: Option<bool> = None;
    let mut pending = String::new();
    let outcome = ollama
        .chat_stream(
            &params.model,
            messages,
            tools,
            opts.sampling,
            &mut |delta| {
                let event = match delta {
                    ChatDelta::Thinking(text) => {
                        // Counted before it is forwarded: the client sees every delta
                        // that arrived, including the ones that tripped the guard.
                        if thinking_limit > 0 && !runaway {
                            thinking_chars += text.chars().count();
                            if thinking_chars >= thinking_limit {
                                runaway = true;
                                turn_token.cancel();
                            }
                        }
                        ResearchEvent::Thinking { text }
                    }
                    ChatDelta::Content(text) if stream_content => {
                        match decided {
                            Some(true) => ResearchEvent::Summary { text },
                            Some(false) => return, // withheld for validation
                            None => {
                                pending.push_str(&text);
                                let head = pending.trim_start();
                                if head.is_empty() {
                                    return; // whitespace so far, undecided
                                }
                                decided = Some(!head.starts_with('{'));
                                if decided == Some(false) {
                                    return;
                                }
                                ResearchEvent::Summary {
                                    text: std::mem::take(&mut pending),
                                }
                            }
                        }
                    }
                    ChatDelta::Content(_) => return,
                };
                if tx.send(event).is_err() {
                    // The SSE stream is gone; cancel so the Ollama call unwinds.
                    send_failed = true;
                    token.cancel();
                }
            },
            &turn_token,
        )
        .await;
    if send_failed {
        return Err(ResearchAbort::Cancelled);
    }
    let outcome = match outcome {
        Ok(o) => o,
        // Our own cancellation, not the caller's — the parent token decides which,
        // and it is tested first because a deadline or a disconnect cancels the whole
        // tree and must keep meaning what it means.
        Err(_) if runaway && !token.is_cancelled() => {
            warn!(
                thinking_chars,
                limit = thinking_limit,
                model = %params.model,
                "Abandoning a turn that ran away in its thinking channel: it streamed \
                 past the limit without producing a reply. The run continues without \
                 this turn. Hint: if this fires on healthy turns, raise \
                 [research].max_turn_thinking_chars or set it to 0."
            );
            if let Some(m) = &params.metrics {
                m.research.runaway_thinking_turns.inc();
            }
            // No counts and no window: Ollama's `done` line never arrived. `num_ctx`
            // of zero is why `TokenTally::record` refuses to overwrite a known window
            // with nothing.
            ChatOutcome {
                content: String::new(),
                tool_calls: Vec::new(),
                prompt_tokens: None,
                eval_tokens: None,
                num_ctx: 0,
            }
        }
        Err(e) => return Err(e.into()),
    };
    Ok(outcome)
}

/// Whether `chat_turn`'s content gate held this reply back instead of streaming
/// it — true exactly when the reply opens with `{`. The caller uses it to know a
/// re-ask is still safe: no `summary` delta of this text has left the server.
fn is_withheld(content: &str) -> bool {
    content.trim_start().starts_with('{')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// A model whose template cannot call tools writes the call as text. That is an
    /// operator problem — the wrong model — so it must surface as one instead of
    /// limping along on a second protocol.
    #[tokio::test]
    async fn a_call_written_as_text_fails_with_a_named_cause() {
        let events = drive(vec![("", r#"{"action":"search","query":"gc sweep"}"#)], 8).await;
        match events.last() {
            Some(ResearchEvent::Error { code, detail }) => {
                assert_eq!(code, "research.model_lacks_tools");
                assert!(detail.contains("fake"), "must name the model: {detail}");
                assert!(
                    detail.contains("template"),
                    "must point at the cause, not just fail: {detail}"
                );
            }
            other => panic!("expected research.model_lacks_tools, got {other:?}"),
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ResearchEvent::Step { .. })),
            "nothing may be executed from a text call: {events:?}"
        );
    }

    // ── the broken-template detector ──────────────────────────────────────────

    #[test]
    fn a_json_call_written_as_text_is_detected() {
        // Both spellings seen in the wild: our own old prompt's shape, and the
        // OpenAI-ish one a model imitates when its template has no tool support
        // (qwen2.5-coder:32b, which also mangled the name — which is exactly why
        // this is a detector and not a parser: guessing turns a model error into a
        // call with unpredictable arguments).
        assert!(looks_like_tool_call_attempt(
            r#"{"action":"search","query":"gc"}"#
        ));
        assert!(looks_like_tool_call_attempt(
            r#"{"name":"search Semantic code search over the index.","arguments":{"query":"x"}}"#
        ));
        // Noise around it must not hide it.
        assert!(looks_like_tool_call_attempt(
            "Sure!\n```json\n{\"action\":\"outline\",\"path\":\"a.rs\"}\n```"
        ));
        // Braces inside strings must not confuse the scan.
        assert!(looks_like_tool_call_attempt(
            r#"prefix {not json} then {"action":"search","query":"find {weird} braces"}"#
        ));
    }

    #[test]
    fn prose_and_unrelated_json_are_not_call_attempts() {
        assert!(!looks_like_tool_call_attempt(
            "The loop stops when duplicate_calls exceeds MAX_DUPLICATE_CALLS."
        ));
        assert!(!looks_like_tool_call_attempt("no json at all"));
        assert!(!looks_like_tool_call_attempt(r#"{"foo":"bar"}"#));
        // A report may legitimately contain a JSON example without a name/action.
        assert!(!looks_like_tool_call_attempt(
            r#"Config example: {"num_ctx": 65536, "stream": true}"#
        ));
    }

    // ── effort mapping ───────────────────────────────────────────────────────

    /// The ladder now lives in config, so the ordering that makes `effort` mean
    /// anything is asserted against the defaults.
    #[test]
    fn effort_budget_defaults_are_ordered_on_every_axis() {
        let b = crate::config::EffortBudgets::default();
        for axis in [
            (
                "max_seconds",
                b.low.max_seconds,
                b.medium.max_seconds,
                b.high.max_seconds,
            ),
            (
                "max_tokens",
                b.low.max_tokens,
                b.medium.max_tokens,
                b.high.max_tokens,
            ),
            (
                "max_steps",
                b.low.max_steps as u64,
                b.medium.max_steps as u64,
                b.high.max_steps as u64,
            ),
        ] {
            let (name, low, medium, high) = axis;
            assert!(
                low < medium && medium < high,
                "{name} must increase with effort"
            );
        }
        assert!(b.low.context_fraction < b.medium.context_fraction);
        assert!(b.medium.context_fraction < b.high.context_fraction);
        assert!(
            b.high.context_fraction <= 1.0,
            "a fraction of the window, not a multiple"
        );
    }

    #[test]
    fn a_partial_budget_override_keeps_the_preset_for_the_rest() {
        use crate::backend::v0::models::ResearchBudgetOverride;
        let preset = crate::config::EffortBudgets::default().medium;

        // No override at all: the preset, unchanged.
        let b = Budget::resolve(&preset, None);
        assert_eq!(
            (b.max_seconds, b.max_tokens, b.max_steps),
            (preset.max_seconds, preset.max_tokens, preset.max_steps)
        );

        // One axis named: only that axis moves. The point of the test — a caller
        // shortening a run must not silently deepen it on another axis.
        let b = Budget::resolve(
            &preset,
            Some(ResearchBudgetOverride {
                max_seconds: Some(30),
                ..Default::default()
            }),
        );
        assert_eq!(b.max_seconds, 30);
        assert_eq!(b.max_tokens, preset.max_tokens);
        assert_eq!(b.max_steps, preset.max_steps);

        // `context_fraction` is not overridable, so it always comes from config.
        let b = Budget::resolve(
            &preset,
            Some(ResearchBudgetOverride {
                max_seconds: Some(30),
                max_tokens: Some(1000),
                max_steps: Some(2),
            }),
        );
        assert_eq!(b.context_fraction, preset.context_fraction);
        assert_eq!((b.max_seconds, b.max_tokens, b.max_steps), (30, 1000, 2));
    }

    // ── the loop against fakes ───────────────────────────────────────────────

    /// Which toolless turn a fake is being asked for.
    ///
    /// Production knows by construction — it is the code that pushed the request.
    /// A fake has to read the transcript, and the last user message is exactly
    /// what tells them apart. Keeping the fakes phase-aware is what stops every
    /// scripted test from having to encode the plan and sufficiency turns in its
    /// script, where they are noise: those tests are about the tool loop.
    #[derive(PartialEq, Eq, Clone, Copy)]
    enum ToollessTurn {
        Plan,
        Sufficiency,
        Report,
    }

    fn toolless_turn(messages: &[ChatMessage]) -> ToollessTurn {
        match messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
        {
            Some(PLAN_REQUEST) => ToollessTurn::Plan,
            Some(SUFFICIENCY_REQUEST) => ToollessTurn::Sufficiency,
            _ => ToollessTurn::Report,
        }
    }

    /// A plan a fake hands back, in the shape the prompt asks for.
    const FAKE_PLAN: &str = "1. What ends the loop? — src/research.rs\n\
                             2. What does GC delete? — src/worker/gc.rs";
    /// A sufficiency verdict with nothing left open, so the default run does not
    /// re-enter the tool loop.
    const FAKE_VERDICT_COMPLETE: &str = "1. ANSWERED src/research.rs:10-20\n\
                                         2. ANSWERED src/worker/gc.rs:5-9";

    /// Scripted Ollama: pops the next reply per call; each reply is a
    /// (thinking, content) pair.
    struct FakeOllama {
        replies: Mutex<Vec<(&'static str, &'static str)>>,
    }

    #[async_trait]
    impl OllamaModel for FakeOllama {
        async fn chat_stream(
            &self,
            _model: &str,
            messages: &[ChatMessage],
            tools: &[ToolSpec],
            _sampling: Sampling,
            on_delta: &mut (dyn FnMut(ChatDelta) + Send),
            token: &CancellationToken,
        ) -> Result<ChatOutcome, OllamaError> {
            if token.is_cancelled() {
                return Err(OllamaError::Cancelled);
            }
            // Answered off-script: a scripted reply list is a script for the tool
            // loop, and charging it for the framing turns would make every test
            // here about the framing.
            if tools.is_empty() {
                let canned = match toolless_turn(messages) {
                    ToollessTurn::Plan => Some(FAKE_PLAN),
                    ToollessTurn::Sufficiency => Some(FAKE_VERDICT_COMPLETE),
                    ToollessTurn::Report => None,
                };
                if let Some(text) = canned {
                    on_delta(ChatDelta::Content(text.to_string()));
                    return Ok(ChatOutcome {
                        content: text.to_string(),
                        tool_calls: Vec::new(),
                        prompt_tokens: Some(100),
                        eval_tokens: Some(7),
                        num_ctx: 8192,
                    });
                }
            }
            let (thinking, content) = {
                let mut r = self.replies.lock().unwrap();
                if r.is_empty() {
                    return Err(OllamaError::Decode("script exhausted".into()));
                }
                r.remove(0)
            };
            if !thinking.is_empty() {
                on_delta(ChatDelta::Thinking(thinking.to_string()));
            }
            on_delta(ChatDelta::Content(content.to_string()));
            Ok(ChatOutcome {
                content: content.to_string(),
                tool_calls: Vec::new(),
                prompt_tokens: Some(100),
                eval_tokens: Some(7),
                num_ctx: 8192,
            })
        }

        async fn list_models(&self) -> Result<Vec<String>, OllamaError> {
            unreachable!("the research loop does not list models")
        }
    }

    /// Fake index: `search` returns one fixed hit, `symbols` one definition,
    /// `outline` two definitions of a known file, `list_files` one path.
    ///
    /// `file_versions` answers from `versions`, which a test mutates between turns
    /// to play the part of `mindex-index` writing to the index mid-run. Absent from
    /// the map = no row = the file has left the index. Default: every path this fake
    /// can return, at a fixed hash, so a run sees a corpus that holds still.
    struct FakeTools {
        search_calls: Mutex<Vec<String>>,
        grep_calls: Mutex<Vec<String>>,
        versions: Mutex<std::collections::HashMap<String, FileVersion>>,
        /// Scripted index writes, applied by `file_versions` before it answers.
        reindexes: Vec<Reindex>,
        /// Probes answered so far, which is what `Reindex::on_probe` counts. The
        /// loop probes only once it has evidence, so probe 1 lands between the first
        /// and second tool turns.
        probes: Mutex<usize>,
    }

    /// `mindex-index` landing mid-run: on the `on_probe`th freshness probe, this
    /// path's indexed hash becomes `sha256` — or, when that is `None`, its row
    /// disappears, which is how a delete and a soft-delete both read to a reader.
    struct Reindex {
        on_probe: usize,
        path: &'static str,
        sha256: Option<&'static str>,
    }

    /// Every path `FakeTools` can put in front of the model, at hash `v1`.
    fn stable_versions() -> std::collections::HashMap<String, FileVersion> {
        ["src/worker/gc.rs", "src/db/qdrant.rs"]
            .into_iter()
            .map(|p| {
                (
                    p.to_string(),
                    FileVersion {
                        path: p.to_string(),
                        sha256: "v1".into(),
                        in_flight: false,
                    },
                )
            })
            .collect()
    }

    impl Default for FakeTools {
        fn default() -> Self {
            FakeTools {
                grep_calls: Mutex::new(Vec::new()),
                search_calls: Mutex::new(Vec::new()),
                versions: Mutex::new(stable_versions()),
                reindexes: Vec::new(),
                probes: Mutex::new(0),
            }
        }
    }

    impl FakeTools {
        fn reindexing(reindexes: Vec<Reindex>) -> Self {
            FakeTools {
                reindexes,
                ..Default::default()
            }
        }
    }

    #[async_trait]
    impl ResearchTools for FakeTools {
        async fn search(
            &self,
            req: SearchRequest,
            _token: &CancellationToken,
        ) -> Result<Vec<SearchResult>, ApiError> {
            self.search_calls.lock().unwrap().push(req.query.clone());
            Ok(vec![SearchResult {
                score: 0.9,
                path: "src/worker/gc.rs".into(),
                code: "fn sweep() {}".into(),
                start_line: 10,
                end_line: 20,
                start_column: 0,
                end_column: 1,
            }])
        }

        async fn symbols(
            &self,
            _req: SymbolsRequest,
            _token: &CancellationToken,
        ) -> Result<SymbolsResponse, ApiError> {
            Ok(SymbolsResponse {
                definitions: vec![crate::backend::v0::models::SymbolInfo {
                    path: "src/db/qdrant.rs".into(),
                    kind: "function".into(),
                    start_line: 5,
                    end_line: 9,
                    start_column: 0,
                    end_column: 1,
                    parent_name: None,
                    parent_kind: None,
                    doc: None,
                }],
                references: vec![],
                total_definitions: 1,
                total_references: 0,
                out_of_scope_definitions: 0,
                out_of_scope_references: 0,
            })
        }

        async fn callers(
            &self,
            name: String,
            direction: CallDirection,
            _scope: &ToolScope,
            _token: &CancellationToken,
        ) -> Result<CallersResponse, ApiError> {
            // A path `stable_versions` knows, so a citation to it can verify and
            // the freshness probe has something to say about it.
            Ok(CallersResponse {
                name,
                direction,
                defined: true,
                sites: vec![crate::backend::v0::models::CallSite {
                    path: "src/worker/gc.rs".into(),
                    symbol: Some("sweep".into()),
                    kind: Some("function".into()),
                    first_line: 12,
                    occurrences: 2,
                }],
                total_sites: 1,
                total_references: 2,
                out_of_scope_sites: 0,
            })
        }

        async fn outline(
            &self,
            path: String,
            _scope: &ToolScope,
            _token: &CancellationToken,
        ) -> Result<OutlineResponse, ApiError> {
            // Only one path is "indexed", so a test can exercise both answers.
            if path != "src/worker/gc.rs" {
                return Ok(OutlineResponse {
                    path,
                    indexed: false,
                    in_scope: true,
                    programming_language: None,
                    symbols: vec![],
                    total_definitions: 0,
                });
            }
            Ok(OutlineResponse {
                path,
                indexed: true,
                in_scope: true,
                programming_language: Some(crate::backend::v0::models::ProgrammingLanguage::Rust),
                symbols: vec![
                    crate::backend::v0::models::OutlineSymbol {
                        name: "collect".into(),
                        kind: "function".into(),
                        start_line: 10,
                        end_line: 40,
                        parent_name: None,
                        parent_kind: None,
                        doc: Some("Sweeps deleted chunks.".into()),
                    },
                    crate::backend::v0::models::OutlineSymbol {
                        name: "sweep_candidates".into(),
                        kind: "function".into(),
                        start_line: 42,
                        end_line: 60,
                        parent_name: None,
                        parent_kind: None,
                        doc: None,
                    },
                ],
                total_definitions: 2,
            })
        }

        async fn list_files(
            &self,
            _glob: String,
            _scope: &ToolScope,
            _token: &CancellationToken,
        ) -> Result<ListFilesResponse, ApiError> {
            Ok(ListFilesResponse {
                files: vec![crate::backend::v0::models::FileListing {
                    path: "src/worker/gc.rs".into(),
                    programming_language: crate::backend::v0::models::ProgrammingLanguage::Rust,
                }],
                total: 1,
            })
        }

        /// One match in the same file the other fakes use, so a citation to it can
        /// verify.
        async fn grep(
            &self,
            pattern: String,
            _glob: Option<String>,
            _scope: &ToolScope,
            _token: &CancellationToken,
        ) -> Result<GrepResponse, ApiError> {
            self.grep_calls.lock().unwrap().push(pattern);
            Ok(GrepResponse {
                matches: vec![crate::backend::v0::models::GrepMatch {
                    path: "src/worker/gc.rs".into(),
                    start_line: 10,
                    end_line: 40,
                    match_line: 17,
                    excerpt: "let guard = GcGuard::new();".into(),
                }],
                total: 1,
                out_of_scope: 0,
            })
        }

        /// Covers 10-40 of `src/worker/gc.rs` and nothing else, so a test can ask
        /// for both the covered case and the sparse-coverage gap.
        async fn read_chunks(
            &self,
            path: String,
            start_line: usize,
            end_line: usize,
            _scope: &ToolScope,
            _token: &CancellationToken,
        ) -> Result<ReadChunksResponse, ApiError> {
            let indexed = path == "src/worker/gc.rs";
            let overlaps = indexed && start_line <= 40 && end_line >= 10;
            Ok(ReadChunksResponse {
                path,
                indexed,
                in_scope: true,
                chunks: if overlaps {
                    vec![crate::backend::v0::models::ChunkExcerpt {
                        start_line: 10,
                        end_line: 40,
                        code: "fn collect() {}".into(),
                    }]
                } else {
                    vec![]
                },
            })
        }

        async fn file_versions(
            &self,
            paths: Vec<String>,
            _token: &CancellationToken,
        ) -> Result<Vec<FileVersion>, ApiError> {
            let mut versions = self.versions.lock().unwrap();
            let probe = {
                let mut p = self.probes.lock().unwrap();
                *p += 1;
                *p
            };
            for r in self.reindexes.iter().filter(|r| r.on_probe == probe) {
                match r.sha256 {
                    Some(sha) => {
                        versions.insert(
                            r.path.to_string(),
                            FileVersion {
                                path: r.path.to_string(),
                                sha256: sha.into(),
                                in_flight: false,
                            },
                        );
                    }
                    None => {
                        versions.remove(r.path);
                    }
                }
            }
            Ok(paths
                .iter()
                .filter_map(|p| versions.get(p).cloned())
                .collect())
        }
    }

    fn params(max_steps: usize) -> ResearchParams {
        ResearchParams {
            question: "How does GC work?".into(),
            model: "fake".into(),
            scope: ToolScope::default(),
            budget: Budget {
                // Generous on the axes a step-focused test is not about, so only
                // the step cap can end these runs.
                max_seconds: 3600,
                max_tokens: u64::MAX,
                context_fraction: 1.0,
                max_steps,
                search_top_k: 5,
            },
            sampling: Sampling::default(),
            // Generous too: a test about the investigation must not be ended by the
            // report window, and the tests that *are* about the window set it.
            report_timeout_ms: 3_600_000,
            // Off unless a test is about the guard: the fakes emit a short thinking
            // delta every turn, and a guard armed by default would be a tripwire on
            // every one of these runs rather than a thing under test.
            max_turn_thinking_chars: 0,
            metrics: None,
        }
    }

    /// Event names with `progress` filtered out — for the tests that are about the
    /// *order of the model's* events. The cadence of `progress` itself is pinned by
    /// `full_session_search_symbols_finalize_summary` and
    /// `progress_is_emitted_before_the_first_turn_and_after_every_step`, so it is
    /// asserted somewhere without every ordering test having to restate it.
    fn names(events: &[ResearchEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(ResearchEvent::name)
            .filter(|n| *n != "progress")
            .collect()
    }

    /// Every `progress` event of a run, in order.
    fn progress_events(events: &[ResearchEvent]) -> Vec<RunProgress> {
        events
            .iter()
            .filter_map(|e| match e {
                ResearchEvent::Progress { progress, .. } => Some(*progress),
                _ => None,
            })
            .collect()
    }

    /// The `reason` of the run's `done` event, if it produced one.
    fn done_reason(events: &[ResearchEvent]) -> Option<DoneReason> {
        events.iter().find_map(|e| match e {
            ResearchEvent::Done { reason, .. } => Some(*reason),
            _ => None,
        })
    }

    async fn drive(
        replies: Vec<(&'static str, &'static str)>,
        max_steps: usize,
    ) -> Vec<ResearchEvent> {
        let ollama = Arc::new(FakeOllama {
            replies: Mutex::new(replies),
        });
        let tools = Arc::new(FakeTools::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_research(
            ollama,
            tools,
            Arc::new(NoJournal),
            params(max_steps),
            tx,
            CancellationToken::new(),
        )
        .await;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    #[tokio::test]
    async fn full_session_search_symbols_finalize_summary() {
        let events = drive_native_with(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("symbols", json!({"name": "collection_for"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report\n\nGC works by sweeping."],
            8,
        )
        .await;

        let names: Vec<&str> = events.iter().map(|e| e.name()).collect();
        // The full cadence, `progress` included: once up front (limits, nothing
        // spent), then after every completed turn and every executed step.
        assert_eq!(
            names,
            vec![
                "progress",
                "thinking",
                "progress",
                "step",
                "progress",
                "thinking",
                "progress",
                "step",
                "progress",
                "thinking",
                "progress",
                "summary",
                "citations",
                "done"
            ],
            "unexpected event sequence: {events:?}"
        );
        assert!(matches!(
            &events[3],
            ResearchEvent::Step {
                n: 1,
                call: StepCall::Search { .. },
                hits: 1,
            }
        ));
        assert!(matches!(
            &events[7],
            ResearchEvent::Step {
                n: 2,
                call: StepCall::Symbols { .. },
                hits: 1,
            }
        ));
        assert!(
            matches!(&events[11], ResearchEvent::Summary { text } if text.starts_with("# Report"))
        );
        // `citations` sits between the report and `done` — the verdict is about the
        // report, so it cannot be emitted before one exists.
        assert!(matches!(&events[12], ResearchEvent::Citations { .. }));
        assert!(
            matches!(
                &events[13],
                ResearchEvent::Done {
                    reason: DoneReason::Finalized,
                    ..
                }
            ),
            "a voluntary finalize must report itself as such: {events:?}"
        );
    }

    #[tokio::test]
    async fn duplicate_call_is_not_reexecuted() {
        let events = drive_native(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("search", json!({"query": "gc sweep"}))], // exact repeat
                vec![call("finalize", json!({}))],
            ],
            8,
        )
        .await;
        let steps = events.iter().filter(|e| e.name() == "step").count();
        assert_eq!(steps, 1, "the repeated call must not become a second step");
    }

    /// Always asks for the same tool call, forever — the liveness hazard.
    struct RepeatingOllama {
        call: ToolCall,
    }

    #[async_trait]
    impl OllamaModel for RepeatingOllama {
        async fn chat_stream(
            &self,
            _model: &str,
            _messages: &[ChatMessage],
            tools: &[ToolSpec],
            _sampling: Sampling,
            on_delta: &mut (dyn FnMut(ChatDelta) + Send),
            _token: &CancellationToken,
        ) -> Result<ChatOutcome, OllamaError> {
            if tools.is_empty() {
                // Report turn: answer with a report, or a different failure
                // (`research.no_report`) would mask the one under test.
                let text = "# Report\n\nBest effort.".to_string();
                on_delta(ChatDelta::Content(text.clone()));
                return Ok(ChatOutcome {
                    content: text,
                    tool_calls: Vec::new(),
                    prompt_tokens: None,
                    eval_tokens: None,
                    num_ctx: 8192,
                });
            }
            Ok(ChatOutcome {
                content: String::new(),
                tool_calls: vec![self.call.clone()],
                prompt_tokens: None,
                eval_tokens: None,
                num_ctx: 8192,
            })
        }

        async fn list_models(&self) -> Result<Vec<String>, OllamaError> {
            unreachable!("the research loop does not list models")
        }
    }

    /// Regression: rejected duplicates consume no tool budget, so only
    /// `MAX_DUPLICATE_CALLS` stops a model that repeats one call from looping
    /// forever. Before the cap this test never returned.
    #[tokio::test]
    async fn endlessly_repeated_call_terminates_the_loop() {
        let ollama = Arc::new(RepeatingOllama {
            call: call("search", json!({"query": "gc sweep"})),
        });
        let tools = Arc::new(FakeTools::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_research(
            ollama,
            tools.clone(),
            Arc::new(NoJournal),
            params(16),
            tx,
            CancellationToken::new(),
        )
        .await;

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert_eq!(
            events.iter().filter(|e| e.name() == "step").count(),
            1,
            "only the first call executes; the repeats are rejected: {events:?}"
        );
        assert_eq!(
            tools.search_calls.lock().unwrap().len(),
            1,
            "a rejected duplicate must never reach the index"
        );
        assert_eq!(
            events.last().map(|e| e.name()),
            Some("done"),
            "the run must finish, not spin: {events:?}"
        );
        assert_eq!(done_reason(&events), Some(DoneReason::RepeatedCalls));
    }

    // ── native tool calling ──────────────────────────────────────────────────

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: Some("call_x".into()),
            function: crate::models::ollama::CalledFunction {
                name: name.to_string(),
                arguments: args,
            },
        }
    }

    #[test]
    fn a_native_call_maps_onto_the_same_action_enum() {
        assert_eq!(
            action_from_call(&call("search", json!({"query": "gc sweep"}))),
            Some(Action::Search {
                query: "gc sweep".into(),
                path_prefix: None,
            })
        );
        // The optional narrowing argument, when the model supplies it.
        assert_eq!(
            action_from_call(&call(
                "search",
                json!({"query": "gc sweep", "path_prefix": "src/worker/"})
            )),
            Some(Action::Search {
                query: "gc sweep".into(),
                path_prefix: Some("src/worker/".into()),
            })
        );
        assert_eq!(
            action_from_call(&call("outline", json!({"path": "src/gc.rs"}))),
            Some(Action::Outline {
                path: "src/gc.rs".into()
            })
        );
        // A zero-argument tool may arrive with no arguments at all.
        assert_eq!(
            action_from_call(&call("finalize", Value::Null)),
            Some(Action::Finalize)
        );
        assert_eq!(
            action_from_call(&call("finalize", json!({}))),
            Some(Action::Finalize)
        );
    }

    #[test]
    fn a_mangled_tool_name_is_rejected_not_guessed() {
        // qwen2.5-coder:32b's template concatenated the tool's name and its
        // description. Taking the first token would have "fixed" it — and would
        // equally have accepted any other garbage that happens to start with a
        // valid name, turning a model error into a call with arguments nobody
        // checked. A wrong name is a wrong call.
        assert_eq!(
            action_from_call(&call(
                "search Semantic code search over the indexed project.",
                json!({"query": "x"})
            )),
            None
        );
    }

    #[test]
    fn an_unknown_tool_or_bad_arguments_is_rejected() {
        assert_eq!(action_from_call(&call("teleport", json!({}))), None);
        assert_eq!(
            action_from_call(&call("search", json!({"q": "wrong key"}))),
            None
        );
        assert_eq!(
            action_from_call(&call("search", json!("not an object"))),
            None
        );
    }

    #[test]
    fn every_action_has_a_tool_spec_and_vice_versa() {
        // A tool the model can call but the loop cannot execute (or the reverse)
        // is a silent dead end, so the two lists must match exactly.
        let mut names: Vec<&str> = tool_specs().iter().map(|t| t.function.name).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "callers",
                "finalize",
                "grep",
                "list_files",
                "note",
                "outline",
                "read_chunks",
                "revise_plan",
                "search",
                "symbols"
            ]
        );
        for name in &names {
            let args = match *name {
                "search" => json!({"query": "x"}),
                "symbols" => json!({"name": "x"}),
                "outline" => json!({"path": "x"}),
                "callers" => json!({"name": "x"}),
                "list_files" => json!({"glob": "x"}),
                "read_chunks" => json!({"path": "x", "start_line": 1, "end_line": 2}),
                "grep" => json!({"pattern": "xyz"}),
                "note" => json!({"text": "x"}),
                "revise_plan" => json!({"plan": "x"}),
                _ => json!({}),
            };
            assert!(
                action_from_call(&call(name, args)).is_some(),
                "tool {name} is offered but does not map to an Action"
            );
        }
    }

    /// Scripted native calls: the loop must execute every call of a multi-call
    /// turn, each as its own step.
    /// Scripted native model: one entry per tool turn (an empty entry = "answer in
    /// prose", i.e. finalize), plus what the report turn should reply.
    struct NativeOllama {
        turns: Mutex<Vec<Vec<ToolCall>>>,
        /// Consumed in order on report turns; the last one repeats. A JSON action
        /// here exercises the report-turn guard.
        reports: Mutex<Vec<&'static str>>,
        /// What every turn reports as its prompt size, against a fixed 8192-token
        /// window — the knob the context-budget test turns up.
        prompt_tokens: u64,
        /// Wall-clock each tool turn burns, so the time budget can be tested
        /// without waiting on a real model.
        turn_delay: Duration,
        /// Wall-clock a *report* turn burns. Separate from `turn_delay` because the
        /// report phase has a window of its own, and a test about that window must be
        /// able to overrun it without also overrunning the investigation's deadline.
        report_delay: Duration,
        /// The sufficiency verdict. Complete by default; a test that wants the
        /// tool loop re-opened sets one naming an UNANSWERED item.
        verdict: &'static str,
        /// The transcript of every turn that was offered tools, so a test can
        /// assert what the model was actually shown — which is the only way to
        /// check that a plan survives and that the state note is pinned rather
        /// than accumulated.
        transcripts: Mutex<Vec<Vec<ChatMessage>>>,
        /// Every `user` message the model was shown on a report turn, joined. Kept
        /// apart from `transcripts` (which holds tool turns only, so the indices
        /// stay the turn numbers) because the one thing a rewrite must be told —
        /// *which* citations failed — is only visible here.
        report_prompts: Mutex<Vec<String>>,
    }

    impl NativeOllama {
        fn new(turns: Vec<Vec<ToolCall>>, reports: Vec<&'static str>) -> Self {
            Self {
                turns: Mutex::new(turns),
                reports: Mutex::new(reports),
                prompt_tokens: 10,
                turn_delay: Duration::ZERO,
                report_delay: Duration::ZERO,
                verdict: FAKE_VERDICT_COMPLETE,
                transcripts: Mutex::new(Vec::new()),
                report_prompts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl OllamaModel for NativeOllama {
        async fn chat_stream(
            &self,
            _model: &str,
            messages: &[ChatMessage],
            tools: &[ToolSpec],
            _sampling: Sampling,
            on_delta: &mut (dyn FnMut(ChatDelta) + Send),
            _token: &CancellationToken,
        ) -> Result<ChatOutcome, OllamaError> {
            // A turn with no tools is one of the three framing turns — plan,
            // sufficiency, report — told apart by what was last asked, exactly as
            // production tells them apart by which of them it pushed.
            if tools.is_empty() {
                let kind = toolless_turn(messages);
                if kind == ToollessTurn::Report && !self.report_delay.is_zero() {
                    // Cancellable, unlike a bare sleep: the report window's token is
                    // what production would abort this turn with.
                    tokio::select! {
                        _ = tokio::time::sleep(self.report_delay) => {}
                        _ = _token.cancelled() => return Err(OllamaError::Cancelled),
                    }
                }
                let text = match kind {
                    ToollessTurn::Plan => FAKE_PLAN.to_string(),
                    ToollessTurn::Sufficiency => self.verdict.to_string(),
                    ToollessTurn::Report => {
                        self.report_prompts.lock().unwrap().push(
                            messages
                                .iter()
                                .filter(|m| m.role == "user")
                                .map(|m| m.content.as_str())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                        let mut r = self.reports.lock().unwrap();
                        if r.len() > 1 { r.remove(0) } else { r[0] }
                    }
                    .to_string(),
                };
                on_delta(ChatDelta::Content(text.clone()));
                return Ok(ChatOutcome {
                    content: text,
                    tool_calls: Vec::new(),
                    prompt_tokens: Some(10),
                    eval_tokens: Some(2),
                    num_ctx: 8192,
                });
            }
            self.transcripts.lock().unwrap().push(messages.to_vec());
            // The pairing invariant, asserted from the model's side: an assistant
            // turn that announced N calls must be followed by N tool replies.
            let announced: usize = messages
                .iter()
                .filter_map(|m| m.tool_calls.as_ref())
                .map(|c| c.len())
                .sum();
            let answered = messages.iter().filter(|m| m.role == "tool").count();
            assert_eq!(
                announced, answered,
                "every native call must get exactly one tool reply"
            );

            on_delta(ChatDelta::Thinking("picking a tool".into()));
            let calls = {
                let mut t = self.turns.lock().unwrap();
                if t.is_empty() {
                    Vec::new()
                } else {
                    t.remove(0)
                }
            };
            if calls.is_empty() {
                // No call: prose, which the loop reads as "I am answering now".
                let text = "Done looking.".to_string();
                on_delta(ChatDelta::Content(text.clone()));
                return Ok(ChatOutcome {
                    content: text,
                    tool_calls: Vec::new(),
                    prompt_tokens: Some(self.prompt_tokens),
                    eval_tokens: Some(2),
                    num_ctx: 8192,
                });
            }
            if !self.turn_delay.is_zero() {
                tokio::time::sleep(self.turn_delay).await;
            }
            Ok(ChatOutcome {
                content: String::new(),
                tool_calls: calls,
                prompt_tokens: Some(self.prompt_tokens),
                eval_tokens: Some(2),
                num_ctx: 8192,
            })
        }

        async fn list_models(&self) -> Result<Vec<String>, OllamaError> {
            unreachable!("the research loop does not list models")
        }
    }

    /// End to end: the fake search only ever returns `src/worker/gc.rs:10-20`, so a
    /// report citing anything else is a fabrication and must be reported as one.
    /// This is the failure scout's "trust the report, do not spot-check it"
    /// instruction cannot absorb on its own.
    #[tokio::test]
    async fn a_report_citing_what_no_tool_returned_is_flagged_on_the_stream() {
        let events = drive_native_with(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
            ],
            vec![
                "# Report\n\nThe sweep is in src/worker/gc.rs:12-18, and the claim rests \
                 on src/never_returned.rs:1-40.",
            ],
            8,
        )
        .await;
        let report = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations { report, .. } => Some(report.clone()),
                _ => None,
            })
            .expect("a citations event must precede done");
        assert_eq!(report.total, 2);
        // 12-18 overlaps the 10-20 the search returned.
        assert_eq!(report.verified, 1, "{report:?}");
        assert_eq!(report.unverified, 1, "{report:?}");
        assert_eq!(
            report.unverified_paths,
            vec!["src/never_returned.rs".to_string()]
        );
    }

    /// Indexing has priority over research and nothing serializes the two, so a
    /// file a run has read can be reindexed under it — and the transcript, which is
    /// the run's only memory, then holds notes about code that no longer exists.
    /// The run must say so *while it can still act on it*, which means in the
    /// pinned state note, not only in the final report.
    #[tokio::test]
    async fn a_file_reindexed_under_the_run_is_named_in_the_state_note() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("search", json!({"query": "qdrant delete batch"}))],
                vec![call("search", json!({"query": "sqlite pool pragma"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report\n\nFrom the evidence."],
        ));
        // Probe 1 establishes the baseline (it is the run's first sight of the
        // index's own hashes), so the write has to land on a later one to be a
        // *change* rather than the starting point.
        let tools = Arc::new(FakeTools::reindexing(vec![Reindex {
            on_probe: 2,
            path: "src/worker/gc.rs",
            sha256: Some("v2"),
        }]));
        run_native_with_tools(ollama.clone(), tools, params(8)).await;

        // Turn 1 saw a corpus that had not moved yet; turn 2 must be told it has.
        let first = turn_texts(&ollama, 0).join("\n");
        assert!(
            !first.contains("CHANGED in the index"),
            "nothing had changed before the first probe: {first}"
        );
        let third = turn_texts(&ollama, 2).join("\n");
        assert!(
            third.contains("CHANGED in the index"),
            "the note must name the change: {third}"
        );
        assert!(third.contains("src/worker/gc.rs"), "{third}");
    }

    /// A file that leaves the index entirely must read differently from one that was
    /// merely rewritten: there is nothing left to re-read, so the instruction is to
    /// stop citing it.
    #[tokio::test]
    async fn a_file_that_left_the_index_is_named_as_gone() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("search", json!({"query": "qdrant delete batch"}))],
                vec![call("search", json!({"query": "sqlite pool pragma"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report\n\nFrom the evidence."],
        ));
        let tools = Arc::new(FakeTools::reindexing(vec![Reindex {
            on_probe: 2,
            path: "src/worker/gc.rs",
            sha256: None,
        }]));
        run_native_with_tools(ollama.clone(), tools, params(8)).await;

        let third = turn_texts(&ollama, 2).join("\n");
        assert!(
            third.contains("LEFT the index") && third.contains("src/worker/gc.rs"),
            "{third}"
        );
    }

    /// The citation gate covers freshness as well as provenance: a location the
    /// model really was shown, in a file the index has since rewritten, is not
    /// evidence for a claim about the current code. It fails the draft, the
    /// complaint names it, and the counts reach the wire.
    #[tokio::test]
    async fn a_citation_into_a_reindexed_file_sends_the_draft_back() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report\n\nThe sweep is in src/worker/gc.rs:12-18."],
        ));
        // Probe 1 is the baseline, taken before the finalize turn; probe 2 is the
        // last one of the run, immediately before the report is written — which is
        // exactly the case this covers: the write landed with the evidence already
        // gathered and no turn left to notice it.
        let tools = Arc::new(FakeTools::reindexing(vec![Reindex {
            on_probe: 2,
            path: "src/worker/gc.rs",
            sha256: Some("v2"),
        }]));
        let events = run_native_with_tools(ollama.clone(), tools, params(8)).await;

        let (report, revalidation) = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations {
                    report,
                    revalidation,
                } => Some((report.clone(), *revalidation)),
                _ => None,
            })
            .expect("a citations event must precede done");
        // Provenance is impeccable — the search really did return 10-20 — and the
        // citation is still stale. The two verdicts are independent on purpose.
        assert_eq!(report.verified, 1, "{report:?}");
        assert_eq!(report.unverified, 0, "{report:?}");
        assert_eq!(report.stale, 1, "{report:?}");
        assert_eq!(report.stale_paths, vec!["src/worker/gc.rs".to_string()]);
        let rv = revalidation.expect("a stale citation must send the draft back");
        assert_eq!(rv.draft_stale, 1);
        let complaint = ollama.report_prompts.lock().unwrap().join("\n");
        assert!(
            complaint.contains("reindexed since") && complaint.contains("src/worker/gc.rs:12-18"),
            "the complaint must name the location, not the count: {complaint}"
        );
    }

    /// A run over a corpus that holds still must pay nothing for any of this: no
    /// staleness sections in the note, no repair pass, no stale citations.
    #[tokio::test]
    async fn a_corpus_that_holds_still_produces_no_staleness_anywhere() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("search", json!({"query": "qdrant delete batch"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report\n\nThe sweep is in src/worker/gc.rs:12-18."],
        ));
        let events =
            run_native_with_tools(ollama.clone(), Arc::new(FakeTools::default()), params(8)).await;

        let (report, revalidation) = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations {
                    report,
                    revalidation,
                } => Some((report.clone(), *revalidation)),
                _ => None,
            })
            .expect("a citations event must precede done");
        assert_eq!(report.stale, 0, "{report:?}");
        assert!(report.stale_paths.is_empty(), "{report:?}");
        assert!(revalidation.is_none(), "nothing to repair: {report:?}");
        let notes = turn_texts(&ollama, 1).join("\n");
        assert!(!notes.contains("CHANGED in the index"), "{notes}");
        assert!(!notes.contains("LEFT the index"), "{notes}");
        assert!(!notes.contains("being reindexed right now"), "{notes}");
    }

    /// The ledger's own rules, away from the loop: `changed` is sticky because the
    /// evidence in the transcript is already stale, `in_flight` is not because it
    /// describes what `search` can reach *now*, and "removed" may only be concluded
    /// about a path the probe actually asked about and had previously found.
    #[test]
    fn the_freshness_ledger_is_sticky_where_it_must_be_and_current_where_it_must_be() {
        let mut ev = Evidence::default();
        ev.record("src/a.rs", Some(Span { start: 1, end: 9 }));
        ev.record("src/b.rs", Some(Span { start: 1, end: 9 }));
        let v = |path: &str, sha: &str, in_flight: bool| FileVersion {
            path: path.into(),
            sha256: sha.into(),
            in_flight,
        };
        let asked = ev.paths();

        // First probe: a baseline, no verdicts.
        ev.apply_versions(
            &asked,
            &[v("src/a.rs", "v1", false), v("src/b.rs", "v1", false)],
        );
        assert!(ev.changed_paths().is_empty());
        assert!(!ev.is_stale("src/a.rs"));

        // `a` is being reindexed: in flight, not yet stale — nothing the run read
        // has been contradicted.
        ev.apply_versions(
            &asked,
            &[v("src/a.rs", "v1", true), v("src/b.rs", "v1", false)],
        );
        assert_eq!(ev.in_flight_paths(), vec!["src/a.rs".to_string()]);
        assert!(!ev.is_stale("src/a.rs"));

        // It lands. Now it is stale, and no longer in flight.
        ev.apply_versions(
            &asked,
            &[v("src/a.rs", "v2", false), v("src/b.rs", "v1", false)],
        );
        assert_eq!(ev.changed_paths(), vec!["src/a.rs".to_string()]);
        assert!(ev.in_flight_paths().is_empty());

        // Reindexed back to the original content: still stale. The run took its
        // notes from the intermediate version.
        ev.apply_versions(
            &asked,
            &[v("src/a.rs", "v1", false), v("src/b.rs", "v1", false)],
        );
        assert_eq!(ev.changed_paths(), vec!["src/a.rs".to_string()]);

        // A probe that did not cover `b` says nothing about `b`.
        ev.apply_versions(&["src/a.rs".to_string()], &[v("src/a.rs", "v1", false)]);
        assert!(ev.removed_paths().is_empty(), "{ev:?}");

        // Covered and absent: gone.
        ev.apply_versions(&asked, &[v("src/a.rs", "v1", false)]);
        assert_eq!(ev.removed_paths(), vec!["src/b.rs".to_string()]);
        assert!(ev.is_stale("src/b.rs"));

        // A path the probe never found has no baseline, so it cannot have "left".
        let mut fresh = Evidence::default();
        fresh.record("src/never_indexed.rs", None);
        let asked = fresh.paths();
        fresh.apply_versions(&asked, &[]);
        assert!(fresh.removed_paths().is_empty(), "{fresh:?}");
    }

    /// The liveness invariant, extended: a model that rephrases forever must hit
    /// the duplicate cap just as one that repeats verbatim does. Without this, the
    /// cap guards only the case that never actually happens.
    #[tokio::test]
    async fn endless_rephrasing_terminates_the_loop() {
        let events = drive_native_with(
            vec![
                vec![call(
                    "search",
                    json!({"query": "fn research_inner loop implementation"}),
                )],
                vec![call(
                    "search",
                    json!({"query": "fn research_inner loop impl"}),
                )],
                vec![call(
                    "search",
                    json!({"query": "fn research_inner loop body"}),
                )],
                vec![call(
                    "search",
                    json!({"query": "fn research_inner loop code"}),
                )],
                vec![call(
                    "search",
                    json!({"query": "fn research_inner loop source"}),
                )],
            ],
            vec!["# Report\n\nBest effort."],
            50,
        )
        .await;
        let done = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Done { reason, .. } => Some(*reason),
                _ => None,
            })
            .expect("the run must end");
        assert_eq!(done, DoneReason::RepeatedCalls, "{events:?}");
        // Only the first query was ever executed; the rest cost no step.
        let steps = events
            .iter()
            .filter(|e| matches!(e, ResearchEvent::Step { .. }))
            .count();
        assert_eq!(steps, 1, "rephrasings must not become steps: {events:?}");
    }

    /// `read_chunks` exists because the loop could learn a location and then had
    /// no way to read it — measured, a model searched for the literal string
    /// `"src/research.rs:445-624 research_inner"`.
    #[tokio::test]
    async fn read_chunks_returns_the_code_at_a_location() {
        let events = drive_native(
            vec![
                vec![call(
                    "read_chunks",
                    json!({"path": "src/worker/gc.rs", "start_line": 12, "end_line": 30}),
                )],
                vec![call("finalize", json!({}))],
            ],
            8,
        )
        .await;
        let step = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Step { call, hits, .. } => Some((call.clone(), *hits)),
                _ => None,
            })
            .expect("the read must become a step");
        assert_eq!(step.0.action(), "read_chunks");
        assert_eq!(step.1, 1);
    }

    /// The sparse-coverage case, which is the whole reason this tool reports
    /// gaps: an indexed file whose requested lines have no chunk must not read as
    /// "those lines are empty", or the model concludes the code does not exist.
    #[test]
    fn an_uncovered_range_says_so_instead_of_returning_nothing() {
        let text = format_read_chunks(
            "src/gc.rs",
            5,
            8,
            &crate::backend::v0::models::ReadChunksResponse {
                path: "src/gc.rs".into(),
                indexed: true,
                in_scope: true,
                chunks: vec![],
            },
            &ToolScope::default(),
        );
        assert!(text.contains("The file IS"), "{text}");
        assert!(!text.contains("not an indexed file"), "{text}");
        // A wrong path must read differently from a sparse range.
        let missing = format_read_chunks(
            "src/nope.rs",
            1,
            2,
            &crate::backend::v0::models::ReadChunksResponse {
                path: "src/nope.rs".into(),
                indexed: false,
                in_scope: true,
                chunks: vec![],
            },
            &ToolScope::default(),
        );
        assert!(missing.contains("not an indexed file"), "{missing}");
    }

    async fn drive_native(turns: Vec<Vec<ToolCall>>, max_steps: usize) -> Vec<ResearchEvent> {
        drive_native_with(turns, vec!["# Report\n\nFrom the evidence."], max_steps).await
    }

    async fn drive_native_with(
        turns: Vec<Vec<ToolCall>>,
        reports: Vec<&'static str>,
        max_steps: usize,
    ) -> Vec<ResearchEvent> {
        run_native(NativeOllama::new(turns, reports), params(max_steps)).await
    }

    /// Drive the loop with a fully specified model fake and budget — the entry
    /// point for the tests about *which* budget ends a run.
    async fn run_native(ollama: NativeOllama, params: ResearchParams) -> Vec<ResearchEvent> {
        run_native_shared(Arc::new(ollama), params).await
    }

    /// As `run_native`, but the caller keeps the fake — the tests that inspect
    /// `transcripts` need it after the run.
    async fn run_native_shared(
        ollama: Arc<NativeOllama>,
        params: ResearchParams,
    ) -> Vec<ResearchEvent> {
        run_native_with_tools(ollama, Arc::new(FakeTools::default()), params).await
    }

    /// As `run_native_shared`, but the caller supplies the index fake — the entry
    /// point for the tests where the index changes under the run.
    async fn run_native_with_tools(
        ollama: Arc<NativeOllama>,
        tools: Arc<FakeTools>,
        params: ResearchParams,
    ) -> Vec<ResearchEvent> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_research(
            ollama,
            tools,
            Arc::new(NoJournal),
            params,
            tx,
            CancellationToken::new(),
        )
        .await;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    /// A model that runs away in its thinking channel on its **first** turn — the
    /// plan turn, which is where it was measured — and behaves normally afterwards.
    ///
    /// `deltas` bounds the fake so a guard that never fires fails the test by
    /// assertion rather than by hanging the suite forever, which is what the real
    /// pathology does.
    struct WedgedThinker {
        calls: Mutex<usize>,
        deltas: usize,
        chunk: usize,
        reply: &'static str,
    }

    #[async_trait]
    impl OllamaModel for WedgedThinker {
        async fn chat_stream(
            &self,
            _model: &str,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _sampling: Sampling,
            on_delta: &mut (dyn FnMut(ChatDelta) + Send),
            token: &CancellationToken,
        ) -> Result<ChatOutcome, OllamaError> {
            let n = {
                let mut c = self.calls.lock().unwrap();
                *c += 1;
                *c
            };
            if n == 1 {
                let text = "x".repeat(self.chunk);
                for _ in 0..self.deltas {
                    on_delta(ChatDelta::Thinking(text.clone()));
                    // The guard cancels from inside `on_delta`, so this is observable
                    // immediately — exactly as dropping the reqwest body is in
                    // production.
                    if token.is_cancelled() {
                        return Err(OllamaError::Cancelled);
                    }
                }
            }
            on_delta(ChatDelta::Content(self.reply.to_string()));
            Ok(ChatOutcome {
                content: self.reply.to_string(),
                tool_calls: Vec::new(),
                prompt_tokens: Some(10),
                eval_tokens: Some(2),
                num_ctx: 8192,
            })
        }

        async fn list_models(&self) -> Result<Vec<String>, OllamaError> {
            unreachable!("the research loop does not list models")
        }
    }

    fn thinking_chars(events: &[ResearchEvent]) -> usize {
        events
            .iter()
            .filter_map(|e| match e {
                ResearchEvent::Thinking { text } => Some(text.chars().count()),
                _ => None,
            })
            .sum()
    }

    async fn run_model(ollama: Arc<dyn OllamaModel>, params: ResearchParams) -> Vec<ResearchEvent> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_research(
            ollama,
            Arc::new(FakeTools::default()),
            Arc::new(NoJournal),
            params,
            tx,
            CancellationToken::new(),
        )
        .await;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    /// The guard abandons the runaway turn and the run carries on. The whole point:
    /// the measured failure spent an entire 900 s budget on one turn that never left
    /// its thinking channel, and the deadline could only report that afterwards.
    #[tokio::test]
    async fn a_turn_that_runs_away_in_its_thinking_channel_is_abandoned_not_the_run() {
        let ollama = Arc::new(WedgedThinker {
            calls: Mutex::new(0),
            deltas: 20,
            chunk: 100,
            reply: "# Report\n\nDone.",
        });
        let mut p = params(8);
        p.max_turn_thinking_chars = 500;
        let events = run_model(ollama, p).await;

        assert!(
            thinking_chars(&events) < 1000,
            "the turn must be cut off near the limit, not run to the fake's end: {} chars",
            thinking_chars(&events)
        );
        assert_eq!(
            done_reason(&events),
            Some(DoneReason::Finalized),
            "the run must survive the abandoned turn: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ResearchEvent::Summary { .. })),
            "and still report: {events:?}"
        );
    }

    /// `0` means off, and off means the turn is left alone however long it thinks —
    /// the escape hatch for a model whose thinking really is that verbose.
    #[tokio::test]
    async fn a_zero_thinking_limit_leaves_the_turn_alone() {
        let ollama = Arc::new(WedgedThinker {
            calls: Mutex::new(0),
            deltas: 20,
            chunk: 100,
            reply: "# Report\n\nDone.",
        });
        let mut p = params(8);
        p.max_turn_thinking_chars = 0;
        let events = run_model(ollama, p).await;

        assert_eq!(
            thinking_chars(&events),
            2000,
            "every delta the model produced must reach the client: {events:?}"
        );
        assert_eq!(done_reason(&events), Some(DoneReason::Finalized));
    }

    /// Every message the model was shown on its `n`th tool turn.
    fn turn_texts(ollama: &NativeOllama, n: usize) -> Vec<String> {
        ollama.transcripts.lock().unwrap()[n]
            .iter()
            .map(|m| m.content.clone())
            .collect()
    }

    /// The plan is asked for before any tool and pushed back as an *assistant*
    /// message, which is the only channel that survives a turn: `ChatMessage` has
    /// no `thinking` field, so a thinking model's own plan is discarded after
    /// every turn and re-derived from raw tool output — the loop this moves it out
    /// of.
    #[tokio::test]
    async fn the_plan_survives_into_every_later_turn() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report"],
        ));
        run_native_shared(ollama.clone(), params(8)).await;

        let turns = ollama.transcripts.lock().unwrap().len();
        assert!(turns >= 2, "expected at least two tool turns, got {turns}");
        for n in 0..turns {
            let texts = turn_texts(&ollama, n);
            assert!(
                texts.iter().any(|t| t == FAKE_PLAN),
                "turn {n} lost the plan: {texts:?}"
            );
        }
    }

    /// The run-state note is *pinned*: lifted out and re-pushed each turn, so the
    /// model always sees exactly one — adjacent to where it generates — instead of
    /// a trail of stale copies buried in a growing transcript.
    #[tokio::test]
    async fn the_state_note_is_pinned_once_and_lists_what_was_already_asked() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("outline", json!({"path": "src/worker/gc.rs"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report"],
        ));
        run_native_shared(ollama.clone(), params(8)).await;

        let turns = ollama.transcripts.lock().unwrap().len();
        for n in 0..turns {
            let notes = turn_texts(&ollama, n)
                .into_iter()
                .filter(|t| t.starts_with("Run state (maintained by the server"))
                .count();
            assert_eq!(notes, 1, "turn {n} should carry exactly one state note");
        }
        // By the second turn the note is a memory, not an empty header: the search
        // that just executed is named, which is what stops the model re-asking it.
        let second = turn_texts(&ollama, 1)
            .into_iter()
            .find(|t| t.starts_with("Run state (maintained by the server"))
            .expect("a state note");
        assert!(second.contains("gc sweep"), "{second}");
        assert!(second.contains("src/worker/gc.rs"), "{second}");
    }

    /// A model that finalizes with its own plan unfinished is sent back — once.
    /// The bound matters as much as the behaviour: re-entry is a `continue` at the
    /// phase level, and an unbounded one is the same liveness hazard the duplicate
    /// cap exists for.
    #[tokio::test]
    async fn an_unfinished_plan_reopens_the_loop_at_most_once() {
        let mut fake = NativeOllama::new(
            vec![
                vec![call("finalize", json!({}))],
                // Served only if the loop really re-opens.
                vec![call("search", json!({"query": "sweep_candidates"}))],
                vec![call("finalize", json!({}))],
                // Would be served on a second re-entry, which must not happen.
                vec![call("search", json!({"query": "prune_deleted_files"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report"],
        );
        fake.verdict = "1. UNANSWERED — nothing found yet";
        let ollama = Arc::new(fake);
        let events = run_native_shared(ollama.clone(), params(8)).await;

        let queries: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ResearchEvent::Step {
                    call: StepCall::Search { query },
                    ..
                } => Some(query.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            queries,
            vec!["sweep_candidates".to_string()],
            "exactly one re-entry: {events:?}"
        );
        assert_eq!(done_reason(&events), Some(DoneReason::Finalized));
    }

    /// A run stopped by a budget has nothing left to spend, so its plan's open
    /// items cannot be closed — the verdict rides into the report instead.
    #[tokio::test]
    async fn a_budget_stopped_run_is_not_reopened() {
        let mut fake = NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("search", json!({"query": "qdrant delete_batch"}))],
            ],
            vec!["# Report"],
        );
        fake.verdict = "1. UNANSWERED — out of road";
        let ollama = Arc::new(fake);
        let events = run_native_shared(ollama.clone(), params(2)).await;

        assert_eq!(done_reason(&events), Some(DoneReason::BudgetExhausted));
        let steps = events
            .iter()
            .filter(|e| matches!(e, ResearchEvent::Step { .. }))
            .count();
        assert_eq!(steps, 2, "the step cap must still bind: {events:?}");
    }

    /// The draft is withheld until its citations are checked, and a report citing
    /// a location no tool returned is sent back rather than shipped. This is the
    /// failure the citation check could only *log* before: fluent, cited and
    /// unsupported.
    #[tokio::test]
    async fn a_draft_citing_what_no_tool_returned_is_repaired_before_it_ships() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
                // The revalidation turn: read what the draft cited blindly.
                vec![call("outline", json!({"path": "src/worker/gc.rs"}))],
            ],
            vec![
                "# Draft\n\nSee src/invented.rs:1-9.",
                "# Report\n\nSee src/worker/gc.rs:10-20.",
            ],
        ));
        let events = run_native_shared(ollama.clone(), params(8)).await;

        let summaries: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ResearchEvent::Summary { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            summaries.iter().all(|t| !t.contains("src/invented.rs")),
            "the unverified draft must never reach the client: {summaries:?}"
        );
        assert!(
            summaries.concat().contains("src/worker/gc.rs:10-20"),
            "the repaired report must ship: {summaries:?}"
        );

        let (report, revalidation) = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations {
                    report,
                    revalidation,
                } => Some((report.clone(), *revalidation)),
                _ => None,
            })
            .expect("a citations event");
        let rv = revalidation.expect("the draft failed its check, so this must be set");
        assert_eq!(rv.draft_unverified, 1);
        // The counts describe the report the client was shown, not the draft.
        assert_eq!(
            (report.total, report.verified, report.unverified),
            (1, 1, 0)
        );
    }

    /// A substantial draft naming real files but no line ranges. The shape 4 of the
    /// 5 measured ungrounded reports had, and long enough to clear
    /// `MIN_GROUNDED_REPORT_CHARS`.
    const UNGROUNDED_DRAFT: &str = "# How the sweep avoids orphaning a vector\n\n\
        The garbage collector in src/worker/gc.rs hard-deletes a chunk row only once \
        the vector store has confirmed that the corresponding vector is gone, which is \
        the whole of the guarantee. The sweep collects candidates first, issues one \
        delete per collection through the vector-store seam, and then removes from \
        SQLite only the rows whose collection reported success. A collection that \
        failed keeps its rows marked deleted, so the next sweep tries again rather \
        than losing the reference. Deleting the SQLite row first would invert this: \
        the vector would survive with nothing left in the metadata store pointing at \
        it, and no later sweep could ever find it again. The same pass also prunes the \
        status log and drops file rows whose chunks are all gone, which is what makes \
        a soft delete eventually physical. If every collection in a batch fails the \
        loop breaks instead of spinning, so a wedged vector store costs a skipped \
        sweep rather than a busy one. See also src/db/qdrant.rs for the delete path \
        itself.\n";

    /// A report that grounds nothing is sent back, and the client never sees the
    /// ungrounded draft. This is the defect the check was blind to: `{total: 0}` is
    /// what a clean report emits too, so an ungrounded one shipped looking perfect.
    #[tokio::test]
    async fn a_report_that_grounds_nothing_is_sent_back() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
                // The revalidation turn: recover the range it failed to cite.
                vec![call("outline", json!({"path": "src/worker/gc.rs"}))],
            ],
            vec![UNGROUNDED_DRAFT, "# Report\n\nSee src/worker/gc.rs:10-20."],
        ));
        let events = run_native_shared(ollama.clone(), params(8)).await;

        let summaries: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ResearchEvent::Summary { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            summaries
                .iter()
                .all(|t| !t.contains("How the sweep avoids")),
            "the ungrounded draft must never reach the client: {summaries:?}"
        );
        assert!(
            summaries.concat().contains("src/worker/gc.rs:10-20"),
            "the grounded rewrite must ship: {summaries:?}"
        );

        let (report, revalidation) = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations {
                    report,
                    revalidation,
                } => Some((report.clone(), *revalidation)),
                _ => None,
            })
            .expect("a citations event");
        let rv = revalidation.expect("the draft grounded nothing, so this must be set");
        // No field of its own, deliberately: a draft with no parseable citations has
        // no failing ones either, so "present with all three at zero" *is* the
        // signal, and the wire needs no fourth count for it.
        assert_eq!(
            (rv.draft_unverified, rv.draft_path_only, rv.draft_stale),
            (0, 0, 0)
        );
        assert_eq!((report.total, report.verified), (1, 1));

        // The complaint must say what a citation looks like: 4 of the 5 measured
        // cases named a real path and simply omitted the range.
        let asked = ollama.report_prompts.lock().unwrap().join("\n");
        assert!(
            asked.contains("path/to/file.rs:START-END"),
            "the complaint must name the required form: {asked}"
        );
        assert!(
            asked.contains("src/worker/gc.rs"),
            "the complaint must list what the run was shown: {asked}"
        );
    }

    /// A run no tool showed a single file cannot cite anything, and its "the answer
    /// is not reachable from here" report is the correct outcome — the measured
    /// behaviour of a scoped run that cannot reach its answer. Sending it back would
    /// be demanding a fabrication, so the gate exempts it.
    #[tokio::test]
    async fn a_report_from_a_run_that_was_shown_nothing_ships_uncited() {
        // No tool turns at all: the model answers with prose immediately, so nothing
        // ever entered `Evidence`.
        let ollama = Arc::new(NativeOllama::new(vec![], vec![UNGROUNDED_DRAFT]));
        let events = run_native_shared(ollama.clone(), params(8)).await;

        let revalidation = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations { revalidation, .. } => Some(*revalidation),
                _ => None,
            })
            .expect("a citations event");
        assert!(
            revalidation.is_none(),
            "a run shown nothing must not be asked to cite: {revalidation:?}"
        );
        let summaries = events
            .iter()
            .filter(|e| matches!(e, ResearchEvent::Summary { .. }))
            .count();
        assert_eq!(summaries, 1, "the report ships once: {events:?}");
    }

    /// The short honest refusal keeps shipping as it stands. Only a *substantial*
    /// uncited report is a provenance failure; below the floor, "I could not settle
    /// this" is an answer, not an ungrounded claim.
    #[tokio::test]
    async fn a_short_uncited_report_is_not_sent_back() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report\n\nThe evidence I gathered does not settle this."],
        ));
        let events = run_native_shared(ollama.clone(), params(8)).await;

        let (report, revalidation) = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations {
                    report,
                    revalidation,
                } => Some((report.clone(), *revalidation)),
                _ => None,
            })
            .expect("a citations event");
        assert_eq!(report.total, 0);
        assert!(
            revalidation.is_none(),
            "a short uncited report is taken at its word: {revalidation:?}"
        );
    }

    /// The zero-citation complaint is a different message, not the three-bucket one
    /// with every bucket empty — which is what routing this defect into the existing
    /// gate would otherwise have produced.
    #[test]
    fn the_ungrounded_complaint_names_the_form_and_not_the_empty_buckets() {
        let mut evidence = Evidence::default();
        evidence.record("src/worker/gc.rs", Some(Span { start: 10, end: 20 }));
        let complaint =
            format_citation_complaint("# Report\n\nSee src/worker/gc.rs.", &evidence, true);

        assert!(complaint.contains("path/to/file.rs:START-END"));
        assert!(complaint.contains("src/worker/gc.rs"));
        assert!(
            !complaint.contains("These did not pass"),
            "there is no failing citation to list: {complaint}"
        );
    }

    /// A run stopped by a budget may not buy more tool calls, but it is still told
    /// *which* citations failed. Naming them is the whole instruction: a model that
    /// only knows "some did not check out" can do nothing but guess, and guessing
    /// rewrites the citations that were right.
    #[tokio::test]
    async fn a_budget_stopped_draft_is_told_which_citations_failed() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("search", json!({"query": "qdrant delete_batch"}))],
            ],
            vec![
                "# Draft\n\nSee src/invented.rs:1-9.",
                "# Report\n\nSee src/worker/gc.rs:10-20.",
            ],
        ));
        let events = run_native_shared(ollama.clone(), params(2)).await;
        assert_eq!(done_reason(&events), Some(DoneReason::BudgetExhausted));

        let rewrite = ollama.report_prompts.lock().unwrap()[1].clone();
        assert!(
            rewrite.contains("src/invented.rs:1-9"),
            "the rewrite must name the citation that failed: {rewrite}"
        );
        assert!(
            !rewrite.contains("Use the tools to settle them"),
            "an exhausted run must not be sent to look them up: {rewrite}"
        );
        // No tool call was bought by the repair: the two steps are the two the
        // budget granted.
        let steps = events
            .iter()
            .filter(|e| matches!(e, ResearchEvent::Step { .. }))
            .count();
        assert_eq!(steps, 2, "{events:?}");
    }

    /// A clean draft costs nothing extra: no revalidation, no second generation,
    /// and the report still arrives.
    #[tokio::test]
    async fn a_report_whose_citations_check_out_ships_unchanged() {
        let events = drive_native_with(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report\n\nSee src/worker/gc.rs:10-20."],
            8,
        )
        .await;
        let revalidation = events.iter().find_map(|e| match e {
            ResearchEvent::Citations { revalidation, .. } => Some(*revalidation),
            _ => None,
        });
        assert_eq!(revalidation, Some(None), "{events:?}");
        assert_eq!(
            names(&events).last(),
            Some(&"done"),
            "the run must still finish: {events:?}"
        );
    }

    #[tokio::test]
    async fn several_calls_in_one_turn_all_execute_as_separate_steps() {
        let events = drive_native(
            vec![vec![
                call("list_files", json!({"glob": "*gc*"})),
                call("outline", json!({"path": "src/worker/gc.rs"})),
                call("search", json!({"query": "sweep_candidates"})),
            ]],
            8,
        )
        .await;
        let steps: Vec<(&str, usize)> = events
            .iter()
            .filter_map(|e| match e {
                ResearchEvent::Step { call, n, .. } => Some((call.action(), *n)),
                _ => None,
            })
            .collect();
        assert_eq!(
            steps,
            vec![("list_files", 1), ("outline", 2), ("search", 3)],
            "a multi-call turn must produce one step per call, numbered in order: {events:?}"
        );
        assert_eq!(done_reason(&events), Some(DoneReason::Finalized));
    }

    #[tokio::test]
    async fn a_native_finalize_ends_the_loop() {
        let events = drive_native(
            vec![
                vec![call("search", json!({"query": "q"}))],
                vec![call("finalize", json!({}))],
            ],
            8,
        )
        .await;
        assert_eq!(done_reason(&events), Some(DoneReason::Finalized));
        assert_eq!(
            events.iter().filter(|e| e.name() == "step").count(),
            1,
            "finalize is not a step: {events:?}"
        );
    }

    /// The wall-clock budget is the *primary* one, so it must be able to end a run
    /// that has plenty of steps and context left. Real time, deliberately: the
    /// budget is measured with `std::time::Instant`, which tokio's paused clock
    /// does not move — so the delays here are kept to the smallest values that
    /// still make the assertion unambiguous.
    #[tokio::test]
    async fn the_time_budget_ends_a_run_that_still_has_steps_left() {
        let mut p = params(64);
        p.budget.max_seconds = 1;
        let turns: Vec<Vec<ToolCall>> = (0..8)
            .map(|i| vec![call("search", json!({ "query": format!("q{i}") }))])
            .collect();
        let mut ollama = NativeOllama::new(turns, vec!["# Report"]);
        ollama.turn_delay = Duration::from_millis(400);

        let events = run_native(ollama, p).await;
        assert_eq!(done_reason(&events), Some(DoneReason::TimeExhausted));
        let steps = events.iter().filter(|e| e.name() == "step").count();
        assert!(
            steps > 0 && steps < 8,
            "the clock, not the step cap, must have stopped it: {steps} steps"
        );
        // Cut short or not, the run still reports — that is what the budget buys.
        assert!(events.iter().any(|e| e.name() == "summary"));
    }

    /// The bug this generation fixed: the wall-clock was polled only *between*
    /// turns, so a turn that outran it simply ran on. Here every turn takes far
    /// longer than the whole budget, so the poll can never fire — only cancelling
    /// the turn in flight can stop this run, and it must still produce a report.
    #[tokio::test]
    async fn the_deadline_stops_a_run_mid_turn_and_still_produces_a_report() {
        let mut p = params(64);
        p.budget.max_seconds = 1;
        let turns: Vec<Vec<ToolCall>> = (0..4)
            .map(|i| vec![call("search", json!({ "query": format!("q{i}") }))])
            .collect();
        let mut ollama = NativeOllama::new(turns, vec!["# Report"]);
        // Longer than the budget, so the between-turns poll never gets a chance:
        // the first turn is already past the deadline when it would return.
        ollama.turn_delay = Duration::from_millis(2500);

        let events = run_native(ollama, p).await;
        assert_eq!(
            done_reason(&events),
            Some(DoneReason::TimeExhausted),
            "the deadline, not a counter, must have ended it: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.name() == "summary"),
            "a run cut off by its deadline still reports: {events:?}"
        );
    }

    /// A run stopped by its own deadline and one stopped by a client disconnect are
    /// both `Cancelled` inside the loop, and they must not be confused: the first
    /// owes the caller a report, the second has nobody to send one to. The
    /// disconnect case is covered by the stream tests; this pins that a deadline stop
    /// takes the reporting path rather than the silent one.
    #[tokio::test]
    async fn a_deadline_stop_is_told_apart_from_a_client_disconnect() {
        let mut p = params(64);
        p.budget.max_seconds = 1;
        let mut ollama = NativeOllama::new(
            vec![vec![call("search", json!({"query": "q"}))]],
            vec!["# Report"],
        );
        ollama.turn_delay = Duration::from_millis(1500);

        let events = run_native(ollama, p).await;
        // A silent abort emits neither, which is exactly what a disconnect does.
        assert!(events.iter().any(|e| e.name() == "summary"));
        assert!(events.iter().any(|e| e.name() == "done"));
    }

    /// The report window is the other half of the deadline: without one, a run whose
    /// clock has expired would still run the report phase unbudgeted. A window too
    /// short to write anything must therefore still deliver a `summary` — written by
    /// the server — rather than closing the stream without one.
    #[tokio::test]
    async fn an_expired_report_window_ships_a_server_written_truncation_notice() {
        let mut p = params(64);
        p.budget.max_seconds = 1;
        // Under the floor config validation enforces, but this is the loop's test,
        // not the config's: the point is a window that cannot possibly finish.
        p.report_timeout_ms = 120;
        let mut ollama = NativeOllama::new(
            vec![vec![call("search", json!({"query": "q"}))]],
            vec!["# Report"],
        );
        ollama.turn_delay = Duration::from_millis(200);
        // Every report attempt outlasts the window, so no report can ever be written.
        ollama.report_delay = Duration::from_millis(1500);

        let events = run_native(ollama, p).await;
        let summary = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Summary { text } => Some(text.clone()),
                _ => None,
            })
            .expect("a run always answers, even when the model never wrote the answer");
        assert!(
            summary.contains("Research incomplete") && summary.contains("No report was produced"),
            "the server's own notice must say plainly that this is not a report: {summary}"
        );
        // The window is orthogonal to why the investigation stopped: here the model
        // finished on its own terms and still got no report written, which is the
        // case a window sized from the investigation's budget would have missed.
        assert_eq!(done_reason(&events), Some(DoneReason::Finalized));
    }

    /// A note is the only thing the model writes that survives a turn, so it has to
    /// be in front of the model on every later turn *and* when it writes the report.
    #[tokio::test]
    async fn notes_are_pinned_every_turn_and_reach_the_report_turn() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call(
                    "note",
                    json!({"text": "GC hard-deletes only confirmed rows."}),
                )],
                vec![call("search", json!({"query": "sweep"}))],
                vec![],
            ],
            vec!["# Report"],
        ));
        let events = run_native_shared(ollama.clone(), params(8)).await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                ResearchEvent::Step {
                    call: StepCall::Note { .. },
                    ..
                }
            )),
            "a note is an executed step, and the reader sees it: {events:?}"
        );
        let transcripts = ollama.transcripts.lock().unwrap().clone();
        let last = transcripts.last().expect("a report turn happened");
        assert!(
            last.iter()
                .any(|m| m.content.contains("GC hard-deletes only confirmed rows")),
            "the report turn must see the notes: {last:?}"
        );
    }

    /// Every refusal is explicit. A cap the model cannot see is a cap it keeps
    /// hitting, and a note that vanished silently is worse than one never written —
    /// the model would go on relying on it.
    #[tokio::test]
    async fn a_note_over_the_character_cap_is_refused_in_the_tool_reply() {
        let mut state = RunState::default();
        let long = "x".repeat(MAX_NOTE_CHARS + 1);
        let reply = state.keep_note(&long);
        assert!(reply.contains("Not recorded"), "{reply}");
        assert!(state.notes.is_empty(), "the note must not be kept");
        assert!(state.keep_note("  ").contains("empty"));

        for i in 0..MAX_NOTES {
            assert!(
                state
                    .keep_note(&format!("note {i}"))
                    .starts_with("Note kept")
            );
        }
        let overflow = state.keep_note("one more");
        assert!(
            overflow.contains("oldest note was dropped") && overflow.contains("note 0"),
            "the drop must be announced, and name what was lost: {overflow}"
        );
        assert_eq!(state.notes.len(), MAX_NOTES);
    }

    /// A plan the investigation disproved is worse than no plan: the sufficiency
    /// check judges against it, so it must be replaceable.
    #[tokio::test]
    async fn a_revised_plan_replaces_the_pinned_one() {
        let mut state = RunState {
            plan: Some("1. wrong question".into()),
            ..Default::default()
        };
        let done = apply_local(
            &mut state,
            &Action::RevisePlan {
                plan: "1. the real question".into(),
            },
        );
        assert!(done.text.starts_with("Plan replaced"), "{}", done.text);
        assert_eq!(state.plan.as_deref(), Some("1. the real question"));
        let note = format_state_note(
            &state,
            &Evidence::default(),
            &ToolScope::default(),
            &snapshot(params(8).budget, 0, Duration::ZERO, &TokenTally::default()),
        );
        assert!(note.contains("the real question") && !note.contains("wrong question"));
    }

    /// A refusal must read as a refusal. The measured failure this guards is the one
    /// `outline`'s `indexed` flag already guards: an empty answer tells the model the
    /// file has nothing in it, which is a different and wrong fact — and here it
    /// would also send it hunting for spellings of a path it is simply not allowed
    /// to read.
    #[test]
    fn an_out_of_scope_path_is_refused_by_name_not_answered_empty() {
        let scope = ToolScope {
            include: Some(SearchFilter {
                paths: Some(vec![crate::backend::v0::models::GlobPattern(
                    glob::Pattern::new("docs/*").unwrap(),
                )]),
                programming_languages: None,
            }),
            exclude: None,
        };
        let refused = format_outline(
            &OutlineResponse {
                path: "src/research.rs".into(),
                indexed: false,
                in_scope: false,
                programming_language: None,
                symbols: vec![],
                total_definitions: 0,
            },
            &scope,
        );
        assert!(refused.contains("outside this run's scope"), "{refused}");
        assert!(refused.contains("docs/*"), "the wall is named: {refused}");
        assert!(refused.contains("cannot be widened"), "{refused}");
        assert!(
            !refused.contains("not an indexed file"),
            "a refusal must not read as a wrong path guess: {refused}"
        );
    }

    /// Scoping that hides rows without saying so turns "no such symbol" — an answer
    /// `/symbols` calls definitive — into a lie.
    #[test]
    fn a_scoped_lookup_reports_how_many_matches_it_dropped() {
        let scope = ToolScope {
            include: Some(SearchFilter {
                paths: Some(vec![crate::backend::v0::models::GlobPattern(
                    glob::Pattern::new("docs/*").unwrap(),
                )]),
                programming_languages: None,
            }),
            exclude: None,
        };
        let empty = format_symbols_response(
            "collect",
            &SymbolsResponse {
                definitions: vec![],
                references: vec![],
                total_definitions: 0,
                total_references: 0,
                out_of_scope_definitions: 1,
                out_of_scope_references: 3,
            },
            &scope,
        );
        assert!(
            empty.contains("4 occurrence(s) exist outside it"),
            "an empty scoped answer must not read as \"no such symbol\": {empty}"
        );
    }

    /// The scope is repeated next to where the model generates, but only when there
    /// is one: an unscoped run must not be told about a boundary it does not have.
    #[test]
    fn the_state_note_names_the_scope_only_when_there_is_one() {
        let state = RunState::default();
        let progress = snapshot(params(8).budget, 0, Duration::ZERO, &TokenTally::default());
        let unscoped = format_state_note(
            &state,
            &Evidence::default(),
            &ToolScope::default(),
            &progress,
        );
        assert!(!unscoped.contains("Scope of this run"), "{unscoped}");
        let scope = ToolScope {
            include: Some(SearchFilter {
                paths: None,
                programming_languages: Some(vec![
                    crate::backend::v0::models::ProgrammingLanguage::Rust,
                ]),
            }),
            exclude: None,
        };
        let scoped = format_state_note(&state, &Evidence::default(), &scope, &progress);
        assert!(scoped.contains("Scope of this run"), "{scoped}");
        assert!(scoped.contains("rust"), "{scoped}");
    }

    /// The cost axis. It must end a run the clock and the step cap would let
    /// continue — that is the whole reason it exists: a step is not a unit of work,
    /// and a transcript resent every turn is what the GPU actually pays for.
    #[tokio::test]
    async fn the_token_budget_ends_a_run_the_clock_and_the_step_cap_would_not() {
        let mut p = params(64);
        p.budget.max_tokens = 3000;
        let turns: Vec<Vec<ToolCall>> = (0..8)
            .map(|i| vec![call("search", json!({ "query": format!("q{i}") }))])
            .collect();
        let mut ollama = NativeOllama::new(turns, vec!["# Report"]);
        // 1200 + 2 per turn against a 3000-token budget: the third check trips.
        ollama.prompt_tokens = 1200;

        let events = run_native(ollama, p).await;
        assert_eq!(done_reason(&events), Some(DoneReason::TokensExhausted));
        let steps = events.iter().filter(|e| e.name() == "step").count();
        assert!(
            (1..8).contains(&steps),
            "tokens, not the step cap, must have stopped it: {steps} steps"
        );
        assert!(events.iter().any(|e| e.name() == "summary"));
    }

    /// `progress` is the contract that makes a live run steerable, so its cadence
    /// and its monotonicity are pinned, not incidental.
    #[tokio::test]
    async fn progress_is_emitted_before_the_first_turn_and_after_every_step() {
        let events = drive_native(
            vec![
                vec![
                    call("search", json!({"query": "q1"})),
                    call("outline", json!({"path": "src/worker/gc.rs"})),
                ],
                vec![call("finalize", json!({}))],
            ],
            8,
        )
        .await;

        let progress = progress_events(&events);
        let first = progress
            .first()
            .expect("a run announces its budget up front");
        assert_eq!(
            (first.steps, first.tokens, first.turns),
            (0, 0, 0),
            "the first progress event is the budget announcement, nothing spent yet"
        );
        assert_eq!(
            (first.max_steps, first.max_ms),
            (8, 3_600_000),
            "…and it carries the limits, so a client can render its meters before \
             the first turn returns"
        );
        assert_eq!(
            events[0].name(),
            "progress",
            "the announcement precedes every other event: {events:?}"
        );

        // Monotone: a meter that goes backwards is worse than no meter.
        for pair in progress.windows(2) {
            assert!(
                pair[1].steps >= pair[0].steps
                    && pair[1].tokens >= pair[0].tokens
                    && pair[1].turns >= pair[0].turns
                    && pair[1].elapsed_ms >= pair[0].elapsed_ms,
                "progress went backwards: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }

        let last = progress.last().expect("more than one progress event");
        assert_eq!(last.steps, 2, "both calls of the turn were executed");
        assert!(last.tokens > 0, "token counts reached the client: {last:?}");

        // `done` repeats the same shape, with the report turn's tokens folded in.
        let done = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Done { progress, .. } => Some(*progress),
                _ => None,
            })
            .expect("a done event");
        assert_eq!(done.steps, last.steps);
        assert!(done.tokens >= last.tokens);
    }

    /// The context guard exists for small-window models, where a long transcript
    /// would otherwise be trimmed by Ollama in silence. Here the fake reports a
    /// prompt already past the level's share of its 8192-token window.
    #[tokio::test]
    async fn the_context_budget_ends_a_run_before_ollama_would_trim_it() {
        let mut p = params(64);
        p.budget.context_fraction = 0.5;
        let turns: Vec<Vec<ToolCall>> = (0..8)
            .map(|i| vec![call("search", json!({ "query": format!("q{i}") }))])
            .collect();
        let mut ollama = NativeOllama::new(turns, vec!["# Report"]);
        // Over half of 8192, so the check fires after the first turn has measured
        // it — the loop stops one turn short of the window, never after a trim.
        ollama.prompt_tokens = 5000;

        let events = run_native(ollama, p).await;
        assert_eq!(done_reason(&events), Some(DoneReason::ContextExhausted));
        assert_eq!(
            events.iter().filter(|e| e.name() == "step").count(),
            1,
            "the budget must bite on the turn after the one that measured it: {events:?}"
        );
        assert!(events.iter().any(|e| e.name() == "summary"));
    }

    #[tokio::test]
    async fn a_multi_call_turn_stops_at_the_budget_and_still_answers_every_call() {
        // max_steps = 2, three calls: the third must be answered ("not executed")
        // rather than left unpaired — NativeOllama asserts the pairing itself.
        let events = drive_native(
            vec![vec![
                call("search", json!({"query": "a"})),
                call("search", json!({"query": "b"})),
                call("search", json!({"query": "c"})),
            ]],
            2,
        )
        .await;
        assert_eq!(events.iter().filter(|e| e.name() == "step").count(), 2);
        assert_eq!(done_reason(&events), Some(DoneReason::BudgetExhausted));
    }

    // ── outline / list_files ─────────────────────────────────────────────────

    #[test]
    fn the_orientation_tools_map_from_native_calls() {
        assert_eq!(
            action_from_call(&call("outline", json!({"path": "src/worker/gc.rs"}))),
            Some(Action::Outline {
                path: "src/worker/gc.rs".into()
            })
        );
        assert_eq!(
            action_from_call(&call("list_files", json!({"glob": "*research*"}))),
            Some(Action::ListFiles {
                glob: "*research*".into()
            })
        );
    }

    /// The whole point of these two tools: the model arrives knowing no
    /// identifiers, learns them from metadata, and only then searches.
    #[tokio::test]
    async fn orientation_then_search_is_one_step_each() {
        let events = drive_native(
            vec![
                vec![call("list_files", json!({"glob": "*gc*"}))],
                vec![call("outline", json!({"path": "src/worker/gc.rs"}))],
                vec![call("search", json!({"query": "sweep_candidates"}))],
                vec![call("finalize", json!({}))],
            ],
            8,
        )
        .await;
        let steps: Vec<(&str, usize)> = events
            .iter()
            .filter_map(|e| match e {
                ResearchEvent::Step { call, hits, .. } => Some((call.action(), *hits)),
                _ => None,
            })
            .collect();
        assert_eq!(
            steps,
            vec![("list_files", 1), ("outline", 2), ("search", 1)],
            "each orientation call is one step with its own hit count: {events:?}"
        );
        assert_eq!(done_reason(&events), Some(DoneReason::Finalized));
    }

    #[test]
    fn outline_of_an_unindexed_path_says_so_instead_of_looking_empty() {
        // A wrong guess and a symbol-less file must read differently, or the model
        // concludes the file has nothing in it.
        let missing = format_outline(
            &OutlineResponse {
                path: "src/nope.rs".into(),
                indexed: false,
                in_scope: true,
                programming_language: None,
                symbols: vec![],
                total_definitions: 0,
            },
            &ToolScope::default(),
        );
        assert!(missing.contains("not an indexed file"), "{missing}");
        assert!(
            missing.contains("list_files"),
            "must point a way out: {missing}"
        );

        let empty = format_outline(
            &OutlineResponse {
                path: "src/nope.rs".into(),
                indexed: true,
                in_scope: true,
                programming_language: Some(crate::backend::v0::models::ProgrammingLanguage::Rust),
                symbols: vec![],
                total_definitions: 0,
            },
            &ToolScope::default(),
        );
        assert!(empty.contains("declares no symbols"), "{empty}");
    }

    #[test]
    fn formatted_outline_carries_names_lines_and_truncation() {
        let resp = OutlineResponse {
            path: "src/worker/gc.rs".into(),
            indexed: true,
            in_scope: true,
            programming_language: Some(crate::backend::v0::models::ProgrammingLanguage::Rust),
            symbols: vec![crate::backend::v0::models::OutlineSymbol {
                name: "collect".into(),
                kind: "function".into(),
                start_line: 10,
                end_line: 40,
                parent_name: Some("Gc".into()),
                parent_kind: None,
                doc: Some("Sweeps deleted chunks.\nSecond line dropped.".into()),
            }],
            total_definitions: 7,
        };
        let out = format_outline(&resp, &ToolScope::default());
        assert!(out.contains("function collect :10-40"), "{out}");
        assert!(
            out.contains("[rust]"),
            "the language must be named: tags `kind` labels are not uniform \
             across languages, so the model needs it to read them: {out}"
        );
        assert!(out.contains("(in Gc)"), "{out}");
        assert!(out.contains("Sweeps deleted chunks."), "{out}");
        assert!(
            !out.contains("Second line"),
            "only the doc's first line: {out}"
        );
        assert!(
            out.contains("showing the first 1"),
            "truncation visible: {out}"
        );
    }

    #[test]
    fn empty_list_files_explains_the_glob_semantics() {
        let out = format_list_files(
            "src/*.rs",
            &ListFilesResponse {
                files: vec![],
                total: 0,
            },
        );
        assert!(out.contains("No indexed file matches"), "{out}");
        assert!(out.contains("across directories"), "{out}");
    }

    #[test]
    fn orientation_calls_are_deduplicated_like_the_others() {
        // Same dedup namespace, so an outline cannot be replayed for free.
        assert_ne!(
            format!("outline\u{0}{}", "a.rs"),
            format!("list_files\u{0}{}", "a.rs")
        );
    }

    // ── done reasons ─────────────────────────────────────────────────────────

    #[test]
    fn done_reason_wire_values_are_stable() {
        // These strings are part of the SSE contract (scout and the VS Code view
        // key on them) — changing one is a client-visible break.
        assert_eq!(DoneReason::Finalized.as_str(), "finalized");
        assert_eq!(DoneReason::BudgetExhausted.as_str(), "budget_exhausted");
        assert_eq!(DoneReason::Unparseable.as_str(), "unparseable");
        assert_eq!(DoneReason::RepeatedCalls.as_str(), "repeated_calls");
        assert_eq!(DoneReason::TimeExhausted.as_str(), "time_exhausted");
        assert_eq!(DoneReason::ContextExhausted.as_str(), "context_exhausted");
        assert!(!DoneReason::Finalized.is_truncated());
        for r in [
            DoneReason::BudgetExhausted,
            DoneReason::Unparseable,
            DoneReason::RepeatedCalls,
            DoneReason::TimeExhausted,
            DoneReason::ContextExhausted,
        ] {
            assert!(r.is_truncated(), "{r:?} cut the investigation short");
        }
    }

    #[test]
    fn each_action_names_its_argument_on_the_wire() {
        // scout's whitelist and the VS Code renderer key on these exact names.
        for (call, key) in [
            (StepCall::Search { query: "x".into() }, "query"),
            (StepCall::Symbols { name: "x".into() }, "name"),
            (StepCall::Outline { path: "x".into() }, "path"),
            (StepCall::Callers { name: "x".into() }, "name"),
            (StepCall::ListFiles { glob: "x".into() }, "glob"),
            (StepCall::ReadChunks { path: "x".into() }, "path"),
        ] {
            let action = call.action();
            let d = ResearchEvent::Step {
                n: 1,
                call,
                hits: 2,
            }
            .data();
            assert_eq!(d["action"], action);
            assert_eq!(
                d[key], "x",
                "{action} must carry its argument as {key}: {d}"
            );
        }
    }

    /// An empty `callers` result has two meanings and the model acts differently
    /// on each: "defined, nobody calls it" invites reading the definition, "no such
    /// name" means the identifier was guessed wrong. One shared empty list would
    /// tell the model its name was right — the failure `outline`'s `indexed` flag
    /// exists to prevent.
    #[test]
    fn an_empty_call_graph_says_which_kind_of_empty_it_is() {
        let empty = |defined| CallersResponse {
            name: "collect".into(),
            direction: CallDirection::In,
            defined,
            sites: vec![],
            total_sites: 0,
            total_references: 0,
            out_of_scope_sites: 0,
        };

        let unreferenced = format_callers(&empty(true), &ToolScope::default());
        assert!(
            unreferenced.contains("is defined in this project"),
            "a defined but unreferenced name must say so: {unreferenced}"
        );

        let unknown = format_callers(&empty(false), &ToolScope::default());
        assert!(
            unknown.contains("probably wrong"),
            "an unknown name must be called out as a wrong guess: {unknown}"
        );
    }

    /// The lexical caveat rides on the *result*, not only on the tool description:
    /// by the time this is read the description is thousands of tokens back, and a
    /// list of file:line pairs reads as resolved unless it says otherwise.
    #[test]
    fn a_call_graph_result_repeats_that_its_edges_are_lexical() {
        let text = format_callers(
            &CallersResponse {
                name: "collect".into(),
                direction: CallDirection::In,
                defined: true,
                sites: vec![crate::backend::v0::models::CallSite {
                    path: "src/worker/gc.rs".into(),
                    symbol: Some("sweep".into()),
                    kind: Some("function".into()),
                    first_line: 12,
                    occurrences: 2,
                }],
                total_sites: 1,
                total_references: 2,
                out_of_scope_sites: 0,
            },
            &ToolScope::default(),
        );
        assert!(text.contains("LEXICAL"), "{text}");
        assert!(text.contains("src/worker/gc.rs :12"), "{text}");
        assert!(text.contains("sweep"), "{text}");
    }

    // ── near-duplicate rejection ────────────────────────────────────────────

    /// The measured failure: one run spent its whole budget asking for
    /// `research_inner` six different ways. Every key was distinct, so exact-match
    /// rejection never fired and every rephrasing cost a GPU embed to return
    /// almost the same five chunks.
    #[test]
    fn rephrasings_of_one_query_count_as_the_same_call() {
        let first = "fn research_inner loop implementation";
        for rephrasing in [
            "fn research_inner loop impl",
            "fn research_inner loop body",
            "implementation of the fn research_inner loop",
        ] {
            assert!(
                is_near_duplicate(first, rephrasing),
                "{rephrasing:?} should be rejected as a rephrasing of {first:?}"
            );
        }
    }

    /// The other half: a genuine narrowing must still get through, or the cap
    /// punishes the model for doing exactly what the prompt asks of it.
    #[test]
    fn a_genuinely_different_query_is_not_a_duplicate() {
        for (a, b) in [
            (
                "gc sweep deleted chunks",
                "qdrant delete_batch failure handling",
            ),
            (
                "how are chunks embedded",
                "how is the retry worker scheduled",
            ),
            // Short queries: a single shared token is not a rephrasing.
            ("gc sweep", "gc collect"),
        ] {
            assert!(!is_near_duplicate(a, b), "{a:?} vs {b:?}");
        }
    }

    /// Case and whitespace are not new information, so they must not buy a second
    /// execution of the same search.
    #[test]
    fn case_and_spacing_do_not_make_a_new_call() {
        assert_eq!(normalize_query("  GC   Sweep "), "gc sweep");
        assert_eq!(normalize_query("gc sweep"), normalize_query("GC  SWEEP"));
    }

    // ── citation provenance ─────────────────────────────────────────────────

    fn evidence_of(entries: &[(&str, Option<(usize, usize)>)]) -> Evidence {
        let mut e = Evidence::default();
        for (p, span) in entries {
            e.record(p, span.map(|(start, end)| Span { start, end }));
        }
        e
    }

    #[test]
    fn citations_are_parsed_out_of_ordinary_report_prose() {
        let report = "The cap lives in `src/research.rs:518-539`, and the sweep in \
                      src/worker/gc.rs:222-235. See also tools/vscode/src/api.ts:171-234.";
        let cites = parse_citations(report);
        assert_eq!(
            cites
                .iter()
                .map(|c| (c.path.as_str(), c.start, c.end))
                .collect::<Vec<_>>(),
            vec![
                ("src/research.rs", 518, 539),
                ("src/worker/gc.rs", 222, 235),
                ("tools/vscode/src/api.ts", 171, 234),
            ]
        );
    }

    /// The parser must not turn prose into evidence: without the extension rule,
    /// "step 3:10-20" and a bare range would both score as citations and the
    /// verified ratio would be diluted by things nobody claimed.
    #[test]
    fn prose_that_merely_looks_numeric_is_not_a_citation() {
        for text in [
            "at step 3:10-20 the model gave up",
            "the range 12-30 was never shown",
            "see section 4:1-2 of the README",
            "http://example.com:8080-8090",
        ] {
            assert!(
                parse_citations(text).is_empty(),
                "false positive in {text:?}"
            );
        }
        // A single-line citation is legal and common.
        assert_eq!(parse_citations("src/x.rs:12-12").len(), 1);
    }

    #[test]
    fn a_cited_range_the_tools_showed_is_verified() {
        let ev = evidence_of(&[("src/research.rs", Some((500, 560)))]);
        let r = check_citations("see src/research.rs:518-539", &ev);
        assert_eq!(
            (r.total, r.verified, r.path_only, r.unverified),
            (1, 1, 0, 0)
        );
    }

    /// Overlap, not containment: a model that read two adjacent chunks may cite
    /// across their boundary, and a chunk boundary is not a fact about the code.
    #[test]
    fn a_range_overlapping_what_was_shown_is_verified() {
        let ev = evidence_of(&[("src/gc.rs", Some((100, 120)))]);
        let r = check_citations("src/gc.rs:110-140", &ev);
        assert_eq!(r.verified, 1, "{r:?}");
    }

    /// Knowing a file exists (a `list_files` hit) is not evidence about its line
    /// 40 — the two must land in different buckets or `list_files` would launder
    /// every invented line range in the tree.
    #[test]
    fn a_path_shown_without_a_range_does_not_verify_a_range() {
        let ev = evidence_of(&[("src/gc.rs", None)]);
        let r = check_citations("src/gc.rs:40-60", &ev);
        assert_eq!((r.verified, r.path_only, r.unverified), (0, 1, 0), "{r:?}");
        assert!(r.unverified_paths.is_empty());
    }

    #[test]
    fn a_path_no_tool_returned_is_unverified_and_named() {
        let ev = evidence_of(&[("src/research.rs", Some((1, 10)))]);
        let r = check_citations(
            "as shown in src/invented.rs:1-5 and src/invented.rs:9-9",
            &ev,
        );
        assert_eq!((r.total, r.unverified), (2, 2));
        // Named once, not once per citation: this is a signal, not a log.
        assert_eq!(r.unverified_paths, vec!["src/invented.rs".to_string()]);
        assert_eq!(r.cited_paths, vec!["src/invented.rs".to_string()]);
    }

    #[test]
    fn citations_wire_fields_are_stable() {
        // scout's reader and the VS Code view key on these exact names; a rename
        // here is a silent drop there, not an error.
        let ev = ResearchEvent::Citations {
            report: CitationReport {
                total: 9,
                verified: 7,
                path_only: 1,
                unverified: 1,
                stale: 2,
                unverified_paths: vec!["src/nope.rs".into()],
                stale_paths: vec!["src/moved.rs".into()],
                cited_paths: vec!["src/nope.rs".into()],
            },
            revalidation: Some(Revalidation {
                draft_unverified: 4,
                draft_path_only: 2,
                draft_stale: 1,
                steps: 3,
            }),
        };
        assert_eq!(ev.name(), "citations");
        let d = ev.data();
        assert_eq!(d["total"], 9);
        assert_eq!(d["verified"], 7);
        assert_eq!(d["path_only"], 1);
        assert_eq!(d["unverified"], 1);
        assert_eq!(d["unverified_paths"], json!(["src/nope.rs"]));
        // Freshness beside provenance: a report can be perfectly cited and still
        // describe code the index has replaced.
        assert_eq!(d["stale"], 2);
        assert_eq!(d["stale_paths"], json!(["src/moved.rs"]));
        // The draft's counts ride flat beside the final report's, so a consumer
        // learns "this was repaired" from three names rather than a nested shape.
        assert_eq!(d["draft_unverified"], 4);
        assert_eq!(d["draft_path_only"], 2);
        assert_eq!(d["draft_stale"], 1);
        assert_eq!(d["revalidation_steps"], 3);
        // Deliberately absent from the wire: it is the per-run record's field, and
        // a consumer that wants paths wants the unverified ones.
        assert!(d.get("cited_paths").is_none(), "{d}");
    }

    /// A report whose citations all checked out is not the same event as one that
    /// was repaired, and a consumer must be able to tell without guessing.
    #[test]
    fn a_report_that_needed_no_repair_says_so_with_nulls() {
        let ev = ResearchEvent::Citations {
            report: CitationReport::default(),
            revalidation: None,
        };
        let d = ev.data();
        assert_eq!(d["draft_unverified"], Value::Null);
        assert_eq!(d["draft_path_only"], Value::Null);
        assert_eq!(d["draft_stale"], Value::Null);
        assert_eq!(d["revalidation_steps"], Value::Null);
    }

    /// A `RunProgress` with every field distinct, so a wire test cannot pass by
    /// accident when two keys are swapped.
    fn progress_fixture() -> RunProgress {
        RunProgress {
            steps: 3,
            max_steps: 20,
            elapsed_ms: 10,
            max_ms: 240_000,
            tokens: 1300,
            max_tokens: 400_000,
            prompt_tokens: 1200,
            eval_tokens: 100,
            peak_prompt_tokens: 800,
            num_ctx: 8192,
            turns: 4,
        }
    }

    #[test]
    fn done_event_carries_the_reason_and_the_run_cost_on_the_wire() {
        let ev = ResearchEvent::Done {
            progress: progress_fixture(),
            context_fraction: 0.7,
            reason: DoneReason::BudgetExhausted,
        };
        let d = ev.data();
        assert_eq!(d["reason"], "budget_exhausted");
        // The original contract: these two stay top-level.
        assert_eq!(d["steps"], 3);
        assert_eq!(d["elapsed_ms"], 10);
        // The cost, so a consumer reading only `done` still gets the whole record.
        assert_eq!(d["tokens"], 1300);
        assert_eq!(d["prompt_tokens"], 1200);
        assert_eq!(d["eval_tokens"], 100);
        assert_eq!(d["peak_prompt_tokens"], 800);
        assert_eq!(d["num_ctx"], 8192);
        assert_eq!(d["turns"], 4);
        // Which instructions produced the report. A measurement corpus that cannot
        // tell two prompt generations apart reads a prompt regression as model
        // variance, so this is part of the record, not decoration.
        assert_eq!(d["prompt_version"], PROMPT_VERSION);
    }

    #[test]
    fn progress_wire_fields_are_stable() {
        // The VS Code meter and scout's whitelist key on these exact names; a
        // rename here is a silent drop there, not an error.
        let ev = ResearchEvent::Progress {
            progress: progress_fixture(),
            context_fraction: 0.7,
        };
        assert_eq!(ev.name(), "progress");
        let d = ev.data();
        for (key, want) in [
            ("steps", json!(3)),
            ("max_steps", json!(20)),
            ("elapsed_ms", json!(10)),
            ("max_ms", json!(240_000)),
            ("tokens", json!(1300)),
            ("max_tokens", json!(400_000)),
            ("prompt_tokens", json!(1200)),
            ("eval_tokens", json!(100)),
            ("peak_prompt_tokens", json!(800)),
            ("num_ctx", json!(8192)),
            ("turns", json!(4)),
            // 800/8192 = 9.8%, rounded to one decimal.
            ("context_pct", json!(9.8)),
            // 3/20 steps (15%) beats 800/(8192*0.7) context (14%), time and tokens.
            ("binding", json!("steps")),
        ] {
            assert_eq!(d[key], want, "progress.{key} changed shape: {d}");
        }
    }

    #[test]
    fn binding_names_the_axis_closest_to_exhaustion() {
        let base = RunProgress {
            steps: 0,
            max_steps: 100,
            elapsed_ms: 0,
            max_ms: 100_000,
            tokens: 0,
            max_tokens: 100_000,
            prompt_tokens: 0,
            eval_tokens: 0,
            peak_prompt_tokens: 0,
            num_ctx: 100_000,
            turns: 1,
        };
        assert_eq!(
            RunProgress {
                elapsed_ms: 90_000,
                ..base
            }
            .binding(1.0),
            Binding::Time
        );
        assert_eq!(
            RunProgress {
                tokens: 90_000,
                ..base
            }
            .binding(1.0),
            Binding::Tokens
        );
        assert_eq!(
            RunProgress { steps: 90, ..base }.binding(1.0),
            Binding::Steps
        );
        assert_eq!(
            RunProgress {
                peak_prompt_tokens: 90_000,
                ..base
            }
            .binding(1.0),
            Binding::Context
        );
        // Nothing spent yet: the primary budget is the honest answer, not a tie
        // broken at random.
        assert_eq!(base.binding(1.0), Binding::Time);
        // A model that has not reported a window yet must not read as "context is
        // about to bind" — the ratio is 0, not a division by zero.
        assert_eq!(
            RunProgress {
                num_ctx: 0,
                peak_prompt_tokens: 0,
                elapsed_ms: 1,
                ..base
            }
            .binding(0.7),
            Binding::Time
        );
    }

    // ── token tally ──────────────────────────────────────────────────────────

    #[test]
    fn tally_sums_reported_turns_and_counts_silent_ones() {
        let mut tally = TokenTally::default();
        tally.record(ChatOutcome {
            content: "a".into(),
            tool_calls: Vec::new(),
            prompt_tokens: Some(1200),
            eval_tokens: Some(40),
            num_ctx: 8192,
        });
        tally.record(ChatOutcome {
            content: "b".into(),
            tool_calls: Vec::new(),
            prompt_tokens: Some(1800),
            eval_tokens: None,
            num_ctx: 8192,
        });
        // A turn Ollama reported nothing for must not read as a free turn.
        tally.record(ChatOutcome {
            content: "c".into(),
            tool_calls: Vec::new(),
            prompt_tokens: None,
            eval_tokens: None,
            num_ctx: 8192,
        });
        assert_eq!(
            tally,
            TokenTally {
                turns: 3,
                turns_unreported: 1,
                prompt_tokens: 3000,
                eval_tokens: 40,
                // The peak, not the sum: 1800 was the largest single prompt.
                peak_prompt_tokens: 1800,
                num_ctx: 8192,
            }
        );
    }

    #[test]
    fn tally_record_passes_the_outcome_through() {
        let mut tally = TokenTally::default();
        let out = tally.record(ChatOutcome {
            content: "report".into(),
            tool_calls: Vec::new(),
            prompt_tokens: Some(5),
            eval_tokens: Some(6),
            num_ctx: 8192,
        });
        assert_eq!(out.content, "report");
    }

    /// Prose with no tool call is the model answering, which in a tool-calling
    /// loop means "done" — not a protocol violation. (Before native tools this
    /// was `Unparseable`, because a reply that was not a JSON action could only
    /// be a mistake.)
    #[tokio::test]
    async fn prose_with_no_tool_call_ends_the_loop_as_finalized() {
        let events = drive(
            vec![
                ("", "I already know this from the code I have seen."),
                ("", "# Report\n\nBest effort."),
            ],
            8,
        )
        .await;
        assert_eq!(
            names(&events),
            vec!["summary", "citations", "done"],
            "{events:?}"
        );
        assert_eq!(done_reason(&events), Some(DoneReason::Finalized));
    }

    /// Empty replies are the real protocol failure: nothing to act on, nothing to
    /// report. Bounded by MAX_PARSE_RETRIES.
    #[tokio::test]
    async fn empty_replies_force_finalize_once_the_retries_run_out() {
        let events = drive(
            vec![
                ("", ""),
                ("", ""),
                ("", ""),
                ("", "# Forced report\n\nBest effort."),
            ],
            8,
        )
        .await;
        assert_eq!(done_reason(&events), Some(DoneReason::Unparseable));
    }

    /// An empty report turn is asked again rather than failing the run. Measured on
    /// `gpt-oss:20b`: the model generates the whole report into its analysis channel
    /// and leaves `final` empty, so the run dies holding a finished answer. An empty
    /// content streamed nothing, so the client sees exactly one report either way.
    #[tokio::test]
    async fn an_empty_report_turn_is_asked_again_at_the_next_seed() {
        let events = drive(
            vec![
                ("", "The evidence is sufficient."),
                ("", ""),
                ("", "# Report\n\nOn the second ask."),
            ],
            8,
        )
        .await;
        let summaries: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ResearchEvent::Summary { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            summaries,
            vec!["# Report\n\nOn the second ask."],
            "{events:?}"
        );
        assert_eq!(done_reason(&events), Some(DoneReason::Finalized));
    }

    /// A tool call in place of the report is recoverable: it is never streamed
    /// (the content gate withholds anything opening with `{`), so one re-ask
    /// costs the client nothing and saves the run.
    #[tokio::test]
    async fn a_tool_call_instead_of_a_report_is_retried_not_streamed() {
        let events = drive_native_with(
            vec![vec![call("finalize", json!({}))]],
            vec![
                r#"{"action":"search","query":"loop research_inner"}"#,
                "# Report\n\nOn the second ask.",
            ],
            8,
        )
        .await;
        let summaries: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ResearchEvent::Summary { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            summaries.iter().all(|t| !t.contains("\"action\"")),
            "the tool call must never reach the client: {summaries:?}"
        );
        assert!(
            summaries.concat().contains("On the second ask"),
            "the retried report must be streamed: {summaries:?}"
        );
        assert_eq!(events.last().map(|e| e.name()), Some("done"));
    }

    /// Regression: twice is a failure. A JSON action is not a briefing, and scout
    /// hands the report straight to a frontier model.
    #[tokio::test]
    async fn two_tool_calls_instead_of_a_report_is_an_error() {
        let events = drive_native_with(
            vec![vec![call("finalize", json!({}))]],
            vec![
                r#"{"action":"search","query":"one"}"#,
                r#"{"action":"search","query":"two"}"#,
            ],
            8,
        )
        .await;
        match events.last() {
            Some(ResearchEvent::Error { code, .. }) => assert_eq!(code, "research.no_report"),
            other => panic!("expected a research.no_report error, got {other:?}"),
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ResearchEvent::Summary { .. })),
            "nothing may be streamed as a report: {events:?}"
        );
    }

    #[tokio::test]
    async fn step_budget_forces_finalize() {
        let events = drive_native_with(
            vec![
                vec![call("search", json!({"query": "q1"}))],
                vec![call("search", json!({"query": "q2"}))],
            ],
            vec!["# Report after budget"],
            2,
        )
        .await;
        assert_eq!(
            names(&events),
            vec![
                "thinking",
                "step",
                "thinking",
                "step",
                "summary",
                "citations",
                "done"
            ]
        );
        assert_eq!(done_reason(&events), Some(DoneReason::BudgetExhausted));
    }

    #[tokio::test]
    async fn closed_channel_cancels_quietly() {
        let ollama = Arc::new(FakeOllama {
            replies: Mutex::new(vec![
                ("think hard", r#"{"action":"search","query":"q"}"#),
                ("", r#"{"action":"finalize"}"#),
                ("", "# Report"),
            ]),
        });
        let tools = Arc::new(FakeTools::default());
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx); // client gone before the first event
        let token = CancellationToken::new();
        run_research(
            ollama,
            tools,
            Arc::new(NoJournal),
            params(8),
            tx,
            token.clone(),
        )
        .await;
        assert!(
            token.is_cancelled(),
            "a closed channel must cancel the job's token"
        );
    }

    #[tokio::test]
    async fn ollama_failure_becomes_an_error_event() {
        // Script exhausted on the very first turn → Decode error. The budget
        // announcement has already gone out by then — a run that dies on turn one
        // still told the client what it was granted.
        let events = drive(vec![], 8).await;
        assert_eq!(names(&events), vec!["error"], "{events:?}");
        let events: Vec<_> = events
            .into_iter()
            .filter(|e| e.name() != "progress")
            .collect();
        match &events[0] {
            ResearchEvent::Error { code, .. } => assert_eq!(code, "ollama.unavailable"),
            other => panic!("expected an error event, got {other:?}"),
        }
    }
}
