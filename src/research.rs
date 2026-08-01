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
use tracing::{debug, info, warn};

use crate::backend::error::ApiError;
use crate::backend::v0::handlers::{GREP_MIN_PATTERN_CHARS, research_title};
use crate::backend::v0::models::{
    CallDirection, CallersResponse, ChangeType, FileHistoryResponse, GrepResponse,
    ListFilesResponse, OutlineResponse, ReadChunksResponse, SearchFilter, SearchRequest,
    SearchResult, SymbolRoleFilter, SymbolsRequest, SymbolsResponse,
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
/// [`format_ungrounded_complaint`], [`format_markdown_complaint`],
/// [`REPORT_ROLE`] or [`report_system_prompt`], either report turn's
/// user message, the
/// budget-exhausted nudges, [`format_prior_reports`], or [`tool_specs`] — anything
/// that changes what the model is asked or what it may call. The run-state note counts too, but only its
/// wording ([`format_state_note`]'s labels): its *contents* are the run's own
/// history and differ every run by design. Not a version of the *code*: refactors
/// that leave the wording identical leave this alone.
///
/// `MAJOR.MINOR`, the notation documented on
/// [`CHUNKS_DERIVATION_VERSION`](crate::slicing::traits::CHUNKS_DERIVATION_VERSION).
/// Nothing ever compares this one — it is pure provenance — so the split between
/// the two numbers is the only thing that gives it meaning: MINOR for reworded
/// instructions, MAJOR for a run that asks the model to do a different job.
///
/// 1.3 → 1.4: the report turn learned a length ceiling, a section shape taken from
/// the plan, an evidence digest, a shed notice, a cut-off notice on the rewrite, and
/// the anti-speculation rule that used to reach only truncated runs; `grep`'s spec
/// gained a sentence about what an empty result means. MINOR: the job — read this
/// evidence, write one cited report — is unchanged; only how it is asked for is.
pub const PROMPT_VERSION: &str = "1.4";

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
    /// Word ceiling announced to the report turn, and the source of its
    /// `num_predict`. `0` = announce nothing, the behaviour before this existed.
    ///
    /// Not a budget axis either — nothing stops on it — but it is the only axis
    /// about **output**, and output is where runs were measured to fail: the loop
    /// found the right files every time and then failed to write them up. Like
    /// `context_fraction`, it never comes from the request.
    pub max_report_words: usize,
}

impl Budget {
    /// An effort preset with the request's overrides applied, axis by axis.
    ///
    /// A partial override keeps the preset for every axis it does not name, so
    /// `{"max_seconds": 60}` shortens the run without silently deepening anything
    /// else. `context_fraction` and `max_report_words` never come from the request
    /// — see the struct docs.
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
            max_report_words: preset.max_report_words,
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
    ///
    /// `server_written` says the report was not the model's at all — the report
    /// window expired and the server assembled one. That distinction was invisible
    /// here for its whole life, and its absence is a real defect rather than a
    /// missing nicety: a server-written report contains no `path:start-end`, so
    /// `check_citations` scores it `total: 0, verified: 0, unverified: 0` — byte-for-byte
    /// what a clean report emits, in the exact field a caller is told to trust.
    /// Field reports of "verified: 0 / unverified: 0 even though it read the files"
    /// are this, and nothing else.
    Citations {
        report: CitationReport,
        revalidation: Option<Revalidation>,
        server_written: bool,
    },
    /// The indexed code at every **verified** citation, verbatim. Emitted once,
    /// after `citations` and before `done`.
    ///
    /// The server already has these bytes — the paths and spans are in `Evidence`
    /// and the code is in SQLite — so shipping them costs one query and no GPU. What
    /// it buys is the thing the loop could not do: a caller who wants a file's
    /// literal text used to have to make the *model* retype it into the report,
    /// which is precisely the output-volume failure this generation is about. Now
    /// the report cites and the server quotes.
    ///
    /// Verified citations only. A `path_only` or `unverified` citation names no
    /// location worth reading, and attaching real bytes to one would dress up a
    /// claim the provenance check has just refused.
    Excerpts {
        excerpts: Vec<ReportExcerpt>,
        /// Verified citations found, before the caps. `truncated` says some of
        /// their code did not fit.
        total: usize,
        truncated: bool,
    },
    /// The run's final state and full cost. Carries every `progress` field, so a
    /// consumer that only reads `done` still gets the whole record.
    Done {
        progress: RunProgress,
        context_fraction: f64,
        reason: DoneReason,
        /// How the finished run can be named afterwards, or `None` if the journal
        /// write failed. A client cannot offer to reuse a run it cannot fetch, and
        /// the journal is best-effort by contract — so this is nullable on the wire
        /// rather than fabricated.
        recorded: Option<RecordedRun>,
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
    Search {
        query: String,
    },
    Symbols {
        name: String,
    },
    Outline {
        path: String,
    },
    Callers {
        name: String,
    },
    ListFiles {
        glob: String,
    },
    ReadChunks {
        path: String,
    },
    Grep {
        pattern: String,
    },
    FileHistory {
        path: String,
    },
    Note {
        text: String,
    },
    RevisePlan {
        plan: String,
    },
    ListResearch {
        query: String,
    },
    /// The seq, formatted — `argument()` returns `&str`, and its consumers only
    /// display the value, so a numeric wire type would buy a per-variant special
    /// case in `data()` for nothing.
    ReadResearch {
        seq: String,
    },
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
            StepCall::FileHistory { .. } => "file_history",
            StepCall::Note { .. } => "note",
            StepCall::RevisePlan { .. } => "revise_plan",
            StepCall::ListResearch { .. } => "list_research",
            StepCall::ReadResearch { .. } => "read_research",
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
            StepCall::FileHistory { path } => ("path", path),
            StepCall::Note { text } => ("text", text),
            StepCall::RevisePlan { plan } => ("plan", plan),
            StepCall::ListResearch { query } => ("query", query),
            StepCall::ReadResearch { seq } => ("seq", seq),
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
            ResearchEvent::Excerpts { .. } => "excerpts",
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
                server_written,
            } => json!({
                "server_written": server_written,
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
            ResearchEvent::Excerpts {
                excerpts,
                total,
                truncated,
            } => json!({
                "total": total,
                "truncated": truncated,
                "excerpts": excerpts
                    .iter()
                    .map(|e| json!({
                        "path": e.path,
                        "start_line": e.start_line,
                        "end_line": e.end_line,
                        "code": e.code,
                    }))
                    .collect::<Vec<Value>>(),
            }),
            ResearchEvent::Done {
                progress,
                context_fraction,
                reason,
                recorded,
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
                    // How to ask for this run again. Null when the journal write
                    // failed — the run happened, but nothing can fetch it, and a
                    // fabricated id would be worse than an honest absence.
                    map.insert(
                        "run_id".into(),
                        recorded
                            .as_ref()
                            .map_or(Value::Null, |r| Value::String(r.id.clone())),
                    );
                    map.insert(
                        "seq".into(),
                        recorded.as_ref().map_or(Value::Null, |r| json!(r.seq)),
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

    /// The commits that touched one path, newest first — the git channel's only
    /// model-facing lookup. Path-keyed, so out of scope is an explicit refusal
    /// (`in_scope: false`) rather than an empty list.
    async fn file_history(
        &self,
        path: String,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<FileHistoryResponse, ApiError>;

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

    /// The stored, still-valid research runs of the same project — never invalid
    /// ones. Takes no [`ToolScope`]: reports are not files, and a run scoped to a
    /// corner of the tree may still orient itself from project-wide research (its
    /// content cannot be cited anyway — see the hearsay rule in `execute`). The
    /// loop, not the impl, filters out the runs already injected as context.
    async fn list_research(
        &self,
        query: Option<String>,
        token: &CancellationToken,
    ) -> Result<Vec<ResearchListing>, ApiError>;

    /// One stored run's report, by per-project seq. Unscoped, like
    /// [`list_research`](ResearchTools::list_research).
    async fn read_research(
        &self,
        seq: i64,
        token: &CancellationToken,
    ) -> Result<StoredReport, ApiError>;

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

    async fn file_history(
        &self,
        path: String,
        scope: &ToolScope,
        token: &CancellationToken,
    ) -> Result<FileHistoryResponse, ApiError> {
        let t = Instant::now();
        let r = self.inner.file_history(path, scope, token).await;
        self.record("file_history", t, &r);
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

    async fn list_research(
        &self,
        query: Option<String>,
        token: &CancellationToken,
    ) -> Result<Vec<ResearchListing>, ApiError> {
        let t = Instant::now();
        let r = self.inner.list_research(query, token).await;
        self.record("list_research", t, &r);
        r
    }

    async fn read_research(
        &self,
        seq: i64,
        token: &CancellationToken,
    ) -> Result<StoredReport, ApiError> {
        let t = Instant::now();
        let r = self.inner.read_research(seq, token).await;
        self.record("read_research", t, &r);
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
    /// Earlier runs' reports, injected into the transcript before the plan turn.
    ///
    /// Loaded by the handler, because the loop has no project identity and no database
    /// access of its own — the same reason `scope` arrives already resolved. Empty for
    /// a cold run, which is the overwhelmingly common case and byte-for-byte the
    /// transcript this loop has always built.
    pub prior_reports: Vec<PriorReport>,
    /// Total characters the block above may occupy
    /// (`[research].max_context_chars`). Ignored when `prior_reports` is empty.
    pub max_context_chars: usize,
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

/// One earlier run's report, as handed to a new run.
///
/// Carries the run's own staleness alongside the prose, because that is the only
/// thing that lets the model weigh two reports that disagree — and stating it is what
/// keeps an obsolete report from reading as current. It is the same currency signal
/// `probe_freshness` gives a live run about its own evidence, asked of a stored one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorReport {
    /// The stable identifier, journalled onto the new run so reuse is measurable.
    pub id: String,
    /// The per-project ordinal, which is what the model and the reader both see.
    pub seq: i64,
    pub question: String,
    pub report: String,
    /// Files the run read whose indexed hash has since changed or gone, against how
    /// many it read at all. `0` of anything means the report still describes what is
    /// there.
    pub files_moved: usize,
    pub files_total: usize,
}

/// One row of `list_research`: enough for the model to decide whether to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResearchListing {
    /// The stable id — used only to exclude the runs already injected as context;
    /// the model addresses reports by `seq`.
    pub id: String,
    pub seq: i64,
    /// The report's own stored heading; `None` falls back to the question.
    pub title: Option<String>,
    pub question: String,
    pub created_at: i64,
}

/// `read_research`'s three honest answers. `Invalid` is its own variant rather
/// than an empty result for the `outline.indexed` reason: a refusal that reads as
/// "no such report" sends the model hunting for a seq it was just shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredReport {
    Found {
        seq: i64,
        question: String,
        report: String,
    },
    /// Exists but is no longer valid — never handed to the model.
    Invalid {
        seq: i64,
    },
    Missing {
        seq: i64,
    },
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
    // As with `list_files`, `rename_all = "lowercase"` would spell this
    // `filehistory`.
    #[serde(rename = "file_history")]
    FileHistory {
        path: String,
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
    /// Browse the stored, still-valid research reports of this project.
    #[serde(rename = "list_research")]
    ListResearch {
        #[serde(default)]
        query: Option<String>,
    },
    /// Read one stored report in full, by its per-project seq.
    #[serde(rename = "read_research")]
    ReadResearch {
        seq: i64,
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
             interpreted, the pattern is taken literally. At least 3 characters. An \
             empty result says how much it searched, so \"the text is not there\" and \
             \"nothing here was searchable\" read differently — do not report the \
             second as the first.",
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
            "file_history",
            "The commits that changed this file, newest first, with their messages. \
             This is the only tool that answers WHY the code is the way it is rather \
             than WHAT it does: the reasoning behind a decision is written in the \
             commit that made it, and nowhere in the file itself. Reach for it when the \
             question is why something exists, why it is done this odd way, what was \
             tried before, or what changed recently around a place you are reading — \
             after `outline` or `symbols` has told you which file to ask about. \
             Commit messages are prose, so read them as prose. Two honest limits: the \
             history is walked over a bounded window, so an empty answer may mean \
             \"nothing recent\" rather than \"nothing ever\"; and a file that was moved \
             carries its earlier history under its old name, which the result names \
             when it knows it.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Exact repo-relative path, as returned by another tool."
                    }
                },
                "required": ["path"]
            }),
        ),
        ToolSpec::function(
            "list_research",
            "Earlier research reports stored for this project: the seq, title and \
             question of every run whose evidence still matches the index. Reports \
             already handed to you as context are not repeated. They are HEARSAY — \
             use them to learn names and what was already ruled out, never as \
             citable evidence. Your file scope does not apply to them.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Optional: case-insensitive substring over titles \
                                        and questions."
                    }
                },
                "required": []
            }),
        ),
        ToolSpec::function(
            "read_research",
            "The full Markdown report of one stored run, by its seq from \
             list_research. HEARSAY: you may not cite anything in it — a \
             `path:start-end` copied from it will be reported as invented. Open the \
             code yourself before citing.",
            json!({
                "type": "object",
                "properties": {
                    "seq": {
                        "type": "integer",
                        "description": "The report's seq, as shown by list_research."
                    }
                },
                "required": ["seq"]
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
fn system_prompt(budget: Budget, scope: &ToolScope, has_prior_research: bool) -> String {
    format!(
        r#"You are a code-research agent working over "mindex", a semantic index of ONE project's source code. You cannot open files or browse: the tools are your only access to the code. Gather evidence with them, then answer.

READ THIS — it decides whether you succeed. `search` matches *text*, and this project's code is written in identifiers while your questions are in plain English. A plain-English query tends to return the TEST that describes a behaviour, not the code that implements it. The identifier is what finds the implementation, so your first job is to LEARN THE REAL NAMES and only then search for them:

  list_files → outline → (now you have exact names) → symbols / search / callers → read_chunks

That rule governs CODE. The index also contains this project's DOCUMENTATION — `*.md` files: `README.md`, `CLAUDE.md`, the per-tool READMEs — and there the rule inverts, because documentation is written in the same plain English you think in. Ask those questions in plain English, in the words you would use with a colleague. Documentation is where a project states what it does not say in code: why a design was chosen over the alternative, which steps a change must touch, what an invariant is for. When your question is "why", "what are all the places", or "what is the rule for", search the prose FIRST — it is often a single hit against many steps of reading code, and it will hand you the identifiers to search for next. `list_files` with glob `**/*.md` shows you what documentation exists.

A third channel answers what neither code nor documentation can: the project's GIT HISTORY, through `file_history`. Code says what it does now; documentation says what the project means to do; the commit that made a change says WHY it was made — the alternative that was rejected, the bug that forced it, the measurement behind a number. That reasoning exists nowhere else, so when the question is "why is this like this", "what was tried before", "what changed here recently", or you are staring at code whose shape makes no sense, ask the file's history. Its place in the pipeline is after you know the filename: `outline`/`symbols` tells you WHICH file, then `file_history` tells you why it is that way, and `read_chunks` shows you what it now says. Commit messages are prose, so read them as prose, in plain English — the same inversion documentation gets.

When you take a claim from a commit, cite the CODE it is about — a `path:start-end` you were shown — and name the short sha in your sentence ("added in a1b2c3d4 to ..."). A sha alone is not a citation and will not verify.

This project also keeps STORED RESEARCH REPORTS — the reports earlier runs like this one produced. `list_research` shows the still-valid ones (seq, title, question) and `read_research(seq)` opens one. They are HEARSAY, exactly like any report injected below: the fastest way to learn the real names and what was already ruled out, and never citable — a `path:start-end` copied out of one will be reported as invented, so open the location yourself before citing it. Consulting them early can save most of a cold start. Your file scope does not cover them: they are reports, not files.

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
{scope_rule}{prior_research_rule}"#,
        max_steps = budget.max_steps,
        max_seconds = budget.max_seconds,
        prior_research_rule = if has_prior_research {
            "\nYOU HAVE BEEN GIVEN EARLIER REPORTS on this project, in a message below. They are \
             HEARSAY: prose a model wrote about an earlier state of this tree, never checked \
             against it by you. Read them the way you would read a colleague's note — they are \
             the fastest way to learn the REAL NAMES, which files matter and what has already \
             been ruled out, and that is precisely what a cold run spends its first steps \
             discovering. (They are also excluded from `list_research`, so do not \
             go looking for them there.)\n\
             But you may NOT cite them. A `path:start-end` copied out of an earlier report has \
             not been shown to *you*, and the provenance check that runs before your report \
             ships will mark it unverified exactly as if you had invented it. To cite a \
             location, open it yourself first — `read_chunks`, `outline` or `symbols` — and \
             cite what you were shown.\n\
             Each report says how many of the files it read have changed since. Where that \
             number is not zero, treat its specific claims as most likely wrong and check those \
             first; where it is zero, the report still describes what is there.\n"
        } else {
            ""
        },
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
const REPORT_ROLE: &str = "You are a technical writer. Below is a research \
    question about a codebase and the evidence a research agent gathered for it — \
    code excerpts, symbol locations and file outlines. Your only job now is to write \
    the report. You have NO tools: there is nothing left to call, and any JSON you \
    emit would be discarded. Write Markdown prose, grounded in the evidence, citing \
    locations as `path:start-end`. Begin with a single `# heading` that names the \
    finding.";

/// [`REPORT_ROLE`] plus the length ceiling, when the effort level sets one.
///
/// The second paragraph exists because output volume, not retrieval, is where runs
/// were measured to fail: the loop found the right files every time and then failed
/// to write them up, deterministically by how broad the question was. Nothing in the
/// prompt had ever mentioned length.
///
/// Two things it is careful about. It says **at most**, not "about": a target makes a
/// model write to the number, and the whole finding is that shorter answers survive.
/// And it forbids reproducing code — which is only honest because the server ships
/// the verbatim text of every verified citation itself, on the `excerpts` event. That
/// sentence and that event must ship together; without the channel this would be a
/// demand that the caller pay for what the run already had.
fn report_system_prompt(budget: Budget) -> String {
    if budget.max_report_words == 0 {
        return REPORT_ROLE.to_string();
    }
    format!(
        "{REPORT_ROLE}\n\nLENGTH IS PART OF THE TASK. Write AT MOST {} words. That is a \
         ceiling, not a target: a shorter report that answers the question is a better \
         report, and a long one is the failure this instruction exists to prevent. Do \
         NOT reproduce code you were shown — the server ships the verbatim text of every \
         location you cite alongside your report, so quoting it buys the reader nothing \
         and costs you the report. Cite the location and say what it does.",
        budget.max_report_words
    )
}

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
    shed: usize,
) -> String {
    let mut base = String::from(
        "The investigation is over and the tools are closed. Write the final report \
         answering the original research question, using only the evidence above. \
         Output a complete, self-contained Markdown document (headings, code spans \
         where useful) and NOTHING else — no JSON, no tool call, no preamble. Cite \
         evidence as `path:start-end`, and cite only locations a tool returned in \
         this run.",
    );
    // The shape clause needs both halves to mean anything: a per-section word
    // allowance without a plan to divide by is a number with no denominator, and a
    // plan with no ceiling is what the run already had.
    if budget.max_report_words > 0
        && let Some(sections) = state.plan_item_count()
    {
        base.push_str(&format!(
            "\n\nShape it like this: a `# heading`, then one `##` section per sub-question \
             of your plan, in the plan's order, and nothing else. Keep the whole document \
             under {} words — roughly {} per section. Where a sub-question was not \
             answered, say so in its section in one line rather than filling it.",
            budget.max_report_words,
            (budget.max_report_words / sections.max(1)).max(1)
        ));
    }
    // The anti-speculation rule used to live only in the truncated branch, so a run
    // that finished on its own terms was never told not to guess — and a report that
    // reasoned from a naming convention and presented it as a finding is exactly what
    // that omission buys.
    base.push_str(
        "\n\nGround every claim: state what the evidence shows, and where a claim rests on \
         inference rather than on something a tool returned, say so in the sentence that \
         makes it. Do not pad a gap with what you would expect the code to do. If the \
         evidence was insufficient, say so explicitly and state what is missing.",
    );
    // Silence about a shed transcript is Ollama's failure mode restated in Rust: the
    // model would find holes where results used to be and have no way to know whether
    // it may still cite what they showed. It may — that is the whole point of the
    // digest — and it has to be told so.
    if shed > 0 {
        base.push_str(
            "\n\nNOTE FROM THE SERVER: some of the tool output above was removed to make \
             room for this turn. Nothing you may cite was lost — the list headed \
             \"Evidence: every location this run was shown\" is complete, and it is the \
             full set of locations that will pass the citation check. Where a removed \
             result is what a claim rested on, cite the location and describe it from \
             your notes rather than quoting it.",
        );
    }
    if !reason.is_truncated() {
        return base;
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
    // "Do not pad the gaps" used to live here as well; it now sits in `base`, which
    // every report gets. What stays is the part only a truncated run needs: not
    // presenting an unfinished finding as a settled one.
    format!(
        "IMPORTANT: this investigation did NOT finish — it was stopped by {limit}. Begin \
         the report by saying so in one sentence, so nobody reads it as a complete \
         answer. Then report what you did establish, and be explicit about which parts \
         of the question remain open and what you would have looked at next. Do not \
         present a partial finding as a settled one.{plan}\n\n{base}"
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
    histories: Vec<String>,
    /// Stored-report lookups (`list_research` queries and `read_research` seqs),
    /// one list: both answer "which earlier research have I already consulted".
    report_lookups: Vec<String>,
}

impl RunState {
    /// How many numbered sub-questions the plan states, if it states any.
    ///
    /// `PLAN_REQUEST` asks for 3-6 numbered lines, and models comply often enough
    /// for this to be worth reading — but not reliably enough to depend on, so
    /// every caller must handle `None`. Counts a line whose first non-space
    /// characters are digits followed by `.` or `)`; anything else is prose the run
    /// wrapped its plan in.
    ///
    /// Scanned by `char_indices` rather than by byte offset: a plan is model output,
    /// hence arbitrary UTF-8, and slicing it by byte is the panic this codebase has
    /// already paid for once.
    fn plan_item_count(&self) -> Option<usize> {
        let plan = self.plan.as_deref()?;
        let n = plan
            .lines()
            .filter(|line| {
                let t = line.trim_start();
                let digits = t.chars().take_while(char::is_ascii_digit).count();
                digits > 0 && matches!(t.chars().nth(digits), Some('.') | Some(')'))
            })
            .count();
        (n > 0).then_some(n)
    }

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
            Action::FileHistory { path } => self.histories.push(path.clone()),
            Action::Grep { pattern, glob } => self.greps.push(match glob {
                Some(g) => format!("{pattern} (in {g})"),
                None => pattern.clone(),
            }),
            Action::ListResearch { query } => self.report_lookups.push(match query {
                Some(q) if !q.trim().is_empty() => format!("list ({})", q.trim()),
                _ => "list (all)".to_string(),
            }),
            Action::ReadResearch { seq } => self.report_lookups.push(format!("#{seq}")),
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
        Action::Outline { .. } | Action::ReadChunks { .. } | Action::FileHistory { .. }
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
    state_note_line(
        &mut out,
        "Files whose history you already have",
        &state.histories,
        STATE_NOTE_MAX_ITEMS,
    );
    state_note_line(
        &mut out,
        "Stored reports already consulted",
        &state.report_lookups,
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
    /// Every file the run was shown, at the version it read. Lifted out of the
    /// loop's private `Evidence` because the journal — which runs one level up, in
    /// `run_research` — cannot reach it, and because "what the loop produced" is
    /// exactly this struct's contract.
    file_baselines: Vec<FileBaseline>,
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
    ///
    /// Returns how the run can be named afterwards, or `None` when nothing was
    /// stored. `None` is the *normal* expression of a failed write, not an error
    /// path: the `done` event then carries a null `run_id` and the client simply
    /// cannot offer to reuse a run that was never persisted. A non-optional return
    /// would force either a fabricated identifier or a lie about the contract above.
    async fn record(&self, record: RunRecord) -> Option<RecordedRun>;
}

/// How a stored run is named once it exists.
///
/// Two identifiers on purpose, and they are not interchangeable. `id` is the stable
/// one — a UUID, what every per-run endpoint keys on, safe in a URL or a bookmark.
/// `seq` is a per-project ordinal: short enough to say out loud and to type into a
/// picker, and monotonic enough to be the keyset cursor a paginated list resumes
/// from — but it is renumbered if a project's runs are ever wiped entirely, so it
/// must never be treated as identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRun {
    pub id: String,
    pub seq: i64,
}

/// A file the run read, and the index's hash for it at the moment the run first
/// looked — the persistent half of [`Evidence`]'s `baseline_sha`.
///
/// This is what lets a *stored* report be told apart from a current one later: the
/// same comparison `apply_versions` makes during a run, asked again days afterwards
/// against whatever `project_files` now holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBaseline {
    pub path: String,
    pub sha256: String,
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
    async fn record(&self, _record: RunRecord) -> Option<RecordedRun> {
        None
    }
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
    /// Every file the run was shown, with the index's hash for it when the run first
    /// probed it. Paths never probed are absent, not zero-valued: a baseline nobody
    /// established cannot be compared against later, and inventing one would make a
    /// file read as changed the moment anything touched it.
    ///
    /// The sha is the one from the *first* probe — the version the report was
    /// actually written against. A run whose file moved mid-flight is therefore
    /// stored already-stale, which is correct and is what `stale_citations` on the
    /// same row already said.
    pub file_baselines: Vec<FileBaseline>,
    /// Earlier runs whose reports were injected into this one's transcript, in the
    /// order they were given. Empty for a cold run.
    pub context_run_ids: Vec<String>,
    /// The report's own first ATX heading (`extract_report_title`). None when the
    /// report has no heading or the heading trivially repeats the question —
    /// readers then fall back to a title derived from `question`.
    pub title: Option<String>,
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

    /// What this run read, and the hash it read it at — the persistent form of the
    /// baselines above, for the journal.
    ///
    /// Paths with no `baseline_sha` are **dropped, not defaulted**: the probe never
    /// established a version for them, so there is nothing a later comparison could
    /// be against, and inventing one (an empty string, a zero hash) would make the
    /// file read as changed the first time anyone asked. Sorted, so a run's rows land
    /// in a stable order and two journal writes of the same run are comparable.
    fn baselines(&self) -> Vec<FileBaseline> {
        let mut out: Vec<FileBaseline> = self
            .by_path
            .iter()
            .filter_map(|(path, e)| {
                e.baseline_sha.as_ref().map(|sha| FileBaseline {
                    path: path.clone(),
                    sha256: sha.clone(),
                })
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
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

/// One chunk of indexed code, shipped verbatim beside the report that cites it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportExcerpt {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub code: String,
}

/// Verified citations the excerpt channel will read code for.
///
/// A report that cites forty locations is not asking for forty files to be pasted
/// after it; past this point the channel has stopped being a convenience and become
/// a second copy of the index.
const MAX_EXCERPT_CITATIONS: usize = 24;

/// Total bytes of code the excerpt channel will ship.
///
/// Enforced by dropping **whole chunks**, never by cutting one. A report is
/// arbitrary UTF-8 and so is the code beside it; slicing either by byte is the panic
/// this codebase has already paid for once, and a chunk cut mid-token is not a
/// smaller excerpt but a wrong one.
const MAX_EXCERPT_BYTES: usize = 262_144;

/// A `path:start-end` reference parsed out of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub path: String,
    pub start: usize,
    pub end: usize,
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
    /// The citations that came back `Verified` — the report's own claim about where
    /// it got something, checked against what the run's tools actually returned.
    ///
    /// Not on the wire and not journalled: it exists so the excerpt channel can ship
    /// the *code* at those locations without re-parsing the report or re-deriving
    /// the verdicts. `Verified` only, deliberately — a `path_only` or `unverified`
    /// citation names no location worth reading, and shipping bytes for it would
    /// dress up a claim the check just refused.
    pub verified_locations: Vec<Citation>,
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
        //
        // Walked over **bytes**, and that is load-bearing rather than a
        // micro-optimisation: `report[k - 1..k]` panics outright when byte
        // `k - 1` is inside a multi-byte character, which is any report
        // whose prose is not ASCII. It is exactly equivalent here because
        // `is_path_char` accepts only ASCII, so a non-ASCII byte ends the
        // path either way — and a byte below 0x80 is always a char boundary.
        // Measured in production: `gpt-oss:20b` writes OpenAI-style `【…】`
        // citation brackets, and one of them landing before a `:N-M` killed
        // the whole research job, so the run was never journalled and the
        // client saw a stream that simply stopped. Russian prose does it too.
        let mut k = i;
        while k > 0 && b[k - 1].is_ascii() && is_path_char(b[k - 1] as char) {
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
            Verdict::Verified => {
                r.verified += 1;
                if !r.verified_locations.contains(c) {
                    r.verified_locations.push(c.clone());
                }
            }
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

/// Read the indexed code at every verified citation, for the `excerpts` event.
///
/// Best-effort by construction: every failure is a `warn!` and a shorter list, never
/// an error. The report has already shipped, and a run must not be turned into a
/// failure by the convenience channel that follows it.
///
/// Scope is enforced because `read_chunks` enforces it — this channel must never
/// become the way a scoped run hands over bytes its scope refused. Deduplicated by
/// `(path, chunk span)`: several citations into one chunk are one excerpt, which is
/// also why the caps are counted over chunks rather than over citations.
async fn collect_excerpts(
    tools: &dyn ResearchTools,
    params: &ResearchParams,
    citations: &CitationReport,
    token: &CancellationToken,
) -> (Vec<ReportExcerpt>, usize, bool) {
    let total = citations.verified_locations.len();
    let mut out: Vec<ReportExcerpt> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = total > MAX_EXCERPT_CITATIONS;
    for c in citations
        .verified_locations
        .iter()
        .take(MAX_EXCERPT_CITATIONS)
    {
        if token.is_cancelled() {
            return (out, total, true);
        }
        let resp = match tools
            .read_chunks(c.path.clone(), c.start, c.end, &params.scope, token)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    error = ?e,
                    path = %c.path,
                    "Failed to read the code for a verified citation; the report ships \
                     without that excerpt."
                );
                truncated = true;
                continue;
            }
        };
        for chunk in resp.chunks {
            if out
                .iter()
                .any(|x| x.path == c.path && x.start_line == chunk.start_line)
            {
                continue;
            }
            // Whole chunks only: the byte cap drops the next one rather than
            // cutting this one in half.
            if bytes + chunk.code.len() > MAX_EXCERPT_BYTES {
                truncated = true;
                continue;
            }
            bytes += chunk.code.len();
            out.push(ReportExcerpt {
                path: c.path.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                code: chunk.code,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.start_line.cmp(&b.start_line)));
    (out, total, truncated)
}

/// The report's first ATX heading, as the stored display title.
///
/// None when the report has no heading, the heading is empty, or it trivially
/// repeats the question (case- and whitespace-insensitive equality) — a heading
/// that merely echoes the question adds nothing over the question column the
/// readers already fall back to, so storing it would only make the two drift
/// apart visually. Hand-rolled: the grammar is one line.
pub fn extract_report_title(report: &str, question: &str) -> Option<String> {
    let line = report.lines().find(|l| !l.trim().is_empty())?.trim();
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    // A closing ATX sequence (`# Title #`) is part of the syntax, not the title.
    let title = rest.trim().trim_end_matches('#').trim();
    if title.is_empty() {
        return None;
    }
    let collapse = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    if collapse(title) == collapse(question) {
        return None;
    }
    Some(title.to_string())
}

/// The one problem [`validate_report_markdown`] reports that the server can fix
/// itself — named as a const because [`repair_missing_heading`] keys on it, and a
/// reworded complaint that no longer matched would silently turn the repair off.
const MISSING_HEADING: &str = "The report must begin with a `# heading` naming the finding.";

/// Honest structural checks on a finished report — empty vec means valid.
///
/// tree-sitter-md accepts *anything* (an unclosed fence or stray HTML yields a
/// block, never an error node), so parsing it would be a validator that cannot
/// fail — worse than none, because it would read as coverage. What is honestly
/// checkable is shape: the defects a broken report actually ships with are JSON
/// where prose was asked for, a missing heading, and an unclosed code fence that
/// swallows the rest of the document in every renderer. Each entry is one concrete
/// problem, worded for the model — the list is sent back verbatim as a complaint.
pub fn validate_report_markdown(report: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let trimmed = report.trim();
    if trimmed.is_empty() {
        problems.push("The report is empty.".to_string());
        return problems;
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        problems.push(
            "The report begins with JSON, not Markdown prose. Output a Markdown document."
                .to_string(),
        );
    }
    let first_line = trimmed.lines().next().unwrap_or_default().trim();
    let hashes = first_line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) || !first_line[hashes..].starts_with(char::is_whitespace) {
        problems.push(MISSING_HEADING.to_string());
    }
    let fences = report
        .lines()
        .filter(|l| l.trim_start().starts_with("```"))
        .count();
    if fences % 2 != 0 {
        problems.push("An unclosed ``` code fence: every fence opened must be closed.".to_string());
    }
    problems
}

/// Write the heading a report is missing, rather than discarding the report over it.
///
/// The gate exists to keep malformed prose out of the corpus, because a stored report
/// is fed to a later run as context. A missing top heading fails none of that: the
/// analysis, the citations and the structure below it are whatever the model made
/// them, and the defect is one line of syntax the server can supply as well as the
/// model can. Measured here on 2026-07-31: of three runs that reached a finished
/// report, one was thrown away for exactly this and nothing else — a full local
/// investigation discarded over a `#`.
///
/// Repairs **only** when the missing heading is the *sole* problem. A report that
/// also begins with JSON, or that leaves a fence open, is broken in ways no
/// substitution fixes: prepending a heading to JSON would produce something that
/// passes the gate while still being unusable as prose, which is worse than the
/// refusal. Those keep going back to the model, then keep being refused.
///
/// The heading is derived from the question by the same rule the readers already
/// fall back to (`research_title`), so a repaired report is titled exactly as an
/// untitled one is displayed. Deliberately not recorded as a flag on the run: it is
/// derivable from the row (`title IS NULL` and the report opens with that
/// derivation) and a column costs a table rebuild.
///
/// Returns whether it wrote one. Idempotent — a second call on a repaired report
/// finds no problem and changes nothing.
fn repair_missing_heading(report: &mut String, question: &str) -> bool {
    let problems = validate_report_markdown(report);
    if problems.len() != 1 || problems[0] != MISSING_HEADING {
        return false;
    }
    let derived = research_title(question);
    // An empty question cannot happen through the API (validation rejects it), but a
    // heading is what this function promises to produce, so it must not depend on that.
    let title = if derived.trim().is_empty() {
        "Research report"
    } else {
        derived.trim()
    };
    *report = format!("# {title}\n\n{}", report.trim_start());
    true
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
    // An empty result has three meanings and used to have one sentence — the
    // `file_history` three-flag problem, in counter form. The middle branch is the
    // one that was missing: a glob matching no file, or a scope holding none, is not
    // evidence about the pattern at all, and reporting it as "no chunk contains
    // this" is how the same literal is honestly reported absent by one run and found
    // five times by the next.
    if resp.matches.is_empty() {
        if resp.out_of_scope > 0 {
            return format!(
                "No occurrence of \"{pattern}\" within this run's scope ({}), though {} \
                 exist outside it.",
                scope.describe(),
                resp.out_of_scope
            );
        }
        if resp.searched_files == Some(0) {
            return format!(
                "Nothing here was searchable: no indexed chunk was in reach of this \
                 search. This is NOT a fact about \"{pattern}\" — the files it would \
                 live in are out of reach, so do not read it as \"the text does not \
                 exist\". Check the glob you passed, or say in your report that the \
                 text is not reachable from this index."
            );
        }
        return match (resp.searched_chunks, resp.searched_files) {
            (Some(chunks), Some(files)) => format!(
                "No occurrence of \"{pattern}\" in the {chunks} indexed chunk(s) across \
                 {files} file(s) this search could reach. The match is literal and \
                 case-insensitive, so check the spelling — or the text may live in a \
                 part of a file the slicer left out of every chunk."
            ),
            _ => format!(
                "No indexed chunk contains \"{pattern}\". The match is literal and \
                 case-insensitive, so check the spelling — or the text may live in a \
                 part of a file the slicer left out of every chunk."
            ),
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

/// One file's commits, as prose the model is meant to read as prose.
///
/// The three empty cases are spelled out rather than collapsed into one, because
/// they call for three different next moves: reconcile the channel, ask about a
/// path the run may read, or accept that the file predates the window. A single
/// "no results" would send the model looking for a spelling problem in all three.
fn format_file_history(path: &str, resp: &FileHistoryResponse, scope: &ToolScope) -> String {
    if !resp.in_scope {
        return out_of_scope_reply(path, scope);
    }
    if !resp.history_indexed {
        return format!(
            "This project has NO indexed git history at all, so nothing can be said \
             about {path} from commits — this is not a fact about {path}. The history \
             channel is opt-in and was never reconciled for this project. Do not ask \
             for another file's history; use the code tools instead, and if the \
             question needs the reasoning behind a change, say in your report that it \
             is not reachable from this index."
        );
    }
    if resp.commits.is_empty() {
        return format!(
            "No indexed commit touches {path}. The project HAS an indexed history, so \
             this means the file changed only outside the walked window, or under an \
             earlier name — a rename is followed only when git detected it. {}",
            if resp.path_indexed {
                "The file itself is indexed and readable with the code tools."
            } else {
                "The file is also absent from the code index, so check the path first."
            }
        );
    }

    let mut out = if resp.total > resp.commits.len() {
        format!(
            "{} of {} indexed commits touching {path}, newest first:\n",
            resp.commits.len(),
            resp.total
        )
    } else {
        format!(
            "{} indexed commit(s) touching {path}, newest first:\n",
            resp.commits.len()
        )
    };
    if !resp.path_indexed {
        out.push_str(
            "\n(This path has history but is NOT in the code index — deleted, excluded \
             from this project's scope, or in an unsupported language. Its commits are \
             still real; its current contents are not readable here.)\n",
        );
    }
    for c in &resp.commits {
        out.push_str(&format!("\n--- {} · {}", c.short_sha, c.author_name));
        match (&c.old_path, c.change_type) {
            (Some(old), ChangeType::Renamed) => {
                out.push_str(&format!(" · renamed from {old}"));
            }
            (Some(old), ChangeType::Copied) => out.push_str(&format!(" · copied from {old}")),
            _ => out.push_str(&format!(" · {}", c.change_type.name())),
        }
        out.push_str(&format!("\n{}\n", c.subject));
        if !c.body.trim().is_empty() {
            out.push_str(&format!("{}\n", c.body.trim()));
        }
    }
    // Said on every result, not only in the tool description: by the time this is
    // read the description is thousands of tokens back, and a sha reads as a
    // citation unless something says it is not one.
    out.push_str(
        "\n(Cite what you take from these by the `path:start-end` in the CODE that the \
         claim is about, naming the short sha in your prose. A sha is not a citation.)\n",
    );
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

/// Characters per token when sizing a prompt the server assembled but has not sent.
///
/// A rough ratio, and the number in this module most likely to be wrong: it comes
/// from prose, and code tokenizes far denser — a transcript full of `read_chunks`
/// fences may be closer to 3. It is only used to decide whether to shed, where
/// erring low means shedding a little early and erring high means not shedding when
/// it was needed. Log the estimate against the turn's real `prompt_tokens` before
/// trusting it; do not tune it from intuition, which has already lost twice here.
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;

/// Size an assembled transcript without asking Ollama.
///
/// The alternative is to send it and find out, which is exactly the failure this
/// guards: Ollama trims an over-long prompt to `num_ctx` and streams on as if
/// nothing happened.
fn estimate_prompt_tokens(messages: &[ChatMessage]) -> u64 {
    let chars: usize = messages.iter().map(|m| m.content.chars().count()).sum();
    (chars / CHARS_PER_TOKEN_ESTIMATE) as u64
}

/// The complete list of locations the run was shown, pushed before the report
/// request.
///
/// It is what [`REPORT_SYSTEM_PROMPT`](REPORT_ROLE) has always claimed sits below it
/// ("the evidence a research agent gathered"), and what `check_citations` actually
/// scores against — so stating it explicitly costs a few hundred tokens and removes
/// the gap between what the model is told it may cite and what will pass. It is also
/// what makes shedding safe: a tool result can be dropped from the transcript
/// without dropping the *citability* of what it showed.
///
/// Paths and spans only, never code. A path with no span is marked as such rather
/// than omitted, so the model does not invent a range for a file it only knows
/// exists.
fn format_evidence_digest(evidence: &Evidence) -> String {
    let paths = evidence.paths();
    let mut out = format!(
        "Evidence: every location this run was shown ({} file(s)). These, and only \
         these, are what your citations are checked against.\n\n",
        paths.len()
    );
    for path in &paths {
        let Some(e) = evidence.by_path.get(path) else {
            continue;
        };
        if e.spans.is_empty() {
            out.push_str(&format!("{path} — shown, no line range\n"));
            continue;
        }
        let mut spans: Vec<&Span> = e.spans.iter().collect();
        spans.sort_by_key(|s| (s.start, s.end));
        let ranges: Vec<String> = spans
            .iter()
            .map(|s| format!("{}-{}", s.start, s.end))
            .collect();
        out.push_str(&format!("{path}:{}\n", ranges.join(", ")));
    }
    out
}

/// Drop old tool output until the report turn's prompt fits the window.
///
/// Returns how many replies were shed. The order is deliberate and the floor is
/// load-bearing:
///
/// - The prior-reports block goes first. It is hearsay by contract, was never
///   citable, and by the report turn its whole value — telling the run what names to
///   look for — has already been spent.
/// - Then `role: "tool"` replies, oldest first, because a run's early turns are
///   orientation (`list_files`, `outline`) that the run-state note already
///   summarises, while its late reads are what its conclusions rest on.
/// - Nothing else is ever shed: the system prompt, the question, the plan, the
///   notes, the sufficiency verdict, the evidence digest and the report request are
///   short, and they are what the report's structure and grounding rest on. If even
///   that floor is over the ceiling, this gives up and says so rather than shipping
///   nothing.
///
/// A shed reply is **replaced by a stub, never removed**. Every announced tool call
/// gets exactly one `role: "tool"` reply, in order; an assistant turn asking for
/// three calls followed by two replies is a malformed transcript, and some templates
/// fail on it outright.
fn shed_for_report(
    messages: &mut [ChatMessage],
    prior_reports_idx: Option<usize>,
    ceiling: u64,
) -> usize {
    let mut shed = 0;
    if let Some(i) = prior_reports_idx
        && estimate_prompt_tokens(messages) > ceiling
        && let Some(m) = messages.get_mut(i)
        && !m.content.is_empty()
    {
        m.content = "[Removed by the server to fit the context window: the earlier \
                     reports given to this run as context. They were hearsay and were \
                     never citable.]"
            .to_string();
        shed += 1;
    }
    for i in 0..messages.len() {
        if estimate_prompt_tokens(messages) <= ceiling {
            break;
        }
        let m = &mut messages[i];
        if m.role != "tool" || m.content.starts_with("[Removed by the server") {
            continue;
        }
        // Naming what was there keeps the reply honest about being a hole rather
        // than about being empty — and points at where the location still lives.
        let tool = m.tool_name.clone().unwrap_or_else(|| "a tool".to_string());
        m.content = format!(
            "[Removed by the server to fit the context window: this was the result of \
             `{tool}`. Whatever it showed is still citable — the evidence list below \
             is complete.]"
        );
        shed += 1;
    }
    shed
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
        Action::FileHistory { path } => {
            let resp = tools
                .file_history(path.clone(), &params.scope, token)
                .await
                .map_err(ResearchAbort::from)?;
            let hits = resp.commits.len();
            let text = format_file_history(path, &resp, &params.scope);
            // ONLY the asked path, and span-less. A commit touched other files,
            // but the model was not shown them — recording them here would mark
            // files it never read as "shown", quietly promoting a later invented
            // citation from `unverified` to `path_only` and blinding the gate.
            // Span-less because a commit has no line range at all; the citation
            // it grounds is the file, not a region of it.
            let shown = if resp.in_scope && !resp.commits.is_empty() {
                vec![(resp.path.clone(), None)]
            } else {
                vec![]
            };
            Ok(Executed {
                call: StepCall::FileHistory { path: path.clone() },
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
        // The two stored-report tools. Both return `shown: Vec::new()`
        // unconditionally — the hearsay invariant: a stored report is another
        // run's prose, not this run's evidence, so nothing in it may seed
        // `Evidence`. A `path:start-end` copied out of one therefore lands
        // `unverified`, exactly as `format_prior_reports`' injected blocks do.
        Action::ListResearch { query } => {
            let mut listings = tools
                .list_research(query.clone(), token)
                .await
                .map_err(ResearchAbort::from)?;
            // The runs already injected as context are in the transcript in
            // full; repeating their titles here would invite re-reading them.
            listings.retain(|l| !params.prior_reports.iter().any(|p| p.id == l.id));
            let hits = listings.len();
            let text = format_research_listing(&listings);
            Ok(Executed {
                call: StepCall::ListResearch {
                    query: query.clone().unwrap_or_default(),
                },
                hits,
                text,
                shown: Vec::new(),
            })
        }
        Action::ReadResearch { seq } => {
            let stored = tools
                .read_research(*seq, token)
                .await
                .map_err(ResearchAbort::from)?;
            let (hits, text) = match stored {
                StoredReport::Found {
                    seq,
                    question,
                    report,
                } => (
                    1,
                    format_stored_report(seq, &question, &report, params.max_context_chars),
                ),
                StoredReport::Invalid { seq } => (
                    0,
                    format!(
                        "Report #{seq} exists but is no longer valid: its evidence, or a \
                         report it depended on, has changed or been deleted since it was \
                         written. It cannot be used."
                    ),
                ),
                StoredReport::Missing { seq } => (0, format!("No stored report #{seq}.")),
            };
            Ok(Executed {
                call: StepCall::ReadResearch {
                    seq: seq.to_string(),
                },
                hits,
                text,
                shown: Vec::new(),
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
        file_baselines,
        mut report,
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
        // The one thing that tells a `total: 0` written by the server apart from a
        // `total: 0` the model chose. Sourced from the same flag the journal has
        // always recorded — the fact existed, it just never reached the wire.
        server_written: run_tools.forced_synthesis,
    });
    // The code behind the citations, verbatim. Under the **job** token, not the
    // report window: that window bounds what the *model* is given to write in, and
    // this is one SQL read the server owes the caller afterwards. Emitted between
    // `citations` and `done` so a reader that stops at `done` has already seen it.
    if !citations.verified_locations.is_empty() {
        let (excerpts, total, truncated) =
            collect_excerpts(&*tools, &params, &citations, &token).await;
        if !excerpts.is_empty() {
            let _ = tx.send(ResearchEvent::Excerpts {
                excerpts,
                total,
                truncated,
            });
        }
    }
    // Granted-versus-actual, the measurement the length ceiling exists to produce.
    // Counted in words rather than characters or tokens because words are what the
    // model was asked for; comparing the grant against a different unit answers
    // nothing.
    if let Some(m) = &params.metrics {
        m.research
            .report_words
            .get_or_create(&crate::backend::metrics::ModelLabels {
                model: params.model.clone(),
            })
            .observe(report.split_whitespace().count() as f64);
    }
    // Journalled after the events are queued, not before: the client's report must
    // never wait on a database write, and a write failure must not change what the
    // client saw.
    //
    // A report that is still structurally broken Markdown after the repair pass is
    // streamed (a broken report the client watched beats a silently vanished one)
    // but never journalled: the corpus is what later runs are fed as context, and
    // storing JSON-as-a-report there would inject it into a future transcript as
    // prose. `done` then carries null run_id/seq — the same wire shape as a failed
    // journal write, which is what this is. A forced synthesis is server-written
    // and valid by construction, so the gate exempts it rather than re-parsing it.
    //
    // The title is read from the model's own text *before* the repair below, so a
    // server-written heading is never mistaken for one the model chose: a repaired
    // run stores no title and its readers keep falling back to the question — which
    // is the same string the repair derived the heading from.
    let title = extract_report_title(&report, &params.question);
    // The second and last repair site. The first (in `research_inner`) catches an
    // unstreamed draft; this one catches a rewrite, which the client has already
    // watched arrive — so here, and only here, the stored report can carry a heading
    // line the live view did not show. That divergence is one derived line weighed
    // against losing the whole run, and it resolves towards the corpus, because the
    // corpus is what a later run reads.
    if repair_missing_heading(&mut report, &params.question) {
        info!(
            model = %params.model,
            "The final report had no heading; the server wrote one so the run could be \
             journalled."
        );
    }
    let md_problems = validate_report_markdown(&report);
    let recorded = if md_problems.is_empty() || run_tools.forced_synthesis {
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
                // What this run read and at which version, so a reader days from
                // now can be told whether the report still describes the tree.
                file_baselines,
                context_run_ids: params.prior_reports.iter().map(|p| p.id.clone()).collect(),
                title,
                report,
            })
            .await
    } else {
        warn!(
            model = %params.model,
            problems = ?md_problems,
            "The final research report is structurally broken Markdown even after \
             the repair pass; it was streamed to the client but will not be \
             journalled."
        );
        None
    };
    let _ = tx.send(ResearchEvent::Done {
        progress,
        context_fraction: params.budget.context_fraction,
        reason,
        recorded,
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
        ChatMessage::system(system_prompt(
            params.budget,
            &params.scope,
            !params.prior_reports.is_empty(),
        )),
        ChatMessage::user(format!("Research question:\n{}", params.question)),
    ];
    // Before the plan turn on purpose: the plan is the run's only sufficiency
    // criterion, and prior work is exactly the input that should change which
    // sub-questions are worth asking.
    // Remembered so the report turn's size guard can shed it first: it is hearsay by
    // contract, never citable, and by then its whole value — telling the run what
    // names to look for — has already been spent. Safe as a fixed index because it
    // sits before every message the loop later removes and re-pins.
    let mut prior_reports_idx = None;
    if !params.prior_reports.is_empty() {
        let (block, truncated) =
            format_prior_reports(&params.prior_reports, params.max_context_chars);
        if truncated && let Some(m) = params.metrics.as_ref() {
            m.research.context_truncations.inc();
        }
        prior_reports_idx = Some(messages.len());
        messages.push(ChatMessage::user(block));
    }

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
                         grep, symbols, outline, callers, list_files, read_chunks, \
                         file_history, list_research, read_research, note, \
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
                    Action::FileHistory { path } => {
                        format!("file_history\u{0}{}", path.trim())
                    }
                    Action::ReadChunks {
                        path,
                        start_line,
                        end_line,
                    } => format!(
                        "read_chunks\u{0}{}\u{0}{start_line}\u{0}{end_line}",
                        path.trim()
                    ),
                    Action::ListResearch { query } => {
                        format!("list_research\u{0}{:?}", query.as_deref().map(str::trim))
                    }
                    Action::ReadResearch { seq } => format!("read_research\u{0}{seq}"),
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
    messages[0] = ChatMessage::system(report_system_prompt(params.budget));
    // The notes, pinned once for the report. The run-state *note* is deliberately not
    // rebuilt here, but these are different in kind: they are the conclusions the model
    // reached and chose to keep, and the report is what they were kept for. A message of
    // their own rather than folded into the instruction below, because that instruction
    // is fixed text under `PROMPT_VERSION` while this is the run's own content — and
    // because it must survive the rewrite turn's second instruction unchanged.
    if let Some(notes) = format_notes_note(&state) {
        messages.push(ChatMessage::user(notes));
    }
    // Unconditional: it is what the system prompt above already promises sits below
    // it, it is what `check_citations` scores against, and two prompt shapes would be
    // two things to measure. Cheap — paths and spans, no code.
    messages.push(ChatMessage::user(format_evidence_digest(&evidence)));
    // The transcript's own guard runs *between* turns and against the previous turn's
    // prompt, so the report turn — the only one that adds the notes block and the
    // instruction on top, and the largest prompt of the run once a draft and a
    // complaint join it — has until now been measured by nothing. `None` here means no
    // turn ever reported a window; do not substitute the configured ceiling, which
    // over-estimates the real one precisely when shedding would matter.
    let shed = match context_ceiling(&tally, params.budget.context_fraction) {
        None => {
            debug!("No turn reported a context window; skipping the report-turn size guard.");
            0
        }
        Some(ceiling) => {
            // What the report turn is allowed to generate is not available for the
            // prompt, so the ceiling it must fit under is the window minus the grant.
            let reserved =
                params.budget.max_report_words as u64 * crate::config::REPORT_WORDS_TO_TOKENS;
            let ceiling = ceiling.saturating_sub(reserved).max(1);
            let estimated = estimate_prompt_tokens(&messages);
            if estimated <= ceiling {
                0
            } else {
                let shed = shed_for_report(&mut messages, prior_reports_idx, ceiling);
                warn!(
                    estimated_prompt_tokens = estimated,
                    ceiling,
                    num_ctx = tally.num_ctx,
                    shed,
                    after = estimate_prompt_tokens(&messages),
                    "The report turn's prompt was over the context ceiling; dropped old \
                     tool output to fit rather than letting Ollama trim it in silence."
                );
                if let Some(m) = &params.metrics {
                    m.research.report_context_sheds.inc();
                }
                shed
            }
        }
    };
    messages.push(ChatMessage::user(report_request(
        reason,
        &state,
        params.budget,
        started.elapsed(),
        shed,
    )));
    // Set (never cleared) by any report turn whose reply was cut at `num_predict`.
    // Its only use is the rewrite instruction below: a report that was cut is the
    // one case where "write it again" should also say "write it shorter".
    let mut length_capped = false;
    let drafted = write_report(
        ollama,
        params,
        &mut messages,
        tx,
        token,
        writing,
        &mut tally,
        false,
        &mut length_capped,
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
    // The heading is repaired before the gate reads the draft, and after the
    // citations are counted: the derived heading comes from the question, which can
    // itself contain a `path.rs:1-2`, and a server-written line must never enter the
    // provenance report as a claim the model did not make. Nothing has streamed the
    // draft at this point — the content gate held it — so the caller receives the
    // repaired text, byte-for-byte what is journalled.
    if repair_missing_heading(&mut summary, &params.question) {
        info!("The draft report had no heading; the server wrote one rather than sending it back.");
    }
    // Structural Markdown defects join the gate: a broken draft is never
    // journalled (see `run_research`), so sending it back while the gate is
    // already closed is the one chance to store the run at all.
    let md_problems = validate_report_markdown(&summary);
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
    //    the same answer — but only from a run the budget stopped. A run that
    //    *finalized* declared its own evidence sufficient; a short uncited report
    //    from it is not the honest short version, it is a self-contradiction, and
    //    exempting it is how a run that read a dozen files ships prose citing none.
    let ungrounded = !forced
        && citations.total == 0
        && !evidence.paths().is_empty()
        && (summary.chars().count() >= MIN_GROUNDED_REPORT_CHARS
            || reason == DoneReason::Finalized);
    // Staleness joins the two provenance defects in the gate: a claim cited to a
    // file the index has since rewritten is as unsupported as one cited nowhere,
    // and the remedy is the same — re-read it, then correct or drop the claim.
    let citation_defects =
        ungrounded || citations.unverified + citations.path_only + citations.stale > 0;
    if !forced && (citation_defects || !md_problems.is_empty()) {
        if ungrounded {
            info!(
                report_chars = summary.chars().count(),
                shown_paths = evidence.paths().len(),
                "The draft report cites nothing checkable; sending it back to be grounded."
            );
        }
        if !md_problems.is_empty() {
            info!(
                problems = ?md_problems,
                "The draft report is structurally broken Markdown; sending it back."
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
        // drop the claim, which is the honest half of the fix. A markdown-only
        // defect re-opens nothing either way — reformatting needs no lookups, so
        // it goes straight to the rewrite turn.
        let tools_reopen = reason == DoneReason::Finalized && citation_defects;
        // The complaint goes out either way. Which citations failed is the whole
        // content of the instruction: told only "some did not check out", a model
        // can do nothing but guess, and guessing means rewriting the ones that
        // were right — the same reason the complaint names locations rather than
        // counts. Structural problems ride in the same message: one combined
        // complaint, one repair pass.
        if tools_reopen {
            messages[0] = ChatMessage::system(REVALIDATION_SYSTEM_PROMPT);
        }
        let mut complaint = String::new();
        if citation_defects {
            complaint.push_str(&format_citation_complaint(
                &summary,
                &evidence,
                tools_reopen,
            ));
        }
        if !md_problems.is_empty() {
            if !complaint.is_empty() {
                complaint.push_str("\n\n");
            }
            complaint.push_str(&format_markdown_complaint(&md_problems));
        }
        messages.push(ChatMessage::user(complaint));
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
        messages[0] = ChatMessage::system(report_system_prompt(params.budget));
        let mut rewrite_request = String::from(
            "Now write the final report. It replaces the draft entirely, so repeat \
             everything that should survive. Keep every claim the evidence supports, \
             fix the citations that did not check out — point them at a location a \
             tool actually returned — and drop any claim you could not ground. \
             Markdown only, no preamble, no JSON.",
        );
        // Only when the draft was actually severed. Saying it unconditionally would
        // teach every rewrite to shrink a report that was the right length.
        if length_capped {
            rewrite_request.push_str(
                "\n\nYour previous reply was cut off at the generation limit before it \
                 finished, which is why the draft ends where it does. Write a shorter \
                 report — cover the same ground in fewer words.",
            );
        }
        messages.push(ChatMessage::user(rewrite_request));
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
            &mut length_capped,
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
        file_baselines: evidence.baselines(),
        report: summary,
    })
}

/// Render a `list_research` result for the model.
fn format_research_listing(listings: &[ResearchListing]) -> String {
    if listings.is_empty() {
        return "No stored reports.".to_string();
    }
    let mut out = format!(
        "{} stored report(s), newest first. These are HEARSAY — read one with \
         read_research(seq) if it looks relevant; nothing in them is citable.\n",
        listings.len()
    );
    for l in listings {
        let title = l.title.as_deref().unwrap_or(&l.question);
        out.push_str(&format!("- #{}: {}\n", l.seq, title));
    }
    out
}

/// Render one stored report for the model, truncated out loud at the same cap as
/// an injected prior report — a stored body is unbounded, and an unbounded tool
/// reply is prompt tokens on every later turn.
fn format_stored_report(seq: i64, question: &str, report: &str, max_chars: usize) -> String {
    let mut out = format!(
        "Stored report #{seq} — {question}\n(HEARSAY: another run's prose, not \
         evidence. Verify anything you intend to state; a citation copied from it \
         will be reported as invented.)\n\n"
    );
    let budget = max_chars.saturating_sub(out.len());
    if report.len() <= budget {
        out.push_str(report);
    } else {
        let mut cut = budget;
        while cut > 0 && !report.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push_str(&report[..cut]);
        out.push_str(
            "\n[This report was TRUNCATED to fit the context budget. Do not treat \
             its ending as its conclusion.]",
        );
    }
    out
}

/// Render the earlier reports a run was given, as one `user` message.
///
/// A `user` message and not an assistant one, for the reason the run-state note is:
/// this is not something *this* model said, and attributing another run's prose to
/// the assistant would let it be mistaken for its own established reasoning.
///
/// Each section states its own staleness in the header sentence. That is the whole
/// difference between handing the model background and handing it a trap: a report
/// written against files that have since moved is still useful for names and shape,
/// and actively misleading about specifics, and only the header says which.
///
/// Returns the block and whether the last report had to be truncated to fit
/// `max_chars` — the caller counts that, because a cap nobody can see the effect of
/// cannot be tuned.
fn format_prior_reports(reports: &[PriorReport], max_chars: usize) -> (String, bool) {
    let mut out = String::from(
        "Earlier research on this project. These are REPORTS, not tool output: prose written \
         by a model about an earlier state of this tree. Use them for orientation — the names, \
         files and shape of the answer — and verify anything you intend to state.\n",
    );
    let mut truncated = false;
    for r in reports {
        let freshness = if r.files_moved == 0 {
            format!("all {} files it read still match the index", r.files_total)
        } else {
            format!(
                "{} of the {} files it read have CHANGED or been removed since — treat its \
                 specifics as most likely wrong, and check those first",
                r.files_moved, r.files_total
            )
        };
        let header = format!(
            "\n## Earlier report #{} — {}\n({})\n\n",
            r.seq, r.question, freshness
        );

        // The budget is over the whole block, so what is left shrinks as sections are
        // added. A section whose header alone would not fit is dropped entirely
        // rather than half-written.
        let remaining = max_chars.saturating_sub(out.len());
        if remaining <= header.len() {
            truncated = true;
            break;
        }
        out.push_str(&header);
        let room = remaining - header.len();
        if r.report.len() <= room {
            out.push_str(&r.report);
        } else {
            // Cut on a char boundary, and SAY SO. A silently clipped report lets the
            // model reason from half a conclusion and present it whole — the same
            // argument the `note` cap makes for announcing what it drops.
            let mut cut = room.min(r.report.len());
            while cut > 0 && !r.report.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push_str(&r.report[..cut]);
            out.push_str(
                "\n\n[This report was TRUNCATED to fit the context budget. Do not treat \
                          its ending as its conclusion.]",
            );
            truncated = true;
        }
        out.push('\n');
    }
    (out, truncated)
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

/// The message that sends back a draft whose Markdown is structurally broken.
///
/// The problems come from [`validate_report_markdown`] verbatim: each names one
/// concrete defect, so the model fixes the form without touching claims that were
/// right. Distinct from the citation complaints because the remedy is different —
/// nothing needs to be looked up, only rewritten.
fn format_markdown_complaint(problems: &[String]) -> String {
    let mut out =
        String::from("Your draft is not published yet — its Markdown is structurally broken:\n");
    for p in problems {
        out.push_str(" - ");
        out.push_str(p);
        out.push('\n');
    }
    out.push_str(
        "\nWhen you are asked to rewrite the report, produce a well-formed Markdown \
         document — keep every claim and citation that was right as it was.\n",
    );
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
///
/// `length_capped` is an out-flag, set (never cleared) when a turn here reached its
/// `num_predict` ceiling. It is not part of the outcome because it is not a result:
/// the report may be perfectly usable and merely long. The caller needs it because
/// the one useful response — telling the rewrite turn to be shorter — belongs to the
/// message the caller owns.
async fn write_report(
    ollama: &dyn OllamaModel,
    params: &ResearchParams,
    messages: &mut Vec<ChatMessage>,
    tx: &UnboundedSender<ResearchEvent>,
    job: &CancellationToken,
    writing: &CancellationToken,
    tally: &mut TokenTally,
    stream_content: bool,
    length_capped: &mut bool,
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
    // The one turn of the run that bounds its own generation. `0` (the knob off)
    // means no bound at all, exactly as before this existed.
    let num_predict = match params.budget.max_report_words {
        0 => None,
        w => Some(w as u64 * crate::config::REPORT_WORDS_TO_TOKENS),
    };
    let mut report_attempt = 0;
    let mut summary = loop {
        let sampling = Sampling {
            seed: params
                .sampling
                .seed
                .map(|s| s.wrapping_add(report_attempt as i64)),
            num_predict,
            ..params.sampling
        };
        let opts = TurnOpts {
            stream_content,
            sampling,
        };
        let outcome =
            tally.record(chat_turn(ollama, params, messages, NO_TOOLS, tx, writing, opts).await?);
        // Ollama does not tell us *why* it stopped, so reaching the ceiling is the
        // only signal that it cut the reply rather than the model finishing. It is a
        // defect either way: the cap is sized ~3x the honest prose ratio precisely so
        // an overshooting report never meets it, so meeting it means the multiplier
        // or the model is wrong — and a cut lands mid-token, which can sever a fence
        // and cost a full rewrite.
        let capped = matches!((num_predict, outcome.eval_tokens), (Some(n), Some(e)) if e >= n);
        if capped {
            warn!(
                model = %params.model,
                num_predict = ?num_predict,
                eval_tokens = ?outcome.eval_tokens,
                granted_report_words = params.budget.max_report_words,
                "The report turn hit its generation ceiling, so the reply was cut \
                 rather than finished. Sysadmin: raise [research.effort.*].max_report_words \
                 or lower it so the model stops writing before the ceiling stops it."
            );
            if let Some(m) = &params.metrics {
                m.research.report_length_caps.inc();
            }
        }
        *length_capped |= capped;
        let content = outcome.content;
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
                        // The re-ask is still a report turn and is bounded like one.
                        // The seed is deliberately *not* shifted: the transcript
                        // changed, which is the whole point of asking again.
                        sampling: Sampling {
                            num_predict,
                            ..params.sampling
                        },
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

    fn prior(seq: i64, report: &str, files_moved: usize, files_total: usize) -> PriorReport {
        PriorReport {
            id: format!("run-{seq}"),
            seq,
            question: "How does GC work?".into(),
            report: report.into(),
            files_moved,
            files_total,
        }
    }

    /// Each section must state its own staleness, and the two cases must read
    /// differently. Handing the model a report written against files that have since
    /// moved, without saying so, is worse than not handing it one at all: it is
    /// confident prose about code that no longer exists.
    #[test]
    fn a_prior_report_states_whether_the_tree_moved_under_it() {
        let (fresh, truncated) = format_prior_reports(&[prior(3, "the body", 0, 12)], 10_000);
        assert!(!truncated);
        assert!(fresh.contains("#3"), "the ordinal is how a reader names it");
        assert!(
            fresh.contains("all 12 files it read still match"),
            "{fresh}"
        );
        assert!(fresh.contains("the body"));

        let (stale, _) = format_prior_reports(&[prior(4, "the body", 5, 12)], 10_000);
        assert!(stale.contains("5 of the 12 files"), "{stale}");
        assert!(
            stale.contains("CHANGED"),
            "a stale section must say so loudly: {stale}"
        );
    }

    /// A report clipped to fit the budget must **say** it was clipped. A silently
    /// truncated report lets the model read half a conclusion and present it whole —
    /// the same argument the `note` cap makes for announcing what it drops.
    #[test]
    fn an_over_long_prior_report_is_truncated_out_loud() {
        let body = "x".repeat(5_000);
        let (block, truncated) = format_prior_reports(&[prior(1, &body, 0, 1)], 800);
        assert!(truncated, "the caller must be able to count this");
        assert!(block.contains("TRUNCATED"), "{block}");
        assert!(
            block.len() < 1_200,
            "the cap should bound the block: {} chars",
            block.len()
        );
    }

    /// **The provenance invariant.** A `path:start-end` copied out of an earlier
    /// report was never shown to *this* run, so it must be reported as `unverified`
    /// exactly as an invented one would be. Seeding `Evidence` from the injected
    /// block would quietly promote hearsay to verified provenance and destroy the one
    /// guarantee scout's "trust the report" instruction rests on.
    #[tokio::test]
    async fn a_prior_report_never_seeds_the_evidence() {
        let mut params = params(8);
        params.prior_reports = vec![PriorReport {
            id: "earlier".into(),
            seq: 1,
            question: "How does the slicer work?".into(),
            // The earlier report cites a location this run will never open.
            report: "The gap pass lives in src/slicing/traits.rs:100-140.".into(),
            files_moved: 0,
            files_total: 1,
        }];

        // No tool calls at all: the model goes straight to a report citing exactly
        // what it was only *told*.
        let events = run_native(
            NativeOllama::new(
                vec![],
                vec!["The gap pass lives in src/slicing/traits.rs:100-140, as established."],
            ),
            params,
        )
        .await;

        let citations = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations { report, .. } => Some(report.clone()),
                _ => None,
            })
            .expect("the run must emit a citation verdict");
        assert_eq!(
            citations.verified, 0,
            "a citation lifted from a prior report must never count as verified"
        );
        assert!(
            citations
                .unverified_paths
                .iter()
                .any(|p| p == "src/slicing/traits.rs"),
            "the copied path must be named as unverified: {:?}",
            citations.unverified_paths
        );
    }

    /// The mirror of `a_prior_report_never_seeds_the_evidence` for the browse
    /// tool: a report the model *opened itself* is still hearsay, and a citation
    /// copied out of it must land unverified exactly like an invented one.
    #[tokio::test]
    async fn read_research_never_seeds_the_evidence() {
        let events = run_native(
            NativeOllama::new(
                // FakeTools' stored report #2 cites src/slicing/traits.rs:100-140 —
                // a path no other fake ever shows.
                vec![
                    vec![call("read_research", json!({"seq": 2}))],
                    vec![call("finalize", json!({}))],
                ],
                vec!["# Pool return\n\nThe pool return lives in src/slicing/traits.rs:100-140."],
            ),
            params(8),
        )
        .await;

        let citations = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations { report, .. } => Some(report.clone()),
                _ => None,
            })
            .expect("the run must emit a citation verdict");
        assert_eq!(
            citations.verified, 0,
            "a citation copied from a stored report must never count as verified"
        );
        assert!(
            citations
                .unverified_paths
                .iter()
                .any(|p| p == "src/slicing/traits.rs"),
            "the copied path must be named as unverified: {:?}",
            citations.unverified_paths
        );
    }

    /// The reports already injected as context are in the transcript in full;
    /// `list_research` repeating them would invite re-reading what is already
    /// there.
    #[tokio::test]
    async fn list_research_excludes_the_reports_already_in_context() {
        let mut params = params(8);
        // `prior(1, …)` carries id `run-1` — the same id FakeTools lists as seq 1.
        params.prior_reports = vec![prior(1, "# GC sweep ordering\n\nOld prose.", 0, 1)];
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("list_research", json!({}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Listing\n\nNothing further."],
        ));
        let events = run_native_shared(ollama.clone(), params).await;

        let hits = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Step {
                    call: StepCall::ListResearch { .. },
                    hits,
                    ..
                } => Some(*hits),
                _ => None,
            })
            .expect("the list_research step must execute");
        assert_eq!(
            hits, 1,
            "the injected run-1 must be excluded from the listing"
        );
        // The tool reply the model saw must not offer the injected report again.
        let replies = ollama.transcripts.lock().unwrap();
        let listed = replies
            .iter()
            .flatten()
            .filter(|m| m.role == "tool")
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !listed.contains("GC sweep ordering"),
            "an injected report must not be re-offered: {listed}"
        );
        assert!(
            listed.contains("#2"),
            "the other stored report must still be offered: {listed}"
        );
    }

    /// A repeated `read_research` executes nothing and is bounded like every other
    /// duplicate.
    #[tokio::test]
    async fn a_repeated_read_research_is_rejected_as_a_duplicate() {
        let events = run_native(
            NativeOllama::new(
                vec![
                    vec![call("read_research", json!({"seq": 2}))],
                    vec![call("read_research", json!({"seq": 2}))],
                    vec![call("finalize", json!({}))],
                ],
                vec!["# Done\n\nEnough."],
            ),
            params(8),
        )
        .await;
        let read_steps = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ResearchEvent::Step { call, .. }
                        if matches!(call, StepCall::ReadResearch { .. })
                )
            })
            .count();
        assert_eq!(read_steps, 1, "the repeat must execute nothing");
        assert_eq!(done_reason(&events), Some(DoneReason::Finalized));
    }

    /// A structurally broken draft is sent back through the same gate as a
    /// mis-cited one — with the gate closed, so nothing reaches the client — and
    /// the rewrite ships instead. Markdown-only defects reopen no tools.
    #[tokio::test]
    async fn a_markdown_broken_draft_is_sent_back_and_rewritten() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
            ],
            vec![
                // Broken: JSON, no heading. Cites nothing, and is short enough to
                // stay out of the ungrounded gate — the markdown gate alone fires.
                r#"{"finding": "the sweep is safe"}"#,
                "# Fixed\n\nSee src/worker/gc.rs:10-20.",
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
            summaries.iter().all(|t| !t.contains("finding")),
            "the broken draft must never reach the client: {summaries:?}"
        );
        assert!(
            summaries.concat().contains("# Fixed"),
            "the rewrite must ship: {summaries:?}"
        );
        // The complaint must name the concrete defects.
        let asked = ollama.report_prompts.lock().unwrap().join("\n");
        assert!(
            asked.contains("structurally broken"),
            "the model must be told the Markdown is broken: {asked}"
        );
        assert!(
            asked.contains("begins with JSON"),
            "the complaint must name the defect: {asked}"
        );
    }

    /// The other half of the gate: a report that is STILL broken after the repair
    /// pass is streamed (a watched broken report beats a vanished one) but never
    /// journalled — the corpus is what later runs are fed as context.
    #[tokio::test]
    async fn a_run_whose_final_report_stays_broken_is_not_journalled() {
        struct CountingJournal(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl ResearchJournal for CountingJournal {
            async fn record(&self, _: RunRecord) -> Option<RecordedRun> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                None
            }
        }

        let journal = Arc::new(CountingJournal(std::sync::atomic::AtomicUsize::new(0)));
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_research(
            Arc::new(NativeOllama::new(
                vec![vec![call("finalize", json!({}))]],
                // Both the draft and the rewrite are JSON.
                vec![r#"{"a": 1}"#, r#"{"a": 2}"#],
            )),
            Arc::new(FakeTools::default()),
            journal.clone(),
            params(8),
            tx,
            CancellationToken::new(),
        )
        .await;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }

        assert_eq!(
            journal.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a structurally broken report must not be journalled"
        );
        match events.last() {
            Some(ResearchEvent::Done { recorded, .. }) => {
                assert!(recorded.is_none(), "done must carry null run_id/seq");
            }
            other => panic!("expected a done event, got {other:?}"),
        }
    }

    /// The half the gate used to get wrong: a report whose only defect is a missing
    /// heading is repaired and kept, not thrown away. The corpus is the point of the
    /// run, and one line of syntax is not a reason to lose an investigation.
    #[tokio::test]
    async fn a_report_missing_only_its_heading_is_journalled_after_repair() {
        struct CapturingJournal(std::sync::Mutex<Vec<RunRecord>>);
        #[async_trait]
        impl ResearchJournal for CapturingJournal {
            async fn record(&self, r: RunRecord) -> Option<RecordedRun> {
                self.0.lock().unwrap().push(r);
                Some(RecordedRun {
                    id: "run-uuid".into(),
                    seq: 1,
                })
            }
        }

        let journal = Arc::new(CapturingJournal(std::sync::Mutex::new(Vec::new())));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut p = params(8);
        p.question = "How does the GC sweep order its deletes?".into();
        run_research(
            Arc::new(NativeOllama::new(
                vec![vec![call("finalize", json!({}))]],
                // Valid Markdown in every respect but the heading. Only one report is
                // scripted: a repair that instead sent this back would ask the fake for
                // a second and fail the test by starving it.
                vec!["The sweep deletes from SQLite only after Qdrant confirms."],
            )),
            Arc::new(FakeTools::default()),
            journal.clone(),
            p,
            tx,
            CancellationToken::new(),
        )
        .await;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }

        let recorded = journal.0.lock().unwrap();
        let record = match recorded.as_slice() {
            [r] => r,
            other => panic!(
                "the run must be journalled exactly once, got {}",
                other.len()
            ),
        };
        assert!(
            record
                .report
                .starts_with("# How does the GC sweep order its deletes?"),
            "the stored report must carry the derived heading: {:?}",
            record.report
        );
        assert!(
            validate_report_markdown(&record.report).is_empty(),
            "the repaired report must satisfy the gate it was refused by"
        );
        // The model wrote no heading, so no title is stored — the readers fall back to
        // the question, which is what the heading was derived from anyway.
        assert!(
            record.title.is_none(),
            "a server-written heading must not be stored as the model's title: {:?}",
            record.title
        );
        // The draft was never streamed, so the caller must receive the repaired text —
        // what it reads and what the corpus holds are the same bytes.
        let summary = events.iter().find_map(|e| match e {
            ResearchEvent::Summary { text } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(
            summary.as_deref(),
            Some(record.report.as_str()),
            "the streamed report and the stored one must not diverge"
        );
    }

    /// The repair is not a way past the gate: a report broken in any other way is
    /// still refused, heading or no heading.
    #[test]
    fn only_the_missing_heading_is_ever_repaired() {
        let q = "How does GC work?";

        let mut ok = "Prose with no heading.".to_string();
        assert!(repair_missing_heading(&mut ok, q));
        assert_eq!(ok, "# How does GC work?\n\nProse with no heading.");
        // Idempotent: the repaired report has no problem left to key on.
        assert!(!repair_missing_heading(&mut ok, q));

        // JSON where prose was asked for: prepending a heading would produce something
        // that passes the gate and is still unusable as prose.
        let mut json = r#"{"action": "finalize"}"#.to_string();
        assert!(!repair_missing_heading(&mut json, q));
        assert_eq!(json, r#"{"action": "finalize"}"#);

        // An unclosed fence swallows the document in every renderer; a heading above it
        // fixes nothing.
        let mut fence = "Prose.\n\n```rust\nfn f() {}\n".to_string();
        assert!(!repair_missing_heading(&mut fence, q));

        // Nothing to repair, and nothing to invent one from.
        let mut empty = String::new();
        assert!(!repair_missing_heading(&mut empty, q));
        let mut headed = "# Title\n\nProse.".to_string();
        assert!(!repair_missing_heading(&mut headed, q));

        // A question that derives no title still yields a heading, because that is what
        // this function promises.
        let mut no_question = "Prose.".to_string();
        assert!(repair_missing_heading(&mut no_question, "   "));
        assert!(validate_report_markdown(&no_question).is_empty());
    }

    /// The server's own fallback report must pass the gate it is exempt from —
    /// the exemption is a shortcut, not a loophole.
    #[test]
    fn forced_synthesis_passes_the_markdown_gate() {
        let p = params(8);
        let text = forced_synthesis(
            &p,
            &RunState::default(),
            &Evidence::default(),
            DoneReason::TimeExhausted,
            Duration::from_secs(30),
        );
        assert!(
            validate_report_markdown(&text).is_empty(),
            "forced synthesis must be valid Markdown"
        );
    }

    // ── extract_report_title / validate_report_markdown ──────────────────────

    #[test]
    fn the_report_title_is_the_first_heading() {
        assert_eq!(
            extract_report_title("# GC sweep ordering\n\nProse.", "How does GC work?"),
            Some("GC sweep ordering".to_string())
        );
        // A closing ATX sequence is syntax, not title.
        assert_eq!(
            extract_report_title("## Pool return ##\n\nProse.", "q"),
            Some("Pool return".to_string())
        );
        // Leading blank lines are skipped, not a missing heading.
        assert_eq!(
            extract_report_title("\n\n# Title\n\nProse.", "q"),
            Some("Title".to_string())
        );
    }

    #[test]
    fn a_heading_that_repeats_the_question_stores_no_title() {
        assert_eq!(
            extract_report_title("# How  does GC work?\n\nProse.", "how does gc work?"),
            None
        );
    }

    #[test]
    fn a_report_without_a_heading_stores_no_title() {
        assert_eq!(extract_report_title("Plain prose.", "q"), None);
        // `#hashtag` is not a heading (no space), and neither is an empty one.
        assert_eq!(extract_report_title("#hashtag\n\nProse.", "q"), None);
        assert_eq!(extract_report_title("#   \n\nProse.", "q"), None);
        assert_eq!(extract_report_title("", "q"), None);
        // Seven hashes is not ATX.
        assert_eq!(extract_report_title("####### Deep\n\nProse.", "q"), None);
    }

    #[test]
    fn a_clean_report_passes_the_markdown_check() {
        assert!(
            validate_report_markdown("# Title\n\nProse with `code`.\n\n```rust\nfn f() {}\n```\n")
                .is_empty()
        );
    }

    #[test]
    fn a_json_report_is_named_as_broken() {
        let problems = validate_report_markdown(r#"{"action": "finalize"}"#);
        assert!(
            problems.iter().any(|p| p.contains("begins with JSON")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("# heading")),
            "{problems:?}"
        );
        // Empty is its own (single) answer.
        assert_eq!(
            validate_report_markdown("  \n "),
            vec!["The report is empty."]
        );
    }

    #[test]
    fn an_unclosed_fence_is_named() {
        let problems = validate_report_markdown("# T\n\n```rust\nfn f() {}\n");
        assert!(
            problems.iter().any(|p| p.contains("code fence")),
            "{problems:?}"
        );
        assert!(validate_report_markdown("# T\n\n```\nx\n```\n").is_empty());
    }

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

        async fn file_history(
            &self,
            path: String,
            _scope: &ToolScope,
            _token: &CancellationToken,
        ) -> Result<FileHistoryResponse, ApiError> {
            Ok(FileHistoryResponse {
                path,
                history_indexed: true,
                in_scope: true,
                path_indexed: true,
                commits: vec![crate::backend::v0::models::CommitSummary {
                    sha: "a".repeat(40),
                    short_sha: "aaaaaaaa".into(),
                    authored_at: 1,
                    author_name: "T".into(),
                    subject: "gc: delete only confirmed rows".into(),
                    body: "Deleting the SQLite row first would orphan the vector.".into(),
                    change_type: ChangeType::Modified,
                    old_path: None,
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
                // A hit, so the coverage probe never ran — the real core reads it
                // only on a miss.
                searched_chunks: None,
                searched_files: None,
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

        /// One stored report, seq 1, whose id `run-1` matches the `prior()`
        /// helper — so a test can assert the already-in-context exclusion.
        async fn list_research(
            &self,
            _query: Option<String>,
            _token: &CancellationToken,
        ) -> Result<Vec<ResearchListing>, ApiError> {
            Ok(vec![
                ResearchListing {
                    id: "run-1".into(),
                    seq: 1,
                    title: Some("GC sweep ordering".into()),
                    question: "How does GC sweep?".into(),
                    created_at: 1,
                },
                ResearchListing {
                    id: "stored-2".into(),
                    seq: 2,
                    title: None,
                    question: "Where is the connection pool returned?".into(),
                    created_at: 2,
                },
            ])
        }

        /// Seq 2 cites a path no other fake ever shows, so the hearsay test can
        /// assert a citation copied from it stays unverified.
        async fn read_research(
            &self,
            seq: i64,
            _token: &CancellationToken,
        ) -> Result<StoredReport, ApiError> {
            match seq {
                2 => Ok(StoredReport::Found {
                    seq,
                    question: "Where is the connection pool returned?".into(),
                    report: "# Pool return\n\nSee src/slicing/traits.rs:100-140.".into(),
                }),
                3 => Ok(StoredReport::Invalid { seq }),
                _ => Ok(StoredReport::Missing { seq }),
            }
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
                // Off by default: an unrelated test must not have its report turn
                // reshaped by a length clause. The tests that are about the budget
                // set it.
                max_report_words: 0,
            },
            sampling: Sampling::default(),
            // Generous too: a test about the investigation must not be ended by the
            // report window, and the tests that *are* about the window set it.
            report_timeout_ms: 3_600_000,
            // Off unless a test is about the guard: the fakes emit a short thinking
            // delta every turn, and a guard armed by default would be a tripwire on
            // every one of these runs rather than a thing under test.
            max_turn_thinking_chars: 0,
            prior_reports: Vec::new(),
            max_context_chars: 24_000,
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
            // Cited, because this test is about the loop's event cadence and an
            // uncited report from a finalized run now (correctly) takes the
            // citation-repair path, adding a turn that has nothing to do with what
            // is being asserted here.
            vec!["# Report\n\nGC works by sweeping — src/worker/gc.rs:10-40."],
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
                // The report cites a location the run was shown, so the server
                // reads that code out of the index and ships it — between
                // `citations` and `done`, which is where the contract puts it.
                "excerpts",
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
        // Then the code behind those citations, read from the index rather than
        // written by the model — the ordering the wire contract fixes.
        assert!(matches!(&events[13], ResearchEvent::Excerpts { .. }));
        assert!(
            matches!(
                &events[14],
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
                "file_history",
                "finalize",
                "grep",
                "list_files",
                "list_research",
                "note",
                "outline",
                "read_chunks",
                "read_research",
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
                "file_history" => json!({"path": "x"}),
                "note" => json!({"text": "x"}),
                "revise_plan" => json!({"plan": "x"}),
                "read_research" => json!({"seq": 1}),
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
        /// `(offered tools, options)` for every turn, in order. The report turn is
        /// the only one production arms `num_predict` on, and "arms it there" and
        /// "arms it nowhere else" are equally load-bearing: the second is what keeps
        /// every other request byte-for-byte what it was.
        samplings: Mutex<Vec<(bool, Sampling)>>,
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
                samplings: Mutex::new(Vec::new()),
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
            self.samplings
                .lock()
                .unwrap()
                .push((!tools.is_empty(), _sampling));
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
                    ..
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
                    ..
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
            // Cited: this test counts re-entries into the *tool loop*, and the
            // citation-repair phase re-opens tools too. An uncited report from a
            // finalized run would consume the next scripted call and read as the
            // second re-entry this asserts cannot happen.
            vec!["# Report\n\nsrc/worker/gc.rs:10-40"],
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

    // ── the report's length ceiling ──────────────────────────────────────────

    /// A run whose report is bounded says so, in words, and derives the per-section
    /// allowance from the plan it is being held to. Output volume — not retrieval —
    /// is where runs were measured to fail, and until this existed nothing in the
    /// whole prompt mentioned length.
    #[tokio::test]
    async fn the_report_turn_announces_its_word_budget() {
        let ollama = Arc::new(NativeOllama::new(
            vec![vec![call("finalize", json!({}))]],
            vec!["# Report\n\nsrc/worker/gc.rs:10-40"],
        ));
        let mut p = params(8);
        p.budget.max_report_words = 900;
        run_native_shared(ollama.clone(), p).await;

        let prompt = ollama.report_prompts.lock().unwrap()[0].clone();
        assert!(prompt.contains("under 900 words"), "{prompt}");
        // A ceiling, never a target: a target makes a model write *to* the number,
        // and the whole finding is that shorter answers survive.
        assert!(!prompt.contains("about 900 words"), "{prompt}");
        // `FAKE_PLAN` numbers two sub-questions, so the allowance is 900/2 — the
        // denominator is the plan the run is actually being held to, not a constant.
        assert!(prompt.contains("roughly 450 per section"), "{prompt}");
    }

    /// `0` is the off switch, and it has to be a real one: an unmeasured number
    /// shipped without a way back is a number nobody can sweep.
    #[tokio::test]
    async fn a_zero_word_budget_announces_no_length() {
        let ollama = Arc::new(NativeOllama::new(
            vec![vec![call("finalize", json!({}))]],
            vec!["# Report\n\nsrc/worker/gc.rs:10-40"],
        ));
        let mut p = params(8);
        p.budget.max_report_words = 0;
        run_native_shared(ollama.clone(), p).await;

        let prompt = ollama.report_prompts.lock().unwrap()[0].clone();
        assert!(
            !prompt.contains("words"),
            "no length is announced: {prompt}"
        );
    }

    /// The report turn is the only one that bounds generation, and "only" is as
    /// load-bearing as "does": an unset `num_predict` is omitted from the request
    /// entirely, so every other turn stays byte-for-byte what it was.
    #[tokio::test]
    async fn the_report_turn_asks_for_the_report_it_can_afford_to_generate() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
            ],
            vec!["# Report\n\nsrc/worker/gc.rs:10-40"],
        ));
        let mut p = params(8);
        p.budget.max_report_words = 900;
        run_native_shared(ollama.clone(), p).await;

        let samplings = ollama.samplings.lock().unwrap().clone();
        // Every turn that was offered tools is an investigation turn.
        for (had_tools, s) in samplings.iter().filter(|(t, _)| *t) {
            assert!(
                s.num_predict.is_none(),
                "an investigation turn must be unbounded ({had_tools}): {s:?}"
            );
        }
        let bounded: Vec<u64> = samplings
            .iter()
            .filter_map(|(_, s)| s.num_predict)
            .collect();
        assert_eq!(
            bounded,
            vec![900 * crate::config::REPORT_WORDS_TO_TOKENS],
            "exactly one turn — the report — is bounded: {samplings:?}"
        );
    }

    // ── the verbatim excerpt channel ─────────────────────────────────────────

    fn excerpts_of(events: &[ResearchEvent]) -> Option<(Vec<ReportExcerpt>, usize, bool)> {
        events.iter().find_map(|e| match e {
            ResearchEvent::Excerpts {
                excerpts,
                total,
                truncated,
            } => Some((excerpts.clone(), *total, *truncated)),
            _ => None,
        })
    }

    /// The channel exists so a caller never has to make the *model* retype a file —
    /// the failure the whole generation is about. It ships only what the provenance
    /// check verified: a `path_only` or `unverified` citation names no location
    /// worth reading, and attaching real bytes to one would dress up a claim that
    /// was just refused.
    #[tokio::test]
    async fn excerpts_carry_only_verified_citations() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
            ],
            vec![
                // One verified location, one path no tool ever returned. The second
                // is what the draft is sent back for; the rewrite drops it.
                "# Report\n\nsrc/worker/gc.rs:10-40 and src/invented.rs:1-9",
                "# Report\n\nsrc/worker/gc.rs:10-40",
            ],
        ));
        let events = run_native_shared(ollama.clone(), params(8)).await;

        let (excerpts, total, truncated) = excerpts_of(&events).expect("an excerpts event");
        assert_eq!(total, 1);
        assert!(!truncated);
        assert_eq!(excerpts.len(), 1);
        assert_eq!(excerpts[0].path, "src/worker/gc.rs");
        // The code itself, from the index — not a summary of it, and not something
        // the model wrote.
        assert_eq!(excerpts[0].code, "fn collect() {}");
        assert!(
            !excerpts.iter().any(|e| e.path == "src/invented.rs"),
            "an invented path has no code to ship: {excerpts:?}"
        );
    }

    /// No verified citation, no event. The alternative — an empty `excerpts` frame
    /// on every ungrounded run — would make "the channel fired" mean nothing.
    #[tokio::test]
    async fn a_report_with_no_verified_citation_ships_no_excerpts() {
        let ollama = Arc::new(NativeOllama::new(
            vec![vec![call("search", json!({"query": "gc sweep"}))]],
            vec!["# Report\n\nNothing here was settled."],
        ));
        let events = run_native_shared(ollama.clone(), params(1)).await;
        assert!(excerpts_of(&events).is_none(), "{events:?}");
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
                    ..
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
                    ..
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

    /// The short honest refusal keeps shipping as it stands — from a run the budget
    /// cut off. It had no chance to gather more, so "I could not settle this" is an
    /// answer, not an ungrounded claim, and sending it back would only ask for a
    /// fabrication.
    #[tokio::test]
    async fn a_short_uncited_report_from_a_budget_stopped_run_is_not_sent_back() {
        // One step granted, one taken: the loop breaks on the step budget, so
        // `reason` is `BudgetExhausted` and the length exemption applies.
        let ollama = Arc::new(NativeOllama::new(
            vec![vec![call("search", json!({"query": "gc sweep"}))]],
            vec!["# Report\n\nThe evidence I gathered does not settle this."],
        ));
        let events = run_native_shared(ollama.clone(), params(1)).await;

        let (report, revalidation) = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations {
                    report,
                    revalidation,
                    ..
                } => Some((report.clone(), *revalidation)),
                _ => None,
            })
            .expect("a citations event");
        assert_eq!(report.total, 0);
        assert!(
            revalidation.is_none(),
            "a short uncited report from a stopped run is taken at its word: {revalidation:?}"
        );
    }

    /// The same report from a run that *finalized* is a different thing, and the
    /// length exemption must not cover it. Finalizing is the model declaring its own
    /// evidence sufficient; following that with prose that cites none of it is a
    /// self-contradiction, not the short honest version — and exempting it by length
    /// is how a run that read a dozen files ships an ungrounded report under 800
    /// characters and no gate ever sees it.
    #[tokio::test]
    async fn a_finalized_run_that_cites_nothing_is_sent_back_however_short_it_is() {
        let ollama = Arc::new(NativeOllama::new(
            vec![
                vec![call("search", json!({"query": "gc sweep"}))],
                vec![call("finalize", json!({}))],
            ],
            vec![
                "# Report\n\nThe evidence I gathered does not settle this.",
                "# Report\n\nThe sweep is at src/worker/gc.rs:10-40.",
            ],
        ));
        let events = run_native_shared(ollama.clone(), params(8)).await;

        let revalidation = events
            .iter()
            .find_map(|e| match e {
                ResearchEvent::Citations { revalidation, .. } => Some(*revalidation),
                _ => None,
            })
            .expect("a citations event");
        assert!(
            revalidation.is_some(),
            "a finalized run that cites nothing must be sent back: {events:?}"
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
            (StepCall::ListResearch { query: "x".into() }, "query"),
            (StepCall::ReadResearch { seq: "x".into() }, "seq"),
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

    /// A report is arbitrary model prose, so it is arbitrary UTF-8 — and the
    /// backward path walk used to index it by byte. Every case here panicked
    /// ("byte index is not a char boundary") before the walk moved to bytes,
    /// and a panic in `parse_citations` kills the research job: no `done`
    /// event, no journal row, a silently vanished run. This is the regression
    /// guard for that, not a parsing nicety.
    #[test]
    fn a_report_is_arbitrary_utf8_and_must_never_panic_the_parser() {
        // The shape that actually did it in production: `gpt-oss:20b` writes
        // OpenAI-style citation brackets, and `【` abuts the path. The bracket
        // is simply not a path character, so the citation inside it parses —
        // which is the right answer, and was unreachable while it panicked.
        assert_eq!(
            parse_citations("see 【F:src/x.rs:10-20】")
                .iter()
                .map(|c| (c.path.as_str(), c.start, c.end))
                .collect::<Vec<_>>(),
            vec![("src/x.rs", 10, 20)]
        );
        // Cyrillic prose directly before a real citation: the citation still
        // parses, and the multi-byte character is simply where the path ends.
        assert_eq!(
            parse_citations("см. src/research.rs:518-539")
                .iter()
                .map(|c| (c.path.as_str(), c.start, c.end))
                .collect::<Vec<_>>(),
            vec![("src/research.rs", 518, 539)]
        );
        // A multi-byte character abutting a bare range is not a citation, and
        // deciding that must not require slicing into the character.
        for text in [
            "версия 123-456",
            "…:12-30",
            "→:1-2",
            "префикс—src/x.rs:12-30",
            "【4:0-1】",
        ] {
            let _ = parse_citations(text);
        }
        // The last one still finds the path that follows the dash, because a
        // dash is a path character: what matters is that it did not panic.
        assert_eq!(
            parse_citations("префикс—src/x.rs:12-30")
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/x.rs"]
        );
    }

    // ── what an empty grep result means ──────────────────────────────────────

    fn empty_grep(searched_chunks: Option<u64>, searched_files: Option<u64>) -> GrepResponse {
        GrepResponse {
            matches: Vec::new(),
            total: 0,
            out_of_scope: 0,
            searched_chunks,
            searched_files,
        }
    }

    /// "The literal is absent" and "nothing here was searchable" are different
    /// facts, and they used to share one sentence. That is how the same string is
    /// honestly reported absent by one run and found five times by the next: a glob
    /// matching no file, or a scope holding none, read as proof of absence.
    #[test]
    fn a_grep_that_searched_nothing_says_so_rather_than_no_match() {
        let text = format_grep(
            "process_ir.schema.json",
            &empty_grep(Some(0), Some(0)),
            &ToolScope::default(),
        );
        assert!(text.contains("Nothing here was searchable"), "{text}");
        // The refusal has to be explicit, or the model reports it as absence.
        assert!(text.contains("NOT a fact about"), "{text}");
        assert!(
            !text.contains("No indexed chunk contains"),
            "the absence sentence must not appear: {text}"
        );
    }

    /// A genuine miss states how much it looked at. Without the number the reader
    /// cannot tell a thorough miss from a narrow one, which is the whole complaint.
    #[test]
    fn a_grep_miss_reports_how_much_it_searched() {
        let text = format_grep(
            "GcGuard",
            &empty_grep(Some(1421), Some(88)),
            &ToolScope::default(),
        );
        assert!(text.contains("1421 indexed chunk(s)"), "{text}");
        assert!(text.contains("88 file(s)"), "{text}");
        // Still says the useful things it always said.
        assert!(text.contains("case-insensitive"), "{text}");
    }

    /// A server too old to send the counts degrades to the sentence it always sent,
    /// rather than to a claim of having searched nothing.
    #[test]
    fn a_grep_miss_without_coverage_counts_reads_as_it_always_did() {
        let text = format_grep("GcGuard", &empty_grep(None, None), &ToolScope::default());
        assert!(text.contains("No indexed chunk contains"), "{text}");
    }

    // ── the report turn's size guard ─────────────────────────────────────────

    /// The digest is what makes shedding safe: it states the full set of citable
    /// locations independently of the tool results that showed them, so dropping a
    /// result never drops a location's citability. If it under-reports, the model is
    /// told it may not cite something the check will happily verify.
    #[test]
    fn the_evidence_digest_names_every_shown_location() {
        let ev = evidence_of(&[
            ("src/research.rs", Some((500, 560))),
            ("src/research.rs", Some((10, 40))),
            ("README.md", None),
        ]);
        let digest = format_evidence_digest(&ev);
        // Spans sorted, both kept, on the path's one line.
        assert!(
            digest.contains("src/research.rs:10-40, 500-560"),
            "{digest}"
        );
        // A path with no span says so rather than being dropped or given a range.
        assert!(
            digest.contains("README.md — shown, no line range"),
            "{digest}"
        );
        assert!(digest.contains("2 file(s)"), "{digest}");
    }

    fn tool_reply(name: &str, body: &str) -> ChatMessage {
        ChatMessage::tool(name.to_string(), body.to_string())
    }

    /// The anti-regression that matters most in this change: on every run whose
    /// prompt already fits — measured on this host, essentially all of them — the
    /// transcript handed to the report turn must be exactly what it was before the
    /// guard existed.
    #[test]
    fn a_report_prompt_that_fits_is_not_shed() {
        let mut messages = vec![
            ChatMessage::system("you are a writer"),
            ChatMessage::user("Research question:\nhow does GC work?"),
            tool_reply("search", "some evidence"),
        ];
        let before: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        let shed = shed_for_report(&mut messages, None, 100_000);
        assert_eq!(shed, 0);
        let after: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        assert_eq!(after, before, "a fitting prompt must be untouched");
    }

    /// Oldest first, because a run's early turns are orientation the state note has
    /// already summarised while its late reads are what its conclusions rest on.
    #[test]
    fn the_report_turn_sheds_the_oldest_evidence_first() {
        let mut messages = vec![
            ChatMessage::system("s"),
            tool_reply("list_files", &"a".repeat(4000)),
            tool_reply("read_chunks", &"b".repeat(4000)),
        ];
        // ~2000 estimated tokens; room for one of the two bodies, not both. The
        // bodies are far larger than the stub that replaces one, so the arithmetic
        // does not hinge on the stub's wording.
        let shed = shed_for_report(&mut messages, None, 1200);
        assert_eq!(shed, 1, "one is enough, so it stops at one");
        assert!(messages[1].content.starts_with("[Removed by the server"));
        assert!(
            messages[1].content.contains("list_files"),
            "names what went"
        );
        assert_eq!(messages[2].content, "b".repeat(4000), "the late read stays");
    }

    /// The prior-reports block goes before any tool result: it is hearsay by
    /// contract, was never citable, and by the report turn its whole value — telling
    /// the run what names to look for — has already been spent.
    #[test]
    fn shedding_drops_hearsay_before_evidence() {
        let mut messages = vec![
            ChatMessage::system("s"),
            ChatMessage::user("prior report ".repeat(60)),
            tool_reply("search", &"real evidence ".repeat(60)),
        ];
        shed_for_report(&mut messages, Some(1), 100);
        assert!(messages[1].content.contains("hearsay"), "{:?}", messages[1]);
        assert!(
            messages[2].content.contains("Removed by the server"),
            "both go when one is not enough: {:?}",
            messages[2]
        );
    }

    /// A shed reply is **replaced**, never removed. Every announced tool call gets
    /// exactly one `role: "tool"` reply, in order — an assistant turn announcing
    /// three calls followed by two replies is a malformed transcript, and some
    /// templates fail on it outright rather than degrading.
    #[test]
    fn a_shed_transcript_still_answers_every_announced_call() {
        let mut messages = vec![
            ChatMessage::system("s"),
            ChatMessage::assistant_calls(
                "",
                vec![
                    call("search", json!({"query": "gc"})),
                    call("outline", json!({"path": "src/worker/gc.rs"})),
                ],
            ),
            tool_reply("search", &"x".repeat(500)),
            tool_reply("outline", &"y".repeat(500)),
        ];
        let announced: usize = messages
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .map(|c| c.len())
            .sum();
        shed_for_report(&mut messages, None, 1);
        let answered = messages.iter().filter(|m| m.role == "tool").count();
        assert_eq!(announced, answered, "{messages:?}");
    }

    /// The floor: the system prompt, the question and the report request are short
    /// and are what the report's structure and grounding rest on. A ceiling nothing
    /// can satisfy must leave them alone and give up, not strip the turn bare.
    #[test]
    fn shedding_never_touches_the_instructions() {
        let mut messages = vec![
            ChatMessage::system("you are a writer"),
            ChatMessage::user("Research question:\nhow does GC work?"),
            tool_reply("search", "evidence"),
        ];
        shed_for_report(&mut messages, None, 1);
        assert_eq!(messages[0].content, "you are a writer");
        assert!(messages[1].content.starts_with("Research question:"));
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
                verified_locations: Vec::new(),
            },
            revalidation: Some(Revalidation {
                draft_unverified: 4,
                draft_path_only: 2,
                draft_stale: 1,
                steps: 3,
            }),
            server_written: false,
        };
        assert_eq!(ev.name(), "citations");
        let d = ev.data();
        assert_eq!(d["server_written"], false);
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
            server_written: false,
        };
        let d = ev.data();
        assert_eq!(d["draft_unverified"], Value::Null);
        assert_eq!(d["draft_path_only"], Value::Null);
        assert_eq!(d["draft_stale"], Value::Null);
        assert_eq!(d["revalidation_steps"], Value::Null);
    }

    /// A report the *server* wrote scores `total: 0, verified: 0, unverified: 0` —
    /// byte-for-byte what a clean report scores, because `check_citations` runs over
    /// a notice that by construction cites nothing. For its whole life that made the
    /// two indistinguishable in the one field a caller is told to trust, and every
    /// "verified: 0 even though it read the files" report is this. The flag is the
    /// only thing that separates them, so it is a wire contract in its own right.
    #[test]
    fn a_server_written_report_says_so_on_the_wire() {
        let model_written = ResearchEvent::Citations {
            report: CitationReport::default(),
            revalidation: None,
            server_written: false,
        };
        let server_written = ResearchEvent::Citations {
            report: CitationReport::default(),
            revalidation: None,
            server_written: true,
        };
        // Same counts, opposite meanings.
        assert_eq!(model_written.data()["total"], 0);
        assert_eq!(server_written.data()["total"], 0);
        assert_eq!(model_written.data()["server_written"], false);
        assert_eq!(server_written.data()["server_written"], true);
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
            recorded: Some(RecordedRun {
                id: "run-uuid".into(),
                seq: 12,
            }),
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
        // How to ask for this run again. Without it a client that just watched a run
        // stream by cannot offer to reuse it, which is the whole point of storing it.
        assert_eq!(d["run_id"], "run-uuid");
        assert_eq!(d["seq"], 12);
    }

    /// The branch nobody exercises by hand: the journal is best-effort, so a failed
    /// write must still produce a well-formed `done` — with the two identifiers
    /// **present and null** rather than absent, since a consumer that whitelists
    /// fields reads a missing key and a null one the same way only if it is looking
    /// for the key at all.
    #[test]
    fn done_names_no_run_when_the_journal_write_failed() {
        let ev = ResearchEvent::Done {
            progress: progress_fixture(),
            context_fraction: 0.7,
            reason: DoneReason::Finalized,
            recorded: None,
        };
        let d = ev.data();
        assert!(d.get("run_id").is_some(), "the key must still be present");
        assert!(d["run_id"].is_null());
        assert!(d["seq"].is_null());
        // The rest of the record is unaffected: the run happened.
        assert_eq!(d["reason"], "finalized");
        assert_eq!(d["steps"], 3);
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

    fn history_resp(
        history_indexed: bool,
        in_scope: bool,
        path_indexed: bool,
        commits: Vec<crate::backend::v0::models::CommitSummary>,
    ) -> FileHistoryResponse {
        FileHistoryResponse {
            path: "src/worker/gc.rs".into(),
            history_indexed,
            in_scope,
            path_indexed,
            total: commits.len(),
            commits,
        }
    }

    fn summary(
        change_type: ChangeType,
        old_path: Option<&str>,
    ) -> crate::backend::v0::models::CommitSummary {
        crate::backend::v0::models::CommitSummary {
            sha: "a1b2c3d4".repeat(5),
            short_sha: "a1b2c3d4".into(),
            authored_at: 1,
            author_name: "T".into(),
            subject: "gc: delete only confirmed rows".into(),
            body: "Deleting the SQLite row first would orphan the vector forever.".into(),
            change_type,
            old_path: old_path.map(str::to_string),
        }
    }

    /// An empty commit list means three different things and each calls for a
    /// different next move. Collapsed into one "no results" they read as the
    /// least alarming of the three — "this file is uninteresting" — which is the
    /// only one that is never true.
    #[test]
    fn the_three_empty_answers_read_differently() {
        let scope = ToolScope::default();

        // 1. The channel was never reconciled: not a fact about this file.
        let no_channel = format_file_history(
            "src/worker/gc.rs",
            &history_resp(false, true, true, vec![]),
            &scope,
        );
        assert!(
            no_channel.contains("NO indexed git history at all"),
            "{no_channel}"
        );
        assert!(
            no_channel.contains("not a fact about"),
            "must not read as a verdict on the file: {no_channel}"
        );

        // 2. The channel exists; this path simply has no commits in the window.
        let no_commits = format_file_history(
            "src/worker/gc.rs",
            &history_resp(true, true, true, vec![]),
            &scope,
        );
        assert!(
            no_commits.contains("No indexed commit touches"),
            "{no_commits}"
        );
        assert!(
            no_commits.contains("walked window") && no_commits.contains("earlier name"),
            "must name both honest explanations: {no_commits}"
        );

        // 3. Refused, not empty — otherwise the model reads a wall as an absence
        // and spends calls guessing at spellings of a path it may not read.
        let refused = format_file_history(
            "src/worker/gc.rs",
            &history_resp(true, false, false, vec![]),
            &scope,
        );
        assert!(refused.contains("outside this run's scope"), "{refused}");

        // All three must be distinguishable from one another, not just from a hit.
        assert_ne!(no_channel, no_commits);
        assert_ne!(no_commits, refused);
    }

    /// A commit legitimately names a path the code index does not hold — deleted
    /// years ago, excluded by `.mindex`, or in an unsupported language. That is
    /// why `project_commit_paths.path` carries no foreign key, and the result has
    /// to say so or the model will keep trying to read the file.
    #[test]
    fn a_path_with_history_but_no_code_row_says_which_half_is_missing() {
        let out = format_file_history(
            "src/worker/gc.rs",
            &history_resp(true, true, false, vec![summary(ChangeType::Modified, None)]),
            &ToolScope::default(),
        );
        assert!(out.contains("NOT in the code index"), "{out}");
        assert!(out.contains("commits are still real"), "{out}");
    }

    /// A rename is where the tool earns the `old_path` column: without naming it,
    /// a file's history simply stops at the move with no explanation.
    #[test]
    fn a_rename_names_the_old_path() {
        let out = format_file_history(
            "src/worker/gc.rs",
            &history_resp(
                true,
                true,
                true,
                vec![summary(ChangeType::Renamed, Some("src/gc.rs"))],
            ),
            &ToolScope::default(),
        );
        assert!(out.contains("renamed from src/gc.rs"), "{out}");
    }

    /// Every result repeats that a sha is not a citation. The tool description
    /// says it too, but by the time a result is read the description is thousands
    /// of tokens back, and a list of shas reads as provenance unless something
    /// present says otherwise.
    #[test]
    fn every_result_says_a_sha_is_not_a_citation() {
        let out = format_file_history(
            "src/worker/gc.rs",
            &history_resp(true, true, true, vec![summary(ChangeType::Modified, None)]),
            &ToolScope::default(),
        );
        assert!(out.contains("A sha is not a citation"), "{out}");
        assert!(out.contains("path:start-end"), "{out}");
    }

    /// The evidence trap. A commit touched many files; the model was shown only
    /// the one it asked about. Recording the rest as "shown" would quietly
    /// promote a later invented citation from `unverified` to `path_only` — that
    /// is, it would blind the very gate that exists because a model once cited 18
    /// locations it had never seen.
    #[tokio::test]
    async fn file_history_records_only_the_asked_path_as_evidence() {
        let tools = FakeTools::default();
        let executed = match execute(
            &tools,
            &params(8),
            &Action::FileHistory {
                path: "src/worker/gc.rs".into(),
            },
            &CancellationToken::new(),
        )
        .await
        {
            Ok(e) => e,
            Err(_) => panic!("file_history must execute against the fake tools"),
        };

        assert_eq!(
            executed.shown,
            vec![("src/worker/gc.rs".to_string(), None)],
            "only the asked path, and with no span: a commit has no line range"
        );
        assert_eq!(
            executed.call,
            StepCall::FileHistory {
                path: "src/worker/gc.rs".into()
            }
        );
    }
}
