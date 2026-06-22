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
    ApiError::MalformedBody(rej.body_text())
}

fn malformed_query(rej: QueryRejection) -> ApiError {
    ApiError::MalformedBody(rej.body_text())
}

fn malformed_path(rej: PathRejection) -> ApiError {
    ApiError::MalformedPath(rej.body_text())
}
