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
