//! API errors and how they map to HTTP answers. Internal details (database and storage errors) are logged,
//! never sent to the client.

use axum::Json;
use axum::extract::rejection::{BytesRejection, JsonRejection, PathRejection, QueryRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use craft_cloud_meta::MetaError;
use craft_cloud_proto::api::ErrorBody;
use craft_cloud_proto::{ChunkId, Conflict};
use craft_cloud_store::StoreError;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub conflicts: Option<Vec<Conflict>>,
    pub missing: Option<Vec<ChunkId>>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), conflicts: None, missing: None }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid", message)
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", format!("no such {what}"))
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "sign in again: the device token is missing, unknown or revoked")
    }

    pub fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "the server hit an internal error; the admin can find details in its log")
    }

    pub fn missing_chunks(missing: Vec<ChunkId>) -> Self {
        let mut e = Self::new(StatusCode::CONFLICT, "missing_chunks", format!("{} chunk(s) must be uploaded first", missing.len()));
        e.missing = Some(missing);
        e
    }
}

impl From<MetaError> for ApiError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::Rejected(conflicts) => {
                let mut err = Self::new(StatusCode::CONFLICT, "conflict", format!("{} operation(s) conflict; nothing was applied", conflicts.len()));
                err.conflicts = Some(conflicts);
                err
            }
            MetaError::MissingChunks(missing) => Self::missing_chunks(missing),
            MetaError::ResyncRequired => Self::new(StatusCode::GONE, "resync_required", "list the whole tree again"),
            MetaError::Invalid(message) => Self::invalid(message),
            MetaError::NotFound(what) => Self::not_found(what),
            MetaError::UsernameTaken => Self::new(StatusCode::CONFLICT, "username_taken", "the username is taken"),
            MetaError::Db(_) | MetaError::Corrupt(_) | MetaError::UnsupportedUrl => {
                tracing::error!(error = %e, "metadata error");
                Self::internal()
            }
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::HashMismatch(_) => Self::invalid("the bytes don't match the ID"),
            StoreError::TooLarge { len, max, .. } => {
                Self::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", format!("{len} bytes; the limit is {max}"))
            }
            StoreError::NotFound(_) => Self::not_found("blob"),
            StoreError::Corrupt { .. } | StoreError::Io(_) | StoreError::Backend(_) => {
                tracing::error!(error = %e, "storage error");
                Self::internal()
            }
        }
    }
}

/// Anything axum refuses before a handler runs becomes the same JSON body as every other error.
fn rejected(status: StatusCode, message: String) -> ApiError {
    let code = if status == StatusCode::PAYLOAD_TOO_LARGE { "too_large" } else { "invalid" };
    ApiError::new(status, code, message)
}

impl From<JsonRejection> for ApiError {
    fn from(e: JsonRejection) -> Self {
        rejected(e.status(), e.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(e: QueryRejection) -> Self {
        rejected(e.status(), e.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(e: PathRejection) -> Self {
        rejected(e.status(), e.body_text())
    }
}

impl From<BytesRejection> for ApiError {
    fn from(e: BytesRejection) -> Self {
        rejected(e.status(), e.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody { error: self.code.to_owned(), message: self.message, conflicts: self.conflicts, missing: self.missing };
        (self.status, Json(body)).into_response()
    }
}
