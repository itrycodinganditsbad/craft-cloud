//! API errors and how they map to HTTP answers. Internal details (database and storage errors) are logged,
//! never sent to the client.

use axum::Json;
use axum::extract::rejection::{BytesRejection, JsonRejection, PathRejection, QueryRejection};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use craft_cloud_meta::MetaError;
use craft_cloud_proto::api::{ErrorBody, QuotaUse, ServerRoom, TwoFactorChallenge, codes};
use craft_cloud_proto::{ChunkId, Conflict};
use craft_cloud_store::StoreError;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub conflicts: Option<Vec<Conflict>>,
    /// A boxed slice, to keep this type small: every handler's `Result` carries it.
    pub missing: Option<Box<[ChunkId]>>,
    /// Boxed: rare, and every handler's `Result` carries this type.
    pub quota: Option<Box<QuotaUse>>,
    /// What the server knows of its room, for `server_full`. Boxed for the same reason as `quota`.
    pub server: Option<Box<ServerRoom>>,
    /// Seconds for the `Retry-After` header.
    pub retry_after: Option<u32>,
    /// The challenge of `two_factor_required`. Boxed for the same reason as `quota`.
    pub two_factor: Option<Box<TwoFactorChallenge>>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), conflicts: None, missing: None, quota: None, server: None, retry_after: None, two_factor: None }
    }

    /// `507 quota_exceeded`: the account would store more than the server lets it.
    pub fn quota_exceeded(used: u64, limit: u64) -> Self {
        let message = format!("the account's storage on the server is full: {used} of {limit} bytes used");
        let mut e = Self::new(StatusCode::INSUFFICIENT_STORAGE, codes::QUOTA_EXCEEDED, message);
        e.quota = Some(Box::new(QuotaUse { used, limit }));
        e
    }

    /// `507 server_full`: the server refuses new uploads because it is full (`refuse_when_full`). Not the account's quota.
    pub fn server_full(room: ServerRoom) -> Self {
        let mut e = Self::new(
            StatusCode::INSUFFICIENT_STORAGE,
            codes::SERVER_FULL,
            "the server is full and is not taking new uploads: its admin needs to free space or raise its capacity",
        );
        e.server = Some(Box::new(room));
        e
    }

    /// `403 forbidden`: signed in, but not allowed to do this.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, codes::FORBIDDEN, message)
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

    /// `401 two_factor_required`: the password (or code) was right and the account needs its second factor. No token.
    /// `challenge` is the secret to send back to `POST /auth/login/second-factor` (empty for `/join`, which has none).
    pub fn two_factor_required(challenge: String, expires_ms: i64) -> Self {
        let mut e = Self::new(StatusCode::UNAUTHORIZED, codes::TWO_FACTOR_REQUIRED, "this account needs a code from its authenticator app");
        e.two_factor = Some(Box::new(TwoFactorChallenge { challenge, expires_ms }));
        e
    }

    /// `403 two_factor_invalid`: a wrong code or recovery code on a signed-in route (never 401: on a signed-in route the
    /// agent reads 401 as "this device was revoked"). [`ApiError::on_public_route`] makes it 401 for sign-in and join.
    pub fn two_factor_invalid() -> Self {
        Self::new(StatusCode::FORBIDDEN, codes::TWO_FACTOR_INVALID, "the code is wrong, or was used already")
    }

    /// `429 two_factor_locked` with `Retry-After`.
    pub fn two_factor_locked(retry_after_secs: u32) -> Self {
        let mut e = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            codes::TWO_FACTOR_LOCKED,
            "too many wrong codes: this account's second factor is locked for a while; try again later, or ask the server's admin",
        );
        e.retry_after = Some(retry_after_secs);
        e
    }

    /// `503 two_factor_unavailable`: the server has no key for two-factor secrets.
    pub fn two_factor_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            codes::TWO_FACTOR_UNAVAILABLE,
            "this server has no key for two-factor secrets; the admin sets [two_factor] key_file in its config",
        )
    }

    /// The answer on a route that is not signed in (sign-in's second step, join): a wrong second factor is `401` there,
    /// where it is `403` on every signed-in route.
    pub fn on_public_route(mut self) -> Self {
        if self.code == codes::TWO_FACTOR_INVALID {
            self.status = StatusCode::UNAUTHORIZED;
        }
        self
    }

    pub fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, codes::INTERNAL, "the server hit an internal error; the admin can find details in its log")
    }

    pub fn missing_chunks(missing: Vec<ChunkId>) -> Self {
        let mut e = Self::new(StatusCode::CONFLICT, codes::MISSING_CHUNKS, format!("{} chunk(s) must be uploaded first", missing.len()));
        e.missing = Some(missing.into_boxed_slice());
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
            MetaError::DeviceRevoked => Self::unauthorized(),
            MetaError::Forbidden(message) => Self::forbidden(message),
            MetaError::LastAdmin => Self::new(StatusCode::CONFLICT, codes::LAST_ADMIN, e.to_string()),
            MetaError::AccountDisabled => Self::new(StatusCode::FORBIDDEN, codes::ACCOUNT_DISABLED, "this account is disabled; ask an administrator"),
            // The status of a wrong second factor depends on the route: 403 here (every signed-in route), made 401 by
            // `on_public_route` for sign-in and join.
            MetaError::TwoFactorInvalid => Self::two_factor_invalid(),
            MetaError::TwoFactorLocked { retry_after_ms } => Self::two_factor_locked(craft_cloud_meta::retry_after_secs(retry_after_ms)),
            MetaError::TwoFactorPending => {
                Self::new(StatusCode::CONFLICT, codes::TWO_FACTOR_PENDING, "a two-factor set-up is open on another device: finish it there, or wait")
            }
            // Only the join route can see this and it builds its own answer; a signed-in route never asks a second factor.
            MetaError::TwoFactorRequired => Self::new(StatusCode::UNAUTHORIZED, codes::TWO_FACTOR_REQUIRED, "this account needs its second factor"),
            MetaError::ChallengeGone => {
                Self::new(StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED, "this sign-in step doesn't work any more: sign in again")
            }
            MetaError::Conflict(message) => Self::new(StatusCode::CONFLICT, codes::CONFLICT, message),
            MetaError::UsernameTaken => Self::new(StatusCode::CONFLICT, codes::USERNAME_TAKEN, "the username is taken"),
            MetaError::QuotaExceeded { used, limit } => Self::quota_exceeded(used, limit),
            MetaError::DocFull { .. } | MetaError::LiveFull { .. } | MetaError::TooManyDocs { .. } | MetaError::TooManyDocRows { .. } => {
                Self::new(StatusCode::INSUFFICIENT_STORAGE, codes::QUOTA_EXCEEDED, e.to_string())
            }
            MetaError::Db(_) | MetaError::Corrupt(_) | MetaError::Missing(_) | MetaError::UnsupportedUrl => {
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
            missing: self.missing.map(Vec::from),
            quota: self.quota.map(|q| *q),
            server: self.server.map(|r| *r),
            two_factor: self.two_factor.map(|t| *t),
        };
        let mut response = (self.status, Json(body)).into_response();
        if let Some(secs) = self.retry_after {
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_that_lost_a_race_is_a_conflict_and_a_signed_out_caller_is_unauthorized() {
        let conflict = ApiError::from(MetaError::Conflict("changed meanwhile"));
        assert_eq!((conflict.status, conflict.code), (StatusCode::CONFLICT, codes::CONFLICT));
        let revoked = ApiError::from(MetaError::DeviceRevoked);
        assert_eq!((revoked.status, revoked.code), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED));
        let forbidden = ApiError::from(MetaError::Forbidden("no"));
        assert_eq!((forbidden.status, forbidden.code), (StatusCode::FORBIDDEN, codes::FORBIDDEN));
        let last = ApiError::from(MetaError::LastAdmin);
        assert_eq!((last.status, last.code), (StatusCode::CONFLICT, codes::LAST_ADMIN));
        let disabled = ApiError::from(MetaError::AccountDisabled);
        assert_eq!((disabled.status, disabled.code), (StatusCode::FORBIDDEN, codes::ACCOUNT_DISABLED));
    }

    #[test]
    fn second_factor_errors_are_403_on_signed_in_routes_and_401_on_public_ones() {
        let invalid = ApiError::from(MetaError::TwoFactorInvalid);
        assert_eq!((invalid.status, invalid.code), (StatusCode::FORBIDDEN, codes::TWO_FACTOR_INVALID));
        let public = invalid.on_public_route();
        assert_eq!((public.status, public.code), (StatusCode::UNAUTHORIZED, codes::TWO_FACTOR_INVALID));
        let locked = ApiError::from(MetaError::TwoFactorLocked { retry_after_ms: 1500 });
        assert_eq!((locked.status, locked.code, locked.retry_after), (StatusCode::TOO_MANY_REQUESTS, codes::TWO_FACTOR_LOCKED, Some(2)));
        let locked = locked.on_public_route();
        assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
        let pending = ApiError::from(MetaError::TwoFactorPending);
        assert_eq!((pending.status, pending.code), (StatusCode::CONFLICT, codes::TWO_FACTOR_PENDING));
        assert_eq!(ApiError::two_factor_unavailable().status, StatusCode::SERVICE_UNAVAILABLE);
        let gone = ApiError::from(MetaError::ChallengeGone);
        assert_eq!((gone.status, gone.code), (StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED));
        let required = ApiError::two_factor_required("challenge-secret".into(), 5);
        assert_eq!((required.status, required.code), (StatusCode::UNAUTHORIZED, codes::TWO_FACTOR_REQUIRED));
        assert!(!format!("{required:?}").contains("challenge-secret"), "the challenge is hidden in Debug");
    }
}
