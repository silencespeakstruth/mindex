//! Extractor wrappers that render axum's deserialization rejections through the
//! [`ApiError`](crate::backend::error::ApiError) RFC 7807 envelope instead of axum's
//! default plain-text 400. A malformed JSON body, an unknown enum/glob, a non-UUID
//! path, or a bad query string therefore carries a stable `code` like any other error.
//!
//! Handlers use `ApiJson<T>` / `ApiPath<T>` / `ApiQuery<T>` in place of the bare axum
//! extractors; they deref-destructure identically (`ApiPath(x): ApiPath<T>`). The
//! `#[utoipa::path]` annotations are hand-written, so swapping the extractor type does
//! not affect the generated OpenAPI spec.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::request::Parts;
use axum::{Json, RequestPartsExt};
use serde::de::DeserializeOwned;

use crate::backend::error::ApiError;

/// `Json<T>` whose deserialization rejection becomes [`ApiError::MalformedBody`].
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned + 'static,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rej) => Err(malformed_body(rej)),
        }
    }
}

/// `Path<T>` whose parse rejection becomes [`ApiError::MalformedPath`].
pub struct ApiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match parts.extract::<Path<T>>().await {
            Ok(Path(value)) => Ok(ApiPath(value)),
            Err(rej) => Err(malformed_path(rej)),
        }
    }
}

/// `Query<T>` whose deserialization rejection becomes [`ApiError::MalformedBody`]
/// (a query string is request input like a body, so it reuses that code).
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match parts.extract::<Query<T>>().await {
            Ok(Query(value)) => Ok(ApiQuery(value)),
            Err(rej) => Err(malformed_query(rej)),
        }
    }
}

// ─── Authorization ───────────────────────────────────────────────────────────
//
// Authorization is carried by **types**, and that is the design decision rather
// than a style. Three shapes were available and two lose:
//
// A tower layer would be the obvious one — `record_request` already reads
// `project_guid` out of `RawPathParams` at exactly that position. It loses
// because its answer has to be uniform and two routes need different ones:
// `POST /drift` must *not* 404 an unknown project (its whole contract is that
// every posted file comes back `missing`), and `POST /index` must be able to
// create one. A layer therefore needs a per-route exception table — the
// hand-kept fifth copy of the route list that `UNDOCUMENTED_ROUTES`' own comment
// warns is "the one nothing checks". It is also blind to the seven routes that
// carry no `{project_guid}` at all, which is the half a reviewer forgets.
//
// A helper on `RouterState` loses for the reason this codebase already learned
// with `set_file_status`: an opt-in call that is silent when omitted is a bug
// waiting for its first careless commit, which is why that function had to
// become `#[must_use]` after the fact.
//
// A type is what a source-text guard can see. `every_scoped_handler_takes_a_scope_extractor`
// reads the route table out of `http3.rs` and asserts that each project-keyed
// handler names one of these — so forgetting one fails the suite instead of
// shipping an open endpoint.

use crate::backend::auth::{Action, AuthError, Claims, bearer_from_header, verify};
use crate::backend::http3::RouterState;
use uuid::Uuid;

/// What the caller proved about itself. `None` when `[auth].enabled` is false,
/// which is what makes every extractor below the identity function on a
/// deployment that has not opted in.
#[derive(Debug, Clone)]
pub struct Authorization(pub Option<Claims>);

impl Authorization {
    /// Whether this caller may see `guid` at all.
    pub fn covers(&self, guid: &Uuid) -> bool {
        self.0.as_ref().is_none_or(|c| c.covers(guid))
    }

    /// The project GUIDs this caller may see, or `None` for "all of them" —
    /// which covers both an unrestricted token and authorization being off.
    /// Read by `GET /projects` and `GET /research/active`, the two listings a
    /// path-shaped check cannot reach.
    pub fn visible_projects(&self) -> Option<&[String]> {
        match &self.0 {
            None => None,
            Some(c) if c.is_wildcard() => None,
            Some(c) => Some(&c.prj),
        }
    }

    /// [`Self::covers`] for a GUID that is already a string.
    ///
    /// Used where the project is carried as text rather than parsed — the live
    /// research registry, whose entries hold what the run was launched with. An
    /// unparseable value is **not** covered: it cannot be matched against a
    /// claim, and reading "I could not tell" as "yes" is the one direction that
    /// fails open.
    pub fn covers_guid_str(&self, guid: &str) -> bool {
        match Uuid::parse_str(guid) {
            Ok(u) => self.covers(&u),
            Err(_) => self.0.is_none(),
        }
    }
}

/// Verifies the bearer token, if this deployment requires one.
///
/// The reason a refusal is *logged* here rather than described to the caller is
/// the same reason `ApiError::TokenInvalid` collapses three distinct failures
/// into one code: telling a caller that its key id is unknown, as against its
/// signature being wrong, is how a prober enumerates which key ids exist. The
/// operator gets the distinction in the journal; the client gets one answer.
impl FromRequestParts<RouterState> for Authorization {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &RouterState,
    ) -> Result<Self, Self::Rejection> {
        let Some(auth) = state.auth.as_ref() else {
            return Ok(Authorization(None));
        };

        let token = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(bearer_from_header)
            .ok_or(ApiError::TokenMissing)?;

        match verify(&auth.keyring, token, auth.leeway_seconds) {
            Ok(claims) => Ok(Authorization(Some(claims))),
            Err(AuthError::Expired) => Err(ApiError::TokenExpired),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Refused a bearer token. Hint for the sysadmin: an unknown key id \
                     usually means the key was rotated out of [auth].signing_key_file \
                     while tokens signed under it were still in use."
                );
                Err(ApiError::TokenInvalid)
            }
        }
    }
}

/// A path shape from which the project GUID can be read.
///
/// Exists because half the scoped routes are `/{project_guid}` and half are
/// `/{project_guid}/…/{run_id}`. Making the scope extractors generic over the
/// shape keeps **one type per action** — which is what the source-text guard
/// greps for — instead of one per action per arity.
pub trait ProjectPath: DeserializeOwned + Send + 'static {
    fn project_guid(&self) -> Uuid;
}

impl ProjectPath for crate::backend::v0::models::UUIDv4 {
    fn project_guid(&self) -> Uuid {
        self.0
    }
}

impl ProjectPath for (crate::backend::v0::models::UUIDv4, String) {
    fn project_guid(&self) -> Uuid {
        self.0.0
    }
}

/// Builds the four project-scoped extractors, which differ only in the action
/// they require.
macro_rules! project_scope {
    ($name:ident, $action:expr, $doc:literal) => {
        #[doc = $doc]
        pub struct $name<P = crate::backend::v0::models::UUIDv4>(pub P, pub Authorization);

        impl<P: ProjectPath> FromRequestParts<RouterState> for $name<P> {
            type Rejection = ApiError;

            async fn from_request_parts(
                parts: &mut Parts,
                state: &RouterState,
            ) -> Result<Self, Self::Rejection> {
                let auth = Authorization::from_request_parts(parts, state).await?;
                let ApiPath(path) = ApiPath::<P>::from_request_parts(parts, state).await?;

                if let Some(claims) = &auth.0 {
                    // Order matters, and this is the only order that is not an
                    // oracle: the project check comes first, so a caller that
                    // cannot see the project learns nothing at all, and only one
                    // that has already proved it holds the project is told which
                    // action it is missing.
                    if !claims.covers(&path.project_guid()) {
                        return Err(ApiError::ProjectNotFound);
                    }
                    if !claims.permits($action) {
                        return Err(ApiError::ActionNotPermitted {
                            action: $action.as_str(),
                        });
                    }
                }
                Ok($name(path, auth))
            }
        }
    };
}

project_scope!(
    SearchScope,
    Action::Search,
    "Reads over indexed content. 404 — byte-identical to a project that never \
     existed — when the token does not cover it."
);
project_scope!(
    ResearchScope,
    Action::Research,
    "Running research and browsing the stored corpus."
);
project_scope!(
    IndexScope,
    Action::Index,
    "Writes chunks: index, history, cancel, retry."
);
project_scope!(
    DeleteScope,
    Action::Delete,
    "Destroys: files, projects, history, stored runs."
);

// `POST /v0/{project_guid}/index` needs no extractor of its own. It is
// `IndexScope` like the rest, and that the token must name the GUID *in advance*
// is what stops it being an existence oracle: to a caller whose token does not
// carry the GUID, "created" and "refused" are the same answer.

/// The read-only drift check, whose refusal is a *rewrite* rather than an error.
///
/// `POST /drift` documents that an unknown project is not a 404 — every posted
/// file simply comes back `missing`. Answering a project the token cannot see
/// the same way costs nothing and keeps the endpoint a non-oracle for free:
/// `in_scope` is false, the handler takes its existing unknown-project path, and
/// the two cases are indistinguishable on the wire.
pub struct DriftScope {
    pub guid: crate::backend::v0::models::UUIDv4,
    /// False when the token does not cover this project. The handler must treat
    /// it exactly as it treats a project it has never seen.
    pub in_scope: bool,
}

impl FromRequestParts<RouterState> for DriftScope {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &RouterState,
    ) -> Result<Self, Self::Rejection> {
        let auth = Authorization::from_request_parts(parts, state).await?;
        let ApiPath(guid) =
            ApiPath::<crate::backend::v0::models::UUIDv4>::from_request_parts(parts, state).await?;

        if let Some(claims) = &auth.0
            && !claims.permits(Action::Search)
        {
            return Err(ApiError::ActionNotPermitted {
                action: Action::Search.as_str(),
            });
        }
        let in_scope = auth.covers(&guid.0);
        Ok(DriftScope { guid, in_scope })
    }
}

/// Builds the two project-less extractors.
macro_rules! global_scope {
    ($name:ident, $action:expr, $doc:literal) => {
        #[doc = $doc]
        pub struct $name(pub Authorization);

        impl FromRequestParts<RouterState> for $name {
            type Rejection = ApiError;

            async fn from_request_parts(
                parts: &mut Parts,
                state: &RouterState,
            ) -> Result<Self, Self::Rejection> {
                let auth = Authorization::from_request_parts(parts, state).await?;
                if let Some(claims) = &auth.0
                    && !claims.permits($action)
                {
                    return Err(ApiError::ActionNotPermitted {
                        action: $action.as_str(),
                    });
                }
                Ok($name(auth))
            }
        }
    };
}

global_scope!(
    AdminScope,
    Action::Admin,
    "The global operator surfaces — `/gc`, `/status`, `/metrics`. No project \
     list can describe them: `/gc` holds the process-wide guard and walks every \
     collection, so scoping it per project would be a promise it cannot keep."
);
global_scope!(
    ListProjectsScope,
    Action::Search,
    "`GET /projects`, which enumerates projects in a **body** — so no \
     path-shaped check can reach it and the handler must filter by \
     `Authorization::visible_projects` itself. This listing is the reason \
     authorization could not live in the gateway at all."
);
global_scope!(
    ActiveRunsScope,
    Action::Research,
    "`GET /research/active` and its cancel. Global because the semaphore is: \
     the run list is filtered per caller, while `slots_total`/`slots_busy` stay \
     whole, since they are capacity rather than content and a caller planning a \
     queue needs to know the slots are gone — not merely that none of its own \
     runs hold them."
);
global_scope!(
    MintScope,
    Action::Mint,
    "Issuing further tokens, bounded by `Claims::may_mint` so it can never widen \
     what its holder already has."
);

fn malformed_body(rej: JsonRejection) -> ApiError {
    // axum's DefaultBodyLimit surfaces as a 413 JsonRejection; keep the status
    // and the stable code instead of flattening it into a 400 malformed_body.
    if rej.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::BodyTooLarge;
    }
    ApiError::MalformedBody(rej.body_text())
}

fn malformed_query(rej: QueryRejection) -> ApiError {
    ApiError::MalformedBody(rej.body_text())
}

fn malformed_path(rej: PathRejection) -> ApiError {
    ApiError::MalformedPath(rej.body_text())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::DefaultBodyLimit;
    use axum::routing::post;
    use tower::ServiceExt as _;

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Payload {
        #[allow(dead_code)]
        x: i64,
    }

    /// Every response this module produces, as `(status, parsed problem+json)`.
    async fn problem(resp: axum::response::Response) -> (u16, serde_json::Value) {
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.starts_with("application/problem+json"),
            "a rejection escaped as {content_type:?}, not problem+json"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).expect("valid JSON"))
    }

    fn post_json(body: &'static str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap()
    }

    /// The whole reason these wrappers exist: axum's own rejection is a plain-text
    /// 400 with no machine `code`, so a client that keys on `code` — which the error
    /// contract tells it to — gets nothing to key on, and the localization key it
    /// needs is a prose sentence.
    #[tokio::test]
    async fn every_body_rejection_is_problem_json_with_a_stable_code() {
        async fn h(ApiJson(_): ApiJson<Payload>) -> &'static str {
            "ok"
        }
        let app = Router::new().route("/", post(h));

        // Syntactically broken JSON.
        let (status, v) = problem(app.clone().oneshot(post_json("{ nope")).await.unwrap()).await;
        assert_eq!(status, 400);
        assert_eq!(v["code"], "request.malformed_body");

        // Well-formed JSON, wrong type for a field.
        let (status, v) = problem(
            app.clone()
                .oneshot(post_json(r#"{"x": "not a number"}"#))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(v["code"], "request.malformed_body");

        // `deny_unknown_fields`: a typo'd key is a rejection, not a silent ignore.
        let (status, v) = problem(
            app.clone()
                .oneshot(post_json(r#"{"x": 1, "xx": 2}"#))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(v["code"], "request.malformed_body");

        // A body with no `content-type` at all — axum's 415, which must still wear
        // the envelope rather than escaping as plain text.
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/")
                    .body(axum::body::Body::from(r#"{"x": 1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (_, v) = problem(resp).await;
        assert_eq!(v["code"], "request.malformed_body");
    }

    /// A path parameter that does not parse — the commonest being a `project_guid`
    /// that is not a UUID — gets its own code, distinct from a body problem: they
    /// are different mistakes and a client wording an error needs to tell them apart.
    #[tokio::test]
    async fn a_path_that_does_not_parse_is_its_own_code() {
        async fn h(ApiPath(_): ApiPath<uuid::Uuid>) -> &'static str {
            "ok"
        }
        let app = Router::new().route("/{guid}/x", axum::routing::get(h));

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/not-a-uuid/x")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let (status, v) = problem(resp).await;
        assert_eq!(status, 400);
        assert_eq!(v["code"], "request.malformed_path");
    }

    /// A query string is request input like a body and deliberately reuses that code
    /// — `?stream=maybe` on `/index` must be a 400 rather than a silent
    /// fall-through to the non-streaming mode, which is what `deny_unknown_fields`
    /// on `IndexQuery` is for.
    #[tokio::test]
    async fn a_query_that_does_not_parse_is_a_body_problem_not_a_silent_default() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Q {
            #[allow(dead_code)]
            n: u32,
        }
        async fn h(ApiQuery(_): ApiQuery<Q>) -> &'static str {
            "ok"
        }
        let app = Router::new().route("/", axum::routing::get(h));

        for uri in ["/?n=not-a-number", "/?nn=1", "/"] {
            let resp = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let (status, v) = problem(resp).await;
            assert_eq!(status, 400, "{uri} was not rejected");
            assert_eq!(v["code"], "request.malformed_body", "{uri}");
        }
    }

    /// The envelope is a contract, not just a content type: RFC 7807 callers read
    /// `title`/`detail`, and an empty or missing one makes the response unusable to
    /// anything that is not already keying on `code`.
    #[tokio::test]
    async fn a_rejection_carries_a_title_and_a_detail() {
        async fn h(ApiJson(_): ApiJson<Payload>) -> &'static str {
            "ok"
        }
        let app = Router::new().route("/", post(h));

        let (_, v) = problem(app.oneshot(post_json("{ nope")).await.unwrap()).await;
        assert!(
            v["title"].as_str().is_some_and(|s| !s.is_empty()),
            "no title: {v}"
        );
        assert!(
            v["detail"].as_str().is_some_and(|s| !s.is_empty()),
            "no detail: {v}"
        );
        assert_eq!(
            v["status"], 400,
            "the envelope must restate the status: {v}"
        );
    }

    #[tokio::test]
    async fn oversized_body_is_413_problem_json_not_400() {
        #[derive(serde::Deserialize)]
        struct Body {
            #[allow(dead_code)]
            x: String,
        }
        async fn h(ApiJson(_): ApiJson<Body>) -> &'static str {
            "ok"
        }

        let app = Router::new()
            .route("/", post(h))
            .layer(DefaultBodyLimit::max(8));

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"x":"far-longer-than-eight-bytes"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], "request.body_too_large");
    }
}
