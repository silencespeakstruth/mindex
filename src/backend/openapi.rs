//! OpenAPI document for the mindex HTTP API, served as Swagger UI.
//!
//! The spec is assembled from the `#[utoipa::path]` annotations on each handler in
//! `v0::handlers` and the `ToSchema`/`IntoParams` derives on `v0::models`. Keep this
//! list of `paths`/`schemas` in lock-step with the routes registered in
//! [`crate::backend::http3::run`] — a handler missing here is simply absent from the
//! docs (no compile error), so adding an endpoint means touching both places.
//!
//! Grouping is by `tag` (set per-handler); `TAGS` below fixes their order and gives
//! each a description. The document `version` is stamped at runtime from the crate
//! version in [`api_doc`] so it never drifts from `Cargo.toml`.

use utoipa::OpenApi;
use utoipa::openapi::OpenApi as OpenApiSpec;

use crate::backend::v0::handlers;

/// Tag (group) order and descriptions shown in Swagger UI. The string must match the
/// `tag = "…"` on the corresponding `#[utoipa::path]`.
const INDEXING: &str = "Indexing";
const SEARCH: &str = "Search";
const PROJECTS: &str = "Projects";
const GC: &str = "Garbage Collection";
const OBSERVABILITY: &str = "Observability";
const CONFIG: &str = "Config";

#[derive(OpenApi)]
#[openapi(
    info(
        title = "mindex",
        description = "\
Async RAG indexing + search engine: tree-sitter AST chunking → BGE-M3 multi-vector \
embeddings (dense / sparse / ColBERT) → Qdrant vectors + SQLite metadata.

**Transport & auth.** Internal service. TLS is the only transport security — there is \
**no API authentication**. Do not expose it on an untrusted network.

**Path versioning.** The data-plane endpoints (`/v0/{project_guid}/index`, \
`/v0/{project_guid}/search`) carry a `/v0` version prefix; their request/response \
contracts are stable within `v0`. The management/observability endpoints \
(`/projects/**`, `/gc`, `/status`, `/config`, `/health`, `/version`) are currently \
**unversioned**. The document `version` field tracks the running build.

**Concurrency.** Every operation's description states whether it is concurrency-safe \
and how it interacts with in-flight indexing, GC, and the SQLite pool. Watch for \
**409** (a GC pass is already running) and **499** (client closed the connection — \
nginx convention). A same-file index collision skips the contended file silently \
(absent from the response, like an unchanged file) rather than returning 429.

**Errors.** Every non-2xx response is an RFC 7807 `application/problem+json` body \
(schema `ProblemDetails`) carrying a stable, namespaced machine `code` — the key a \
client localizes against (the `detail`/`title` prose is English and informational). \
Field-specific errors add `field` and a structured `meta` for interpolation. The code \
catalogue: `request.cancelled`, `request.malformed_body`, `request.malformed_path`, \
`internal.error`, `embedder.unavailable`, `qdrant.unavailable`, `gc.already_running`, \
`project.not_found`, `search.no_match`, `selector.empty`, \
`validation.path_invalid`, `validation.sha256_invalid`, `validation.top_k_out_of_range`, \
`validation.query_empty`, `validation.query_too_long`, `validation.code_too_large`, \
`validation.too_many_files`, `validation.selector_too_large`.",
    ),
    paths(
        // Indexing
        handlers::post_index,
        handlers::delete_files,
        handlers::post_cancel,
        handlers::post_retry,
        handlers::post_drift,
        // Search
        handlers::post_search,
        // Projects
        handlers::get_projects,
        handlers::get_project_stats,
        handlers::get_files,
        handlers::delete_project,
        // Garbage Collection
        handlers::post_gc,
        // Observability
        handlers::get_status,
        handlers::get_health,
        handlers::get_version,
        // Config
        handlers::get_config,
    ),
    components(schemas(
        crate::backend::error::ProblemDetails,
        crate::backend::v0::models::ProgrammingLanguage,
        crate::backend::v0::models::Code,
        crate::backend::v0::models::IndexRequest,
        crate::backend::v0::models::IndexResponse,
        crate::backend::v0::models::SearchFilter,
        crate::backend::v0::models::SearchRequest,
        crate::backend::v0::models::SearchResult,
        crate::backend::v0::models::SearchResponse,
        crate::backend::v0::models::DeleteFilesRequest,
        crate::backend::v0::models::DeleteFilesResponse,
        crate::backend::v0::models::CancelRequest,
        crate::backend::v0::models::CancelResponse,
        crate::backend::v0::models::FileStatusCounts,
        crate::backend::v0::models::ChunkCounts,
        crate::backend::v0::models::ProjectStats,
        crate::backend::v0::models::ProjectSummary,
        crate::backend::v0::models::ProjectListResponse,
        crate::backend::v0::models::DriftRequest,
        crate::backend::v0::models::DriftResponse,
        crate::backend::v0::models::GcResponse,
        crate::backend::v0::models::VersionResponse,
        crate::backend::v0::models::HealthChecks,
        crate::backend::v0::models::HealthResponse,
        crate::backend::v0::models::FileInfo,
        crate::backend::v0::models::FileListResponse,
        crate::backend::v0::models::RetryRequest,
        crate::backend::v0::models::RetryResponse,
        crate::backend::v0::models::StatusResponse,
        crate::backend::v0::models::ConfigResponse,
    )),
    tags(
        (name = INDEXING, description = "Index lifecycle: (re)index files, cancel in-flight work, requeue failures, soft-delete files, and detect working-tree drift."),
        (name = SEARCH, description = "Hybrid semantic + lexical code retrieval over a project's active chunks."),
        (name = PROJECTS, description = "Project inventory, per-project stats, per-file listings, and whole-project hard delete."),
        (name = GC, description = "Reclaim soft-deleted chunks/files and prune the status log (globally serialized)."),
        (name = OBSERVABILITY, description = "Liveness, version, and a live runtime/concurrency snapshot for diagnostics."),
        (name = CONFIG, description = "Static server capabilities and tuning knobs, incl. the canonical supported-language list."),
    ),
)]
struct ApiDoc;

/// The assembled OpenAPI spec, with `info.version` stamped from the crate version so it
/// always matches the running build.
pub fn api_doc() -> OpenApiSpec {
    let mut doc = ApiDoc::openapi();
    doc.info.version = env!("CARGO_PKG_VERSION").to_string();
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec assembles, serializes, stamps the crate version, and covers every route
    /// registered in `http3::run`. A new endpoint that isn't added to `paths(...)`
    /// trips the path-count assertion (utoipa would otherwise silently omit it).
    #[test]
    fn openapi_spec_is_complete_and_versioned() {
        let doc = api_doc();
        assert_eq!(doc.info.version, env!("CARGO_PKG_VERSION"));

        let json = serde_json::to_value(&doc).expect("spec must serialize to JSON");
        let paths = json["paths"].as_object().expect("paths object");

        // Every routed path is documented (13 routes; two carry two methods each).
        for p in [
            "/v0/{project_guid}/index",
            "/v0/{project_guid}/search",
            "/projects",
            "/projects/{project_guid}",
            "/projects/{project_guid}/files",
            "/projects/{project_guid}/cancel",
            "/projects/{project_guid}/retry",
            "/projects/{project_guid}/drift",
            "/gc",
            "/status",
            "/config",
            "/health",
            "/version",
        ] {
            assert!(paths.contains_key(p), "missing path in OpenAPI spec: {p}");
        }
        assert_eq!(paths.len(), 13, "unexpected number of documented paths");

        // The two dual-method routes expose both verbs.
        assert!(paths["/projects/{project_guid}"].get("get").is_some());
        assert!(paths["/projects/{project_guid}"].get("delete").is_some());
        assert!(paths["/projects/{project_guid}/files"].get("get").is_some());
        assert!(paths["/projects/{project_guid}/files"].get("delete").is_some());

        // All six tag groups are declared.
        let tags = json["tags"].as_array().expect("tags array");
        let names: Vec<&str> = tags.iter().filter_map(|t| t["name"].as_str()).collect();
        for t in [INDEXING, SEARCH, PROJECTS, GC, OBSERVABILITY, CONFIG] {
            assert!(names.contains(&t), "missing tag group: {t}");
        }
    }

    /// End-to-end check of the served UI, mirroring `http3::run`'s merge: `/swagger-ui`
    /// redirects to `/swagger-ui/`, the UI index serves, the vendored CSS asset serves
    /// with bytes (guards against the `vendored` feature silently not embedding), and
    /// the raw spec is reachable.
    #[tokio::test]
    async fn swagger_ui_and_assets_are_served() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt; // oneshot
        use utoipa_swagger_ui::SwaggerUi;

        let app: Router<()> = Router::new()
            .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", api_doc()));

        let get = |uri: &str| {
            app.clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        };

        let redirect = get("/swagger-ui").await.unwrap();
        assert_eq!(
            redirect.status(),
            StatusCode::SEE_OTHER,
            "/swagger-ui should redirect to /swagger-ui/"
        );

        let index = get("/swagger-ui/").await.unwrap();
        assert_eq!(index.status(), StatusCode::OK, "/swagger-ui/ must serve the UI");

        let css = get("/swagger-ui/swagger-ui.css").await.unwrap();
        assert_eq!(
            css.status(),
            StatusCode::OK,
            "vendored Swagger UI assets must be embedded and served"
        );
        let body = axum::body::to_bytes(css.into_body(), usize::MAX).await.unwrap();
        assert!(!body.is_empty(), "the CSS asset must have content");

        let spec = get("/api-docs/openapi.json").await.unwrap();
        assert_eq!(spec.status(), StatusCode::OK, "raw OpenAPI spec must be reachable");
    }
}
