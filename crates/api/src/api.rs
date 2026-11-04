//! The parts of the HTTP API's bodies (`/api/v1/…`) that apps see through the agent's local API: the error codes, the
//! error answer's code and message, the account's devices and add-a-computer codes. The rest of the HTTP API (commit
//! batches, manifests, the full error body) is the server's and the agent's, in `craft_cloud_proto::api`, which
//! re-exports these.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ids::DeviceId;

/// Stable error codes, the `error` field of an error answer ([`ErrorAnswer`]).
pub mod codes {
    /// A commit's operations clash with the current state; `conflicts` lists them.
    pub const CONFLICT: &str = "conflict";
    /// Chunks must be uploaded first; `missing` lists them.
    pub const MISSING_CHUNKS: &str = "missing_chunks";
    /// The client's change counter is stale or ahead of the server: list the whole tree again.
    pub const RESYNC_REQUIRED: &str = "resync_required";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const INVALID: &str = "invalid";
    pub const NOT_FOUND: &str = "not_found";
    pub const TOO_LARGE: &str = "too_large";
    /// A request body took longer to arrive than the server allows for it (HTTP 408); send it again.
    pub const TIMEOUT: &str = "timeout";
    pub const USERNAME_TAKEN: &str = "username_taken";
    /// A chunk or manifest would take the account over the bytes the server lets it store (HTTP 507); `quota` says
    /// how much it stores and may store.
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
    /// `join`: the code is wrong, expired or used (never which).
    pub const INVALID_CODE: &str = "invalid_code";
    /// `join`: an invite to an account without a password needs one; send it again with `password`.
    pub const PASSWORD_REQUIRED: &str = "password_required";
    /// `join`: the new password breaks the server's rules (the message says which).
    pub const WEAK_PASSWORD: &str = "weak_password";
    /// A signed-in route that asks for the account's password (changing it) got a wrong one (HTTP 403, never 401: a
    /// 401 on a signed-in route means the device was revoked, and the agent signs out on it). The local API's
    /// `login` uses the same code when the server refuses the username or password.
    pub const WRONG_PASSWORD: &str = "wrong_password";
    /// The device is signed in but may not do this (HTTP 403): a member calling an `/admin` route, or an admin doing
    /// what their account may not. The local API refuses with it a request that didn't come from a local program.
    pub const FORBIDDEN: &str = "forbidden";
    /// A sign-in or a join with the right password or code, for an account an admin disabled or marked for removal
    /// (HTTP 403). Said only after the password or code was right, never to a guesser.
    pub const ACCOUNT_DISABLED: &str = "account_disabled";
    /// An admin change would leave the server without an enabled administrator (HTTP 409).
    pub const LAST_ADMIN: &str = "last_admin";
    /// A password sign-in (or the redeeming of an invite or reset code for an account with two-factor sign-in) was
    /// right, but needs a second factor (HTTP 401, no token). The answer's `two_factor` carries the challenge.
    pub const TWO_FACTOR_REQUIRED: &str = "two_factor_required";
    /// A code or recovery code was wrong, or a code was used before (HTTP 401 at sign-in and join, which are not
    /// signed in, and 403 on a signed-in route, where a 401 would read as "this device was revoked").
    pub const TWO_FACTOR_INVALID: &str = "two_factor_invalid";
    /// The server has no key for two-factor secrets (`[two_factor] key_file`), so it can't set it up (HTTP 503).
    pub const TWO_FACTOR_UNAVAILABLE: &str = "two_factor_unavailable";
    /// A set-up is running on another device (HTTP 409).
    pub const TWO_FACTOR_PENDING: &str = "two_factor_pending";
    /// Too many wrong codes: every check of this account's second factor is refused until `Retry-After` is over
    /// (HTTP 429). A lock never lasts for good.
    pub const TWO_FACTOR_LOCKED: &str = "two_factor_locked";
    pub const INTERNAL: &str = "internal";
}

/// What a client reads of every error answer: `error`, a stable code (one of [`codes`], or the local API's), and
/// `message`, for people. Every other field is ignored (the full body is `craft_cloud_proto::api::ErrorBody`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ErrorAnswer {
    pub error: String,
    pub message: String,
}

/// The second step of a sign-in: what `two_factor_required` carries. `challenge` is a secret, good once and until
/// `expires_ms` (milliseconds since the Unix epoch, by the server's clock); the code or recovery code goes with it to
/// `POST /auth/login/second-factor`. Its `Debug` output hides the challenge. For `POST /join` there is no challenge
/// (the code is simply sent again with the second factor): `challenge` is empty there.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwoFactorChallenge {
    pub challenge: String,
    pub expires_ms: i64,
}

impl std::fmt::Debug for TwoFactorChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwoFactorChallenge").field("challenge", &"<redacted>").field("expires_ms", &self.expires_ms).finish()
    }
}

/// Where an add-a-computer code stands, in [`PairingStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    /// Not used yet, and still good.
    Waiting,
    /// A computer joined with it; [`PairingStatus::device_name`] names it.
    Used,
    /// Its time ran out unused.
    Expired,
}

/// `GET /api/v1/pairings/{id}` (device auth, the account's own pairings only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingStatus {
    pub state: PairingState,
    /// The device that joined, once `used`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
}

/// One of the signed-in user's devices. The fields after `current` are newer than the first release of the API: an
/// older server leaves them out, and a client that doesn't know them ignores them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub created_ms: i64,
    pub last_used_ms: i64,
    pub revoked: bool,
    /// The device making this request.
    pub current: bool,
    /// What the device said about its client when it signed in: `agent`, `desktop` or `cli`, or empty. Display only.
    #[serde(default)]
    pub client_kind: String,
    /// The platform the device said it runs on, free text, empty when it didn't say.
    #[serde(default)]
    pub platform: String,
    /// The version of the client the device said it runs, free text, empty when it didn't say.
    #[serde(default)]
    pub app_version: String,
    /// When the device was revoked, milliseconds since the Unix epoch, by the server's clock.
    #[serde(default)]
    pub revoked_ms: Option<i64>,
    /// Why it was revoked: `user`, `others`, `password`, `admin`, `disabled`, `two_factor`, `idle` or `reset`. A client
    /// treats a reason it doesn't know as "revoked".
    #[serde(default)]
    pub revoke_reason: Option<String>,
}

/// `GET /api/v1/account` (device auth): the signed-in account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub user_id: Uuid,
    /// The username, in lowercase.
    pub username: String,
    /// `admin` or `member`.
    pub role: String,
    pub created_ms: i64,
    /// When the password last changed through the server; `None` while it is still the first one.
    #[serde(default)]
    pub password_changed_ms: Option<i64>,
    /// Whether signing in asks for a second factor.
    #[serde(default)]
    pub two_factor_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_from_an_older_server_reads_with_empty_new_fields() {
        let id = DeviceId::generate();
        let json = serde_json::json!({ "id": id, "name": "pc", "created_ms": 1, "last_used_ms": 2, "revoked": false, "current": true });
        let device: DeviceInfo = serde_json::from_value(json).unwrap();
        assert_eq!((device.client_kind.as_str(), device.platform.as_str(), device.app_version.as_str()), ("", "", ""));
        assert_eq!((device.revoked_ms, device.revoke_reason), (None, None));
    }

    #[test]
    fn the_new_error_codes_are_stable() {
        assert_eq!(codes::WRONG_PASSWORD, "wrong_password");
        assert_eq!(codes::FORBIDDEN, "forbidden");
        assert_eq!(codes::ACCOUNT_DISABLED, "account_disabled");
        assert_eq!(codes::LAST_ADMIN, "last_admin");
    }

    #[test]
    fn the_two_factor_codes_are_stable_and_a_challenge_hides_itself() {
        assert_eq!(codes::TWO_FACTOR_REQUIRED, "two_factor_required");
        assert_eq!(codes::TWO_FACTOR_INVALID, "two_factor_invalid");
        assert_eq!(codes::TWO_FACTOR_UNAVAILABLE, "two_factor_unavailable");
        assert_eq!(codes::TWO_FACTOR_PENDING, "two_factor_pending");
        assert_eq!(codes::TWO_FACTOR_LOCKED, "two_factor_locked");
        let shown = format!("{:?}", TwoFactorChallenge { challenge: "challenge-secret".into(), expires_ms: 5 });
        assert!(!shown.contains("challenge-secret") && shown.contains("<redacted>"), "{shown}");
    }

    #[test]
    fn an_error_answer_reads_any_error_body() {
        let json = serde_json::json!({ "error": "conflict", "message": "m", "conflicts": [{ "op": 0, "kind": "root" }], "quota": { "used": 1, "limit": 2 } });
        let answer: ErrorAnswer = serde_json::from_value(json).unwrap();
        assert_eq!(answer, ErrorAnswer { error: "conflict".into(), message: "m".into() });
    }
}
