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

    // ── Selector ──────────────────────────────────────────────────────────────
    /// A management selector (`include`/`exclude`) was empty where non-empty is required. 400.
    SelectorEmpty,

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
    CodeTooLarge { path: String, got: usize, max: usize },
    /// Too many files in one request (index or drift). 400.
    TooManyFiles { got: usize, max: usize },
    /// A selector carried too many globs+languages combined. 400.
    SelectorTooLarge { got: usize, max: usize },
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
            ApiError::SelectorEmpty => "selector.empty",
            ApiError::PathInvalid { .. } => "validation.path_invalid",
            ApiError::Sha256Invalid { .. } => "validation.sha256_invalid",
            ApiError::TopKOutOfRange { .. } => "validation.top_k_out_of_range",
            ApiError::QueryEmpty => "validation.query_empty",
            ApiError::QueryTooLong { .. } => "validation.query_too_long",
            ApiError::CodeTooLarge { .. } => "validation.code_too_large",
            ApiError::TooManyFiles { .. } => "validation.too_many_files",
            ApiError::SelectorTooLarge { .. } => "validation.selector_too_large",
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
            ApiError::FileInFlight => StatusCode::TOO_MANY_REQUESTS,
            ApiError::ProjectNotFound | ApiError::NoMatch => StatusCode::NOT_FOUND,
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
            ApiError::SelectorEmpty => "Empty selector",
            ApiError::PathInvalid { .. } => "Invalid file path",
            ApiError::Sha256Invalid { .. } => "Invalid sha256",
            ApiError::TopKOutOfRange { .. } => "Invalid top_k",
            ApiError::QueryEmpty => "Empty query",
            ApiError::QueryTooLong { .. } => "Query too long",
            ApiError::CodeTooLarge { .. } => "File too large",
            ApiError::TooManyFiles { .. } => "Too many files",
            ApiError::SelectorTooLarge { .. } => "Selector too large",
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
            ApiError::SelectorEmpty | ApiError::SelectorTooLarge { .. } => Some("include/exclude"),
            _ => None,
        }
    }

    /// The default English `detail` (one human-readable sentence).
    fn detail(&self) -> String {
        match self {
            ApiError::Cancelled => "The client closed the connection before the request completed.".into(),
            ApiError::Internal => "An unexpected server error occurred.".into(),
            ApiError::EmbedderUnavailable => "The embedding model server is unreachable or returned an undecodable response.".into(),
            ApiError::QdrantUnavailable => "The vector store is unreachable or the query failed.".into(),
            ApiError::GcRunning => "A garbage-collection pass is already running; retry later.".into(),
            ApiError::FileInFlight => "The same file is already being indexed by another in-flight request; retry.".into(),
            ApiError::ProjectNotFound => "The project has never been seen.".into(),
            ApiError::NoMatch => "No active chunks match (empty project or over-narrow filter).".into(),
            ApiError::MalformedBody(msg) => format!("The request body could not be parsed: {msg}"),
            ApiError::MalformedPath(msg) => format!("A path parameter could not be parsed: {msg}"),
            ApiError::SelectorEmpty => "At least one non-empty `include` or `exclude` selector is required.".into(),
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
        }
    }

    /// Structured interpolation data for the client's localized message (RFC 7807 extension).
    fn meta(&self) -> Option<Value> {
        match self {
            ApiError::PathInvalid { path } | ApiError::Sha256Invalid { path } => {
                Some(json!({ "path": path }))
            }
            ApiError::TopKOutOfRange { got, max } => Some(json!({ "got": got, "min": 1, "max": max })),
            ApiError::QueryTooLong { got, max } => Some(json!({ "got": got, "max": max })),
            ApiError::CodeTooLarge { path, got, max } => {
                Some(json!({ "path": path, "got": got, "max": max }))
            }
            ApiError::TooManyFiles { got, max } | ApiError::SelectorTooLarge { got, max } => {
                Some(json!({ "got": got, "max": max }))
            }
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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = ProblemDetails::from(&self);
        // Json sets `application/json`; RFC 7807 mandates `application/problem+json`,
        // so override the header after building the body response.
        let mut resp = (status, Json(body)).into_response();
        resp.headers_mut().insert(
            CONTENT_TYPE,
            "application/problem+json".parse().expect("static content-type is valid"),
        );
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
            ApiError::SelectorEmpty,
            ApiError::PathInvalid { path: String::new() },
            ApiError::Sha256Invalid { path: String::new() },
            ApiError::TopKOutOfRange { got: 0, max: 0 },
            ApiError::QueryEmpty,
            ApiError::QueryTooLong { got: 0, max: 0 },
            ApiError::CodeTooLarge { path: String::new(), got: 0, max: 0 },
            ApiError::TooManyFiles { got: 0, max: 0 },
            ApiError::SelectorTooLarge { got: 0, max: 0 },
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
            "request.cancelled",
            "request.malformed_body",
            "request.malformed_path",
            "search.no_match",
            "selector.empty",
            "validation.code_too_large",
            "validation.path_invalid",
            "validation.query_empty",
            "validation.query_too_long",
            "validation.selector_too_large",
            "validation.sha256_invalid",
            "validation.too_many_files",
            "validation.top_k_out_of_range",
        ];
        assert_eq!(codes, expected, "error-code contract changed — update intentionally");
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
        assert_eq!(v["type"], "https://mindex/errors/validation.top_k_out_of_range");
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
