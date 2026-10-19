//! API errors and how they map to HTTP answers. Internal details (database and storage errors) are logged,
//! never sent to the client.

use axum::Json;
use axum::extract::rejection::{BytesRejection, JsonRejection, PathRejection, QueryRejection};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use craft_cloud_meta::MetaError;
use craft_cloud_proto::api::{ErrorBody, QuotaUse, codes};
use craft_cloud_proto::{ChunkId, Conflict};
use craft_cloud_store::StoreError;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub conflicts: Option<Vec<Conflict>>,
    pub missing: Option<Vec<ChunkId>>,
    /// Boxed: rare, and every handler's `Result` carries this type.
    pub quota: Option<Box<QuotaUse>>,
    /// Seconds for the `Retry-After` header.
    pub retry_after: Option<u32>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), conflicts: None, missing: None, quota: None, retry_after: None }
    }

    /// `507 quota_exceeded`: the account would store more than the server lets it.
    pub fn quota_exceeded(used: u64, limit: u64) -> Self {
        let message = format!("the account's storage on the server is full: {used} of {limit} bytes used");
        let mut e = Self::new(StatusCode::INSUFFICIENT_STORAGE, codes::QUOTA_EXCEEDED, message);
        e.quota = Some(Box::new(QuotaUse { used, limit }));
        e
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, codes::INVALID, message)
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, codes::TOO_LARGE, message)
    }

    pub fn rate_limited(message: impl Into<String>, retry_after: Option<u32>) -> Self {
        let mut e = Self::new(StatusCode::TOO_MANY_REQUESTS, codes::RATE_LIMITED, message);
        e.retry_after = retry_after;
        e
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, codes::NOT_FOUND, format!("no such {what}"))
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED, "sign in again: the device token is missing, unknown or revoked")
    }

    /// `404 invalid_code`: the one answer for a join code that is wrong, expired, used or malformed, so it never says
    /// which.
    pub fn invalid_code() -> Self {
        Self::new(StatusCode::NOT_FOUND, codes::INVALID_CODE, "This code doesn't work any more. Make a new one on a signed-in computer.")
    }

    /// `400 password_required`: an invite to an account without a password needs a new one.
    pub fn password_required() -> Self {
        Self::new(StatusCode::BAD_REQUEST, codes::PASSWORD_REQUIRED, "this invite is for a new account: choose a password for it")
    }

    pub fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, codes::INTERNAL, "the server hit an internal error; the admin can find details in its log")
    }

    pub fn missing_chunks(missing: Vec<ChunkId>) -> Self {
        let mut e = Self::new(StatusCode::CONFLICT, codes::MISSING_CHUNKS, format!("{} chunk(s) must be uploaded first", missing.len()));
        e.missing = Some(missing);
        e
    }
}

impl From<MetaError> for ApiError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::Rejected(conflicts) => {
                let mut err =
                    Self::new(StatusCode::CONFLICT, codes::CONFLICT, format!("{} operation(s) conflict; nothing was applied", conflicts.len()));
                err.conflicts = Some(conflicts);
                err
            }
            MetaError::MissingChunks(missing) => Self::missing_chunks(missing),
            MetaError::ResyncRequired => Self::new(StatusCode::GONE, codes::RESYNC_REQUIRED, "list the whole tree again"),
            MetaError::Invalid(message) => Self::invalid(message),
            MetaError::NotFound(what) => Self::not_found(what),
            MetaError::InvalidCode => Self::invalid_code(),
            MetaError::PasswordRequired => Self::password_required(),
            MetaError::UsernameTaken => Self::new(StatusCode::CONFLICT, codes::USERNAME_TAKEN, "the username is taken"),
            MetaError::QuotaExceeded { used, limit } => Self::quota_exceeded(used, limit),
            MetaError::DocFull { .. } | MetaError::LiveFull { .. } | MetaError::TooManyDocs { .. } | MetaError::TooManyDocRows { .. } => {
                Self::new(StatusCode::INSUFFICIENT_STORAGE, codes::QUOTA_EXCEEDED, e.to_string())
            }
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
            StoreError::TooLarge { len, max, .. } => Self::too_large(format!("{len} bytes; the limit is {max}")),
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
    let code = if status == StatusCode::PAYLOAD_TOO_LARGE { codes::TOO_LARGE } else { codes::INVALID };
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
        let body = ErrorBody {
            error: self.code.to_owned(),
            message: self.message,
            conflicts: self.conflicts,
            missing: self.missing,
            quota: self.quota.map(|q| *q),
        };
        let mut response = (self.status, Json(body)).into_response();
        if let Some(secs) = self.retry_after {
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        response
    }
}
