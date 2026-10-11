//! Passwords, device tokens, and the middleware that signs requests in.
//!
//! Passwords are hashed with Argon2id. A device token is 32 random bytes, sent as base64url; the server stores
//! only the BLAKE3 of the token text and finds the device by it. Tokens are never logged.

use std::sync::Arc;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_cloud_proto::api::EPOCH_HEADER;

use crate::error::ApiError;
use crate::state::{AppState, now_ms};

/// Passwords are 8 to 1024 bytes (the cap keeps hashing cheap to refuse).
pub const MIN_PASSWORD_BYTES: usize = 8;
pub const MAX_PASSWORD_BYTES: usize = 1024;
/// A device's `last used` time is written at most this often.
const TOUCH_INTERVAL_MS: i64 = 60_000;

pub fn check_password_rules(password: &str) -> Result<(), ApiError> {
    if (MIN_PASSWORD_BYTES..=MAX_PASSWORD_BYTES).contains(&password.len()) { Ok(()) } else { Err(ApiError::invalid("passwords are 8 to 1024 bytes")) }
}

pub fn hash_password(password: &str) -> Result<String, ApiError> {
    Argon2::default().hash_password(password.as_bytes()).map(|h| h.to_string()).map_err(|_| ApiError::internal())
}

/// Checks a password against a stored hash. For an unknown user the caller verifies against
/// `AppState::dummy_hash` instead, so both cases take the same time. An unparseable stored hash returns
/// false at once.
pub fn verify_password(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// A new device token and the hash to store for it.
pub fn new_token() -> Result<(String, [u8; 32]), ApiError> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| {
        tracing::error!(error = %e, "the system random source failed");
        ApiError::internal()
    })?;
    let token = URL_SAFE_NO_PAD.encode(raw);
    let hash = token_hash(&token);
    Ok((token, hash))
}

pub fn token_hash(token: &str) -> [u8; 32] {
    *blake3::hash(token.as_bytes()).as_bytes()
}

fn bearer(request: &Request) -> Option<&str> {
    let value = request.headers().get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    (token.len() == 43 && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then_some(token)
}

/// Signs the request in from its device token, puts the device in the request extensions, and adds the
/// space's epoch to the answer. The epoch is read before the handler runs, so a failed read stops the
/// request before anything has happened.
pub async fn require_device(State(state): State<Arc<AppState>>, mut request: Request, next: Next) -> Result<Response, ApiError> {
    let token = bearer(&request).ok_or_else(ApiError::unauthorized)?;
    let device = state.meta.device_by_token(&token_hash(token)).await?.ok_or_else(ApiError::unauthorized)?;
    state.touch(device.device, now_ms()).await;
    let space = device.space;
    let epoch = state.meta.space_info(space).await?.epoch;
    request.extensions_mut().insert(device);
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&epoch.to_string()) {
        response.headers_mut().insert(EPOCH_HEADER, value);
    }
    Ok(response)
}

impl AppState {
    /// Updates the device's `last used` time, at most once a minute (every write takes the SQLite write lock).
    async fn touch(&self, device: craft_cloud_proto::DeviceId, now: i64) {
        let due = {
            let mut touches = self.touches.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let last = touches.get(&device).copied().unwrap_or(i64::MIN);
            if now.saturating_sub(last) >= TOUCH_INTERVAL_MS {
                touches.insert(device, now);
                true
            } else {
                false
            }
        };
        if due && let Err(e) = self.meta.touch_device(device, now).await {
            tracing::warn!(error = %e, "couldn't record device use");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_hash_and_verify() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("wrong horse", &hash));
        assert!(!verify_password("correct horse", "not a hash"));
    }

    #[test]
    fn password_rules() {
        assert!(check_password_rules("short").is_err());
        assert!(check_password_rules("eight888").is_ok());
        assert!(check_password_rules(&"x".repeat(1025)).is_err());
    }

    #[test]
    fn tokens_are_43_url_safe_chars_and_unique() {
        let (a, ha) = new_token().unwrap();
        let (b, _) = new_token().unwrap();
        assert_eq!(a.len(), 43);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
        assert_ne!(a, b);
        assert_eq!(ha, token_hash(&a));
    }
}
