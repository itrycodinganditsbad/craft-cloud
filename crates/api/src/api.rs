//! The parts of the HTTP API's bodies (`/api/v1/…`) that apps see through the agent's local API: the error codes, the
//! error answer's code and message, the account's devices and add-a-computer codes. The rest of the HTTP API (commit
//! batches, manifests, the full error body) is the server's and the agent's, in `craft_cloud_proto::api`, which
//! re-exports these.

use serde::{Deserialize, Serialize};

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
    pub const INTERNAL: &str = "internal";
}

/// What a client reads of every error answer: `error`, a stable code (one of [`codes`], or the local API's), and
/// `message`, for people. Every other field is ignored (the full body is `craft_cloud_proto::api::ErrorBody`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ErrorAnswer {
    pub error: String,
    pub message: String,
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

/// One of the signed-in user's devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub created_ms: i64,
    pub last_used_ms: i64,
    pub revoked: bool,
    /// The device making this request.
    pub current: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_answer_reads_any_error_body() {
        let json = serde_json::json!({ "error": "conflict", "message": "m", "conflicts": [{ "op": 0, "kind": "root" }], "quota": { "used": 1, "limit": 2 } });
        let answer: ErrorAnswer = serde_json::from_value(json).unwrap();
        assert_eq!(answer, ErrorAnswer { error: "conflict".into(), message: "m".into() });
    }
}
