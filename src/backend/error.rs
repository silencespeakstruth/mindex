//! The single client-visible error contract: a stable, namespaced error **code**
//! rendered as an RFC 7807 `application/problem+json` body.
//!
//! Every non-2xx response a handler returns flows through [`ApiError`], so a client
//! always receives the same envelope (status, machine-readable `code`, English
//! `title`/`detail`, optional `field`/`meta`) regardless of which layer failed. The
//! `code` is the **localization key**: the server emits English prose, but a client
//! maps `code` → its own catalogue and interpolates `meta`. Codes are an API contract —
//! the [`tests::codes_are_stable`] snapshot makes any rename/removal a deliberate change.
//!
//! Logging stays where the context is (CLAUDE.md convention): call sites log the
//! "what failed" message + `error = ?e` + a sysadmin hint *before* constructing the
//! `ApiError`, so the `From`/constructors here are pure mappings and never double-log.

use axum::Json;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};
use utoipa::ToSchema;

use crate::db::sqlite3::SQLite3PoolError;

/// HTTP 499 (nginx "client closed request"); not in the standard `StatusCode` set.
fn status_499() -> StatusCode {
    StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST)
}

/// Every client-visible error, one variant per kind. The variant determines the
/// `code` / `status` / `title`; dynamic variants additionally carry the data their
/// `detail`/`meta` interpolate. Construct via the variants directly (or the helper
/// constructors / `From` impls below), then return it from a handler — the
/// [`IntoResponse`] impl renders the RFC 7807 body.
#[derive(Debug)]
pub enum ApiError {
    // ── Flow / infrastructure ────────────────────────────────────────────────
    /// The client closed the connection (or the request was cancelled). 499.
    Cancelled,
    /// An unexpected server-side failure (SQLite, slicer, internal invariant). 500.
    Internal,
    /// The embedder is unreachable or returned a response we can't decode. 503.
    EmbedderUnavailable,
    /// Qdrant is unreachable / the query failed. 503.
    QdrantUnavailable,
    /// A GC pass is already running (manual or the hourly worker). 409.
    GcRunning,
    /// The same file is already being indexed by another in-flight request.
    /// Internal sentinel only — `post_index` catches this and skips the file (200);
    /// it is never returned to the client.
    FileInFlight,
    /// The project has never been seen. 404.
    ProjectNotFound,
    /// Search matched no active chunks (empty project or over-narrow filter). 404.
    NoMatch,
    /// The request body could not be deserialized (bad JSON / unknown enum / bad glob).
    /// 400. Carries the deserializer's message as `detail`.
    MalformedBody(String),
    /// A path parameter could not be parsed (e.g. a non-UUID project guid). 400.
    MalformedPath(String),
    /// The request body exceeded the configured size limit. 413.
    BodyTooLarge,

    // ── Selector ──────────────────────────────────────────────────────────────
    /// A destructive endpoint's selector was empty where non-empty is required. 400.
    ///
    /// `field` names what that endpoint's selector is spelled as — `include`/`exclude`
    /// for the file endpoints, `ids` for the research batch delete. One code, because
    /// the rule is one rule (a wipe is asked for, never reached by omission) and a
    /// client maps codes; but the *pointer* has to name a field the request actually
    /// has, or it sends the caller looking for one that does not exist.
    SelectorEmpty { field: &'static str },

    // ── Validation (each carries the data its detail/meta interpolate) ──────────
    /// A repo-relative path violated the path rules (absolute / `..` / backslash / empty). 400.
    PathInvalid { path: String },
    /// A sha256 was not 64 lowercase/uppercase hex chars. 400.
    Sha256Invalid { path: String },
    /// `top_k` was outside `1..=max`. 400.
    TopKOutOfRange { got: u64, max: u64 },
    /// The search query was empty. 400.
    QueryEmpty,
    /// The search query exceeded `max` bytes. 400.
    QueryTooLong { got: usize, max: usize },
    /// A single file's `code` exceeded `max` bytes. 400.
    CodeTooLarge {
        path: String,
        got: usize,
        max: usize,
    },
    /// Too many files in one request (index or drift). 400.
    TooManyFiles { got: usize, max: usize },
    /// A selector carried too many globs+languages combined. 400.
    SelectorTooLarge { got: usize, max: usize },
    /// The symbol `name` was empty. 400.
    SymbolNameEmpty,
    /// The symbol `name` exceeded `max` bytes. 400.
    SymbolNameTooLong { got: usize, max: usize },
    /// The symbols `limit` was outside `1..=max`. 400.
    SymbolLimitOutOfRange { got: usize, max: usize },
    /// Too many commits in one `/history` request. 400.
    TooManyCommits { got: usize, max: usize },
    /// One commit's `subject` + `body` exceeded `max` bytes. 400.
    CommitMessageTooLarge { sha: String, got: usize, max: usize },
    /// A commit sha was not 40 or 64 hex chars, or a commit's shape was
    /// otherwise unusable (empty subject, a rename with no source path). 400.
    CommitInvalid { sha: String, reason: &'static str },
    /// `DELETE /history` named neither `keep_last` nor `older_than`. 400.
    /// The `selector.empty` rule for a resource whose bounds are scalars: a
    /// destructive endpoint must not wipe a channel because a parameter was
    /// forgotten.
    HistoryBoundMissing,
    /// All research slots are taken (`[research].max_concurrent`). 429.
    ResearchBusy,
    /// A research request named no model and `[research].default_model` is unset. 400.
    ResearchModelMissing,
    /// The resolved model matches no `[research].allowed_models` pattern. 400.
    /// A policy refusal, not a shape error — hence the `research.*` namespace,
    /// like its sibling `research.model_missing`.
    ResearchModelNotAllowed { model: String },
    /// A `budget` override axis was outside `1..=[research].max_request_*`. 400.
    /// One code for the spend axes + `evidence_width`; `field`/`meta.field` names
    /// the offender.
    ResearchBudgetOutOfRange {
        field: &'static str,
        got: u64,
        max: u64,
    },
    /// A `budget` shape axis (`max_report_sections` / `max_report_words` /
    /// `checkpoint_every_steps`) was outside its range. 400. Unlike the spend
    /// axes these carry a floor above 1, and two of them accept `0` as the
    /// sanctioned "off" spelling — `zero_ok` is what lets one detail string say
    /// so only where it is true.
    ResearchShapeOutOfRange {
        field: &'static str,
        got: u64,
        min: u64,
        max: u64,
        zero_ok: bool,
    },
    /// A request named more prior runs in `context_run_ids` than
    /// `[research].max_context_runs` allows. 400.
    ResearchContextTooMany { got: usize, max: usize },
    /// `context_run_ids` named a run that is no longer valid — stale itself, or
    /// resting (transitively) on a deleted or stale run. 400. `meta.runs` names
    /// each offender with its reason (`stale` / `context_deleted` /
    /// `context_invalid`), so the client can drop exactly those picks.
    ResearchContextInvalid { runs: Vec<(String, &'static str)> },
    /// A batch delete named more runs than `[limits].max_research_delete_ids`
    /// allows. 400.
    ResearchDeleteTooMany { got: usize, max: usize },
    /// A stored run was named that this project does not have. 404.
    ///
    /// One code for "no such run" and "a run of another project", deliberately: the
    /// distinction is not something the caller can act on, and separating them would
    /// let one project probe another's run ids by their error codes.
    ResearchRunNotFound { run_id: String },
    /// The `limit` on the stored-research list was outside `1..=[research].list_page_limit`. 400.
    ResearchListLimitOutOfRange { got: usize, max: usize },
    /// The run a challenge was aimed at is no longer valid — its files moved, or
    /// its context chain broke — or its scope cannot be reconstructed. 400 rather
    /// than a run: staleness already has its own verdict channel, and letting an
    /// opponent "refute" a report whose code has changed would conflate "the code
    /// moved" with "the report was wrong", poisoning the trust status.
    ChallengeSubjectInvalid {
        run_id: String,
        reason: &'static str,
    },
    /// The run a challenge was aimed at is itself a challenge. 400: trust-status
    /// aggregation is single-level in v1 — to contest a bad challenge, challenge
    /// the original report again (a later valid challenge outweighs) or delete it.
    ChallengeSubjectIsChallenge { run_id: String },
    /// The run's `include`/`exclude` scope admits no indexed file at all. 400 before
    /// the semaphore: every tool is scoped by the same subquery, so such a run can
    /// only refuse every lookup and then report the question unanswerable — which
    /// reads as a finding about the code rather than about the request. Measured
    /// cost of the commonest spelling of it (`"src/"`, where the glob wanted
    /// `"src/**"`): one 302-second run, zero citations, no error anywhere.
    ResearchScopeEmpty { scope: String },
}

impl ApiError {
    /// The stable, namespaced machine code — the localization key. **Changing one is
    /// an API-contract change** (guarded by [`tests::codes_are_stable`]).
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::Cancelled => "request.cancelled",
            ApiError::Internal => "internal.error",
            ApiError::EmbedderUnavailable => "embedder.unavailable",
            ApiError::QdrantUnavailable => "qdrant.unavailable",
            ApiError::GcRunning => "gc.already_running",
            ApiError::FileInFlight => "index.file_in_flight",
            ApiError::ProjectNotFound => "project.not_found",
            ApiError::NoMatch => "search.no_match",
            ApiError::MalformedBody(_) => "request.malformed_body",
            ApiError::MalformedPath(_) => "request.malformed_path",
            ApiError::BodyTooLarge => "request.body_too_large",
            ApiError::SelectorEmpty { .. } => "selector.empty",
            ApiError::PathInvalid { .. } => "validation.path_invalid",
            ApiError::Sha256Invalid { .. } => "validation.sha256_invalid",
            ApiError::TopKOutOfRange { .. } => "validation.top_k_out_of_range",
            ApiError::QueryEmpty => "validation.query_empty",
            ApiError::QueryTooLong { .. } => "validation.query_too_long",
            ApiError::CodeTooLarge { .. } => "validation.code_too_large",
            ApiError::TooManyFiles { .. } => "validation.too_many_files",
            ApiError::SelectorTooLarge { .. } => "validation.selector_too_large",
            ApiError::SymbolNameEmpty => "validation.symbol_name_empty",
            ApiError::SymbolNameTooLong { .. } => "validation.symbol_name_too_long",
            ApiError::SymbolLimitOutOfRange { .. } => "validation.symbol_limit_out_of_range",
            ApiError::TooManyCommits { .. } => "validation.too_many_commits",
            ApiError::CommitMessageTooLarge { .. } => "validation.commit_message_too_large",
            ApiError::CommitInvalid { .. } => "validation.commit_invalid",
            ApiError::HistoryBoundMissing => "validation.history_bound_missing",
            ApiError::ResearchBusy => "research.busy",
            ApiError::ResearchModelMissing => "research.model_missing",
            ApiError::ResearchModelNotAllowed { .. } => "research.model_not_allowed",
            ApiError::ResearchBudgetOutOfRange { .. } => "validation.research_budget_out_of_range",
            ApiError::ResearchShapeOutOfRange { .. } => "validation.research_shape_out_of_range",
            ApiError::ResearchContextTooMany { .. } => "validation.research_context_too_many",
            ApiError::ResearchContextInvalid { .. } => "validation.research_context_invalid",
            ApiError::ResearchDeleteTooMany { .. } => "validation.research_delete_too_many",
            ApiError::ResearchRunNotFound { .. } => "research.run_not_found",
            ApiError::ResearchListLimitOutOfRange { .. } => {
                "validation.research_list_limit_out_of_range"
            }
            ApiError::ChallengeSubjectInvalid { .. } => "research.challenge_subject_invalid",
            ApiError::ChallengeSubjectIsChallenge { .. } => {
                "research.challenge_subject_is_challenge"
            }
            ApiError::ResearchScopeEmpty { .. } => "research.scope_matches_nothing",
        }
    }

    /// The HTTP status carried in both the response line and the `status` body field.
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::Cancelled => status_499(),
            ApiError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::EmbedderUnavailable | ApiError::QdrantUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ApiError::GcRunning => StatusCode::CONFLICT,
            ApiError::FileInFlight | ApiError::ResearchBusy => StatusCode::TOO_MANY_REQUESTS,
            ApiError::ProjectNotFound
            | ApiError::NoMatch
            | ApiError::ResearchRunNotFound { .. } => StatusCode::NOT_FOUND,
            ApiError::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            // Everything else is a client input error.
            _ => StatusCode::BAD_REQUEST,
        }
    }

    /// A short, human-readable English summary (stable per code).
    fn title(&self) -> &'static str {
        match self {
            ApiError::Cancelled => "Request cancelled",
            ApiError::Internal => "Internal server error",
            ApiError::EmbedderUnavailable => "Embedder unavailable",
            ApiError::QdrantUnavailable => "Vector store unavailable",
            ApiError::GcRunning => "Garbage collection already running",
            ApiError::FileInFlight => "File already being indexed",
            ApiError::ProjectNotFound => "Project not found",
            ApiError::NoMatch => "No matching results",
            ApiError::MalformedBody(_) => "Malformed request body",
            ApiError::MalformedPath(_) => "Malformed path parameter",
            ApiError::BodyTooLarge => "Request body too large",
            ApiError::SelectorEmpty { .. } => "Empty selector",
            ApiError::PathInvalid { .. } => "Invalid file path",
            ApiError::Sha256Invalid { .. } => "Invalid sha256",
            ApiError::TopKOutOfRange { .. } => "Invalid top_k",
            ApiError::QueryEmpty => "Empty query",
            ApiError::QueryTooLong { .. } => "Query too long",
            ApiError::CodeTooLarge { .. } => "File too large",
            ApiError::TooManyFiles { .. } => "Too many files",
            ApiError::SelectorTooLarge { .. } => "Selector too large",
            ApiError::SymbolNameEmpty => "Empty symbol name",
            ApiError::SymbolNameTooLong { .. } => "Symbol name too long",
            ApiError::SymbolLimitOutOfRange { .. } => "Invalid symbols limit",
            ApiError::HistoryBoundMissing => "Missing retention bound",
            ApiError::TooManyCommits { .. } => "Too many commits",
            ApiError::CommitMessageTooLarge { .. } => "Commit message too large",
            ApiError::CommitInvalid { .. } => "Invalid commit",
            ApiError::ResearchBusy => "Research capacity exhausted",
            ApiError::ResearchModelMissing => "No research model",
            ApiError::ResearchModelNotAllowed { .. } => "Research model not allowed",
            ApiError::ResearchBudgetOutOfRange { .. } => "Invalid research budget",
            ApiError::ResearchShapeOutOfRange { .. } => "Invalid research report shape",
            ApiError::ResearchContextTooMany { .. } => "Too much prior research",
            ApiError::ResearchContextInvalid { .. } => "Research context run is invalid",
            ApiError::ResearchDeleteTooMany { .. } => "Too many runs in one delete",
            ApiError::ResearchRunNotFound { .. } => "No such research run",
            ApiError::ResearchListLimitOutOfRange { .. } => "Invalid page size",
            ApiError::ChallengeSubjectInvalid { .. } => "Challenge subject is not valid",
            ApiError::ChallengeSubjectIsChallenge { .. } => "Cannot challenge a challenge",
            ApiError::ResearchScopeEmpty { .. } => "Research scope matches no indexed file",
        }
    }

    /// The JSON field the error is about, when it is field-specific (RFC 7807 extension).
    fn field(&self) -> Option<&'static str> {
        match self {
            ApiError::PathInvalid { .. }
            | ApiError::Sha256Invalid { .. }
            | ApiError::CodeTooLarge { .. }
            | ApiError::TooManyFiles { .. } => Some("files"),
            ApiError::TopKOutOfRange { .. } => Some("top_k"),
            ApiError::QueryEmpty | ApiError::QueryTooLong { .. } => Some("query"),
            ApiError::SelectorEmpty { field } => Some(field),
            ApiError::SelectorTooLarge { .. } => Some("include/exclude"),
            ApiError::SymbolNameEmpty | ApiError::SymbolNameTooLong { .. } => Some("name"),
            ApiError::SymbolLimitOutOfRange { .. } => Some("limit"),
            ApiError::TooManyCommits { .. }
            | ApiError::CommitMessageTooLarge { .. }
            | ApiError::CommitInvalid { .. } => Some("commits"),
            ApiError::HistoryBoundMissing => Some("keep_last/older_than"),
            ApiError::ResearchModelMissing | ApiError::ResearchModelNotAllowed { .. } => {
                Some("model")
            }
            ApiError::ResearchBudgetOutOfRange { field, .. }
            | ApiError::ResearchShapeOutOfRange { field, .. } => Some(field),
            ApiError::ResearchContextTooMany { .. } | ApiError::ResearchContextInvalid { .. } => {
                Some("context_run_ids")
            }
            ApiError::ResearchListLimitOutOfRange { .. } => Some("limit"),
            ApiError::ResearchDeleteTooMany { .. } => Some("ids"),
            _ => None,
        }
    }

    /// The default English `detail` (one human-readable sentence).
    /// `pub` for one consumer: the streaming `/index` mode, whose failures happen
    /// after the HTTP status is already 200 and so travel as an SSE `error` event
    /// carrying this same text instead of a problem+json body.
    pub fn detail(&self) -> String {
        match self {
            ApiError::Cancelled => {
                "The client closed the connection before the request completed.".into()
            }
            ApiError::Internal => "An unexpected server error occurred.".into(),
            ApiError::EmbedderUnavailable => {
                "The embedding model server is unreachable or returned an undecodable response."
                    .into()
            }
            ApiError::QdrantUnavailable => {
                "The vector store is unreachable or the query failed.".into()
            }
            ApiError::GcRunning => {
                "A garbage-collection pass is already running; retry later.".into()
            }
            ApiError::FileInFlight => {
                "The same file is already being indexed by another in-flight request; retry.".into()
            }
            ApiError::ProjectNotFound => "The project has never been seen.".into(),
            ApiError::NoMatch => {
                "No active chunks match (empty project or over-narrow filter).".into()
            }
            ApiError::MalformedBody(msg) => format!("The request body could not be parsed: {msg}"),
            ApiError::MalformedPath(msg) => format!("A path parameter could not be parsed: {msg}"),
            ApiError::BodyTooLarge => {
                "The request body exceeds the configured size limit ([server].max_body_mib).".into()
            }
            ApiError::SelectorEmpty { field } => format!(
                "This endpoint refuses an empty selector, so that a destructive request \
                 cannot match everything by omission. Name what it applies to in `{field}`."
            ),
            ApiError::PathInvalid { path } => format!(
                "Path {path:?} is invalid: paths must be non-empty, repo-relative (no leading '/'), \
                 free of '..' traversal, and use '/' (no backslash)."
            ),
            ApiError::Sha256Invalid { path } => {
                format!("The sha256 for path {path:?} must be 64 hexadecimal characters.")
            }
            ApiError::TopKOutOfRange { got, max } => {
                format!("top_k must be between 1 and {max} (got {got}).")
            }
            ApiError::QueryEmpty => "The search query must not be empty.".into(),
            ApiError::QueryTooLong { got, max } => {
                format!("The search query must be at most {max} bytes (got {got}).")
            }
            ApiError::CodeTooLarge { path, got, max } => format!(
                "File {path:?} is {got} bytes, exceeding the per-file limit of {max} bytes."
            ),
            ApiError::TooManyFiles { got, max } => {
                format!("The request carries {got} files, exceeding the limit of {max}.")
            }
            ApiError::SelectorTooLarge { got, max } => format!(
                "A selector carries {got} patterns/languages, exceeding the limit of {max}."
            ),
            ApiError::SymbolNameEmpty => "The symbol name must not be empty.".into(),
            ApiError::SymbolNameTooLong { got, max } => {
                format!("The symbol name must be at most {max} bytes (got {got}).")
            }
            ApiError::SymbolLimitOutOfRange { got, max } => {
                format!("limit must be between 1 and {max} (got {got}).")
            }
            ApiError::TooManyCommits { got, max } => format!(
                "The request carries {got} commits, exceeding the limit of {max}. Narrow the \
                 history window ([limits].max_history_commits raises the cap)."
            ),
            ApiError::CommitMessageTooLarge { sha, got, max } => {
                format!("Commit {sha} has a {got}-byte message, exceeding the limit of {max}.")
            }
            ApiError::CommitInvalid { sha, reason } => {
                format!("Commit {sha} is unusable: {reason}.")
            }
            ApiError::HistoryBoundMissing => {
                "At least one of `keep_last` or `older_than` is required; deleting a project's \
                 whole history must be asked for explicitly (`?keep_last=0`)."
                    .into()
            }
            ApiError::ResearchBusy => {
                // Names where to look rather than only what happened: a refused
                // caller's next question is always "busy with what, and for how
                // long", and until `/research/active` existed there was no answer.
                "All research slots are in use ([research].max_concurrent, published as \
                 research.max_concurrent by GET /config); see GET /research/active for what \
                 holds them and how long it has, then retry."
                    .into()
            }
            ApiError::ResearchModelMissing => {
                "The request names no model and the server has no [research].default_model.".into()
            }
            ApiError::ResearchModelNotAllowed { model } => format!(
                "Model {model:?} is not permitted by [research].allowed_models. GET /config \
                 lists the allowed models in research.models and the patterns in \
                 research.allowed_models."
            ),
            ApiError::ResearchBudgetOutOfRange { field, got, max } => format!(
                "budget.{field} must be between 1 and {max} (got {got}); the ceiling is \
                 [research].{}.",
                // `max_seconds` is capped by `max_request_seconds` — the axis name
                // without its own `max_` prefix. `evidence_width` keeps its whole
                // name and a different prefix: `[research].max_evidence_width`.
                match *field {
                    "evidence_width" => "max_evidence_width".to_string(),
                    other => format!("max_request_{}", other.trim_start_matches("max_")),
                }
            ),
            ApiError::ResearchShapeOutOfRange {
                field,
                got,
                min,
                max,
                zero_ok,
            } => {
                let off = if *zero_ok { ", or 0 to disable it" } else { "" };
                format!(
                    "budget.{field} must be between {min} and {max}{off} (got {got}); the \
                     ceiling is [research].{}.",
                    // `checkpoint_every_steps` shares the step budget's ceiling — an
                    // interval above `max_steps` is `0` spelled differently.
                    match *field {
                        "checkpoint_every_steps" => "max_request_steps",
                        "max_report_sections" => "max_request_report_sections",
                        _ => "max_request_report_words",
                    }
                )
            }
            ApiError::ResearchContextTooMany { got, max } => format!(
                "context_run_ids names {got} earlier runs, but at most {max} may be given \
                 ([research].max_context_runs). Each one is resent on every turn, so the cap is \
                 a token budget rather than a formality."
            ),
            ApiError::ResearchDeleteTooMany { got, max } => format!(
                "The request names {got} runs to delete, but at most {max} may be given in one \
                 call ([limits].max_research_delete_ids). Delete them in batches."
            ),
            ApiError::ResearchContextInvalid { runs } => format!(
                "{} of the named context runs are no longer valid — stale, or resting on a \
                 deleted or stale run; injecting them would feed the new run obsolete prose. \
                 Pick valid runs from GET /projects/{{guid}}/research?valid=true.",
                runs.len()
            ),
            ApiError::ResearchRunNotFound { run_id } => {
                format!("This project has no research run {run_id}.")
            }
            ApiError::ResearchListLimitOutOfRange { got, max } => format!(
                "limit must be between 1 and {max} (got {got}); the ceiling is \
                 [research].list_page_limit."
            ),
            ApiError::ChallengeSubjectInvalid { run_id, reason } => format!(
                "Research run {run_id} cannot be challenged ({reason}). A challenge scores \
                 claims against the code as indexed now, so a subject whose evidence has \
                 already moved would conflate \"the code changed\" with \"the report was \
                 wrong\". Re-run the research first, then challenge the fresh run."
            ),
            ApiError::ChallengeSubjectIsChallenge { run_id } => format!(
                "Research run {run_id} is itself a challenge, and challenges cannot be \
                 challenged. To contest it, challenge the original report again — a later \
                 valid challenge outweighs — or delete it."
            ),
            ApiError::ResearchScopeEmpty { scope } => format!(
                "The requested scope ({scope}) matches no indexed file in this project, so \
                 every tool the run could call would refuse. Globs are root-relative with \
                 forward slashes and `*` stops at `/`, so a directory needs `src/**` — \
                 neither `src/` nor `src` matches anything. GET /projects/{{guid}}/files \
                 lists what is indexed."
            ),
        }
    }

    /// Structured interpolation data for the client's localized message (RFC 7807 extension).
    fn meta(&self) -> Option<Value> {
        match self {
            ApiError::PathInvalid { path } | ApiError::Sha256Invalid { path } => {
                Some(json!({ "path": path }))
            }
            ApiError::TopKOutOfRange { got, max } => {
                Some(json!({ "got": got, "min": 1, "max": max }))
            }
            ApiError::QueryTooLong { got, max } => Some(json!({ "got": got, "max": max })),
            ApiError::CodeTooLarge { path, got, max } => {
                Some(json!({ "path": path, "got": got, "max": max }))
            }
            ApiError::TooManyFiles { got, max }
            | ApiError::SelectorTooLarge { got, max }
            | ApiError::SymbolNameTooLong { got, max } => Some(json!({ "got": got, "max": max })),
            ApiError::SymbolLimitOutOfRange { got, max } => {
                Some(json!({ "got": got, "min": 1, "max": max }))
            }
            ApiError::TooManyCommits { got, max } => Some(json!({ "got": got, "max": max })),
            ApiError::CommitMessageTooLarge { sha, got, max } => {
                Some(json!({ "sha": sha, "got": got, "max": max }))
            }
            ApiError::CommitInvalid { sha, reason } => {
                Some(json!({ "sha": sha, "reason": reason }))
            }
            ApiError::ResearchBudgetOutOfRange { field, got, max } => {
                Some(json!({ "field": field, "got": got, "min": 1, "max": max }))
            }
            ApiError::ResearchShapeOutOfRange {
                field,
                got,
                min,
                max,
                zero_ok,
            } => Some(json!({
                "field": field, "got": got, "min": min, "max": max, "zero_ok": zero_ok
            })),
            ApiError::ResearchContextTooMany { got, max }
            | ApiError::ResearchDeleteTooMany { got, max } => {
                Some(json!({ "got": got, "max": max }))
            }
            ApiError::ResearchContextInvalid { runs } => Some(json!({
                "runs": runs
                    .iter()
                    .map(|(id, reason)| json!({ "id": id, "reason": reason }))
                    .collect::<Vec<_>>()
            })),
            ApiError::ResearchModelNotAllowed { model } => Some(json!({ "model": model })),
            ApiError::ResearchRunNotFound { run_id } => Some(json!({ "run_id": run_id })),
            ApiError::ResearchListLimitOutOfRange { got, max } => {
                Some(json!({ "got": got, "min": 1, "max": max }))
            }
            ApiError::ChallengeSubjectInvalid { run_id, reason } => {
                Some(json!({ "run_id": run_id, "reason": reason }))
            }
            ApiError::ChallengeSubjectIsChallenge { run_id } => Some(json!({ "run_id": run_id })),
            ApiError::ResearchScopeEmpty { scope } => Some(json!({ "scope": scope })),
            _ => None,
        }
    }
}

/// The RFC 7807 problem-details body (`application/problem+json`). `type` is a
/// dereferenceable-looking URI derived from the `code`; `code`/`field`/`meta` are
/// extension members. Serialized for the wire and documented in OpenAPI.
#[derive(Serialize, Debug, ToSchema)]
pub struct ProblemDetails {
    /// A URI reference identifying the problem type, derived from `code`.
    #[schema(example = "https://mindex/errors/validation.top_k_out_of_range")]
    pub r#type: String,
    /// Short, human-readable summary (stable per `code`).
    pub title: String,
    /// HTTP status code, duplicated in the body per RFC 7807.
    pub status: u16,
    /// Human-readable explanation specific to this occurrence (English; localize via `code`).
    pub detail: String,
    /// The stable, namespaced machine code — the localization key.
    #[schema(example = "validation.top_k_out_of_range")]
    pub code: String,
    /// The offending request field, when the error is field-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Structured interpolation data (e.g. `{min, max, got}`), when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl From<&ApiError> for ProblemDetails {
    fn from(e: &ApiError) -> Self {
        let code = e.code();
        ProblemDetails {
            r#type: format!("https://mindex/errors/{code}"),
            title: e.title().to_string(),
            status: e.status().as_u16(),
            detail: e.detail(),
            code: code.to_string(),
            field: e.field().map(str::to_string),
            meta: e.meta(),
        }
    }
}

/// The failed request's stable machine `code`, carried in the response
/// extensions so the metrics middleware can label by it.
///
/// The alternative is parsing the problem+json body back out in middleware,
/// which means buffering a body that was just serialized. This costs nothing:
/// [`ApiError::code`] is already `&'static str`, and this impl is the only place
/// an error response is built, so every non-2xx carries it.
#[derive(Clone, Copy, Debug)]
pub struct ErrorCode(pub &'static str);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let code = self.code();
        let body = ProblemDetails::from(&self);
        // Json sets `application/json`; RFC 7807 mandates `application/problem+json`,
        // so override the header after building the body response.
        let mut resp = (status, Json(body)).into_response();
        resp.headers_mut().insert(
            CONTENT_TYPE,
            "application/problem+json"
                .parse()
                .expect("static content-type is valid"),
        );
        resp.extensions_mut().insert(ErrorCode(code));
        resp
    }
}

// ── Conversions from domain errors (pure mappings — call sites do the logging) ──

impl From<SQLite3PoolError> for ApiError {
    fn from(e: SQLite3PoolError) -> Self {
        match e {
            SQLite3PoolError::Cancelled => ApiError::Cancelled,
            // `HTTPStatusCode` is only ever set to 500 by the slicer error mapping;
            // preserve that as an internal error.
            _ => ApiError::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// Every variant's `code`, in sorted order. This is the public error-code
    /// contract: a failing assertion means a code was renamed, added, or removed —
    /// update intentionally (and any client catalogue / docs) rather than silently.
    #[test]
    fn codes_are_stable() {
        let all = [
            ApiError::Cancelled,
            ApiError::Internal,
            ApiError::EmbedderUnavailable,
            ApiError::QdrantUnavailable,
            ApiError::GcRunning,
            ApiError::FileInFlight,
            ApiError::ProjectNotFound,
            ApiError::NoMatch,
            ApiError::MalformedBody(String::new()),
            ApiError::MalformedPath(String::new()),
            ApiError::BodyTooLarge,
            ApiError::SelectorEmpty { field: "" },
            ApiError::PathInvalid {
                path: String::new(),
            },
            ApiError::Sha256Invalid {
                path: String::new(),
            },
            ApiError::TopKOutOfRange { got: 0, max: 0 },
            ApiError::QueryEmpty,
            ApiError::QueryTooLong { got: 0, max: 0 },
            ApiError::CodeTooLarge {
                path: String::new(),
                got: 0,
                max: 0,
            },
            ApiError::TooManyFiles { got: 0, max: 0 },
            ApiError::SelectorTooLarge { got: 0, max: 0 },
            ApiError::SymbolNameEmpty,
            ApiError::SymbolNameTooLong { got: 0, max: 0 },
            ApiError::SymbolLimitOutOfRange { got: 0, max: 0 },
            ApiError::TooManyCommits { got: 0, max: 0 },
            ApiError::CommitMessageTooLarge {
                sha: String::new(),
                got: 0,
                max: 0,
            },
            ApiError::CommitInvalid {
                sha: String::new(),
                reason: "",
            },
            ApiError::HistoryBoundMissing,
            ApiError::ResearchBusy,
            ApiError::ResearchModelMissing,
            ApiError::ResearchModelNotAllowed {
                model: String::new(),
            },
            ApiError::ResearchBudgetOutOfRange {
                field: "max_seconds",
                got: 0,
                max: 0,
            },
            ApiError::ResearchShapeOutOfRange {
                field: "max_report_sections",
                got: 0,
                min: 0,
                max: 0,
                zero_ok: false,
            },
            ApiError::ResearchContextTooMany { got: 0, max: 0 },
            ApiError::ResearchContextInvalid { runs: Vec::new() },
            ApiError::ResearchDeleteTooMany { got: 0, max: 0 },
            ApiError::ResearchRunNotFound {
                run_id: String::new(),
            },
            ApiError::ResearchListLimitOutOfRange { got: 0, max: 0 },
            ApiError::ChallengeSubjectInvalid {
                run_id: String::new(),
                reason: "",
            },
            ApiError::ChallengeSubjectIsChallenge {
                run_id: String::new(),
            },
            ApiError::ResearchScopeEmpty {
                scope: String::new(),
            },
        ];
        let mut codes: Vec<&str> = all.iter().map(ApiError::code).collect();
        codes.sort_unstable();

        let expected = [
            "embedder.unavailable",
            "gc.already_running",
            "index.file_in_flight",
            "internal.error",
            "project.not_found",
            "qdrant.unavailable",
            "request.body_too_large",
            "request.cancelled",
            "request.malformed_body",
            "request.malformed_path",
            "research.busy",
            "research.challenge_subject_invalid",
            "research.challenge_subject_is_challenge",
            "research.model_missing",
            "research.model_not_allowed",
            "research.run_not_found",
            "research.scope_matches_nothing",
            "search.no_match",
            "selector.empty",
            "validation.code_too_large",
            "validation.commit_invalid",
            "validation.commit_message_too_large",
            "validation.history_bound_missing",
            "validation.path_invalid",
            "validation.query_empty",
            "validation.query_too_long",
            "validation.research_budget_out_of_range",
            "validation.research_context_invalid",
            "validation.research_context_too_many",
            "validation.research_delete_too_many",
            "validation.research_list_limit_out_of_range",
            "validation.research_shape_out_of_range",
            "validation.selector_too_large",
            "validation.sha256_invalid",
            "validation.symbol_limit_out_of_range",
            "validation.symbol_name_empty",
            "validation.symbol_name_too_long",
            "validation.too_many_commits",
            "validation.too_many_files",
            "validation.top_k_out_of_range",
        ];
        assert_eq!(
            codes, expected,
            "error-code contract changed — update intentionally"
        );
    }

    /// The metrics middleware labels by this extension rather than by re-parsing
    /// the body, so it has to survive every change to `IntoResponse`.
    #[tokio::test]
    async fn the_error_response_carries_its_code_in_the_extensions() {
        let resp = ApiError::NoMatch.into_response();
        let code = resp
            .extensions()
            .get::<ErrorCode>()
            .expect("every error response carries its code");
        assert_eq!(code.0, "search.no_match");
    }

    #[tokio::test]
    async fn renders_rfc7807_envelope() {
        let resp = ApiError::TopKOutOfRange { got: 999, max: 100 }.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "application/problem+json",
        );

        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], "validation.top_k_out_of_range");
        assert_eq!(v["status"], 400);
        assert_eq!(v["field"], "top_k");
        assert_eq!(v["meta"]["max"], 100);
        assert_eq!(v["meta"]["got"], 999);
        assert_eq!(
            v["type"],
            "https://mindex/errors/validation.top_k_out_of_range"
        );
    }

    #[test]
    fn cancelled_is_499_and_optional_fields_omitted() {
        let pd = ProblemDetails::from(&ApiError::Cancelled);
        assert_eq!(pd.status, 499);
        assert!(pd.field.is_none());
        assert!(pd.meta.is_none());
        // Optional fields are skipped on the wire when absent.
        let v = serde_json::to_value(&pd).unwrap();
        assert!(v.get("field").is_none());
        assert!(v.get("meta").is_none());
    }
}
