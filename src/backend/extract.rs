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
