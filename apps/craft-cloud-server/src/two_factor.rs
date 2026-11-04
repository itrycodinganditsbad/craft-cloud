//! Two-factor sign-in over HTTP: `GET /api/v1/account/two-factor`, `POST …/setup`, `…/confirm`, `…/disable`,
//! `…/recovery-codes`, and the second step of a sign-in, `POST /api/v1/auth/login/second-factor`. The admin API's
//! confirmation uses [`confirm_admin_second_factor`], `POST /join` the same parsing and proofs. The key for the secrets,
//! its checks at start and `two-factor rekey` are here too.
//!
//! - **TOTP and recovery codes** (see [`crate::totp`]): the secret is 160 random bits, shown once as Base32 and as an
//!   `otpauth://` URI, and kept only sealed with the key from `[two_factor] key_file` ([`crate::secretbox`]). Ten recovery
//!   codes of 16 characters (80 bits, the alphabet of the join codes) are shown once; the database keeps their BLAKE3
//!   hashes, so they work whatever happens to the key, and 80 bits can't be searched even from a leaked database.
//! - **Statuses**: a wrong password is `403 wrong_password`; a wrong code `403 two_factor_invalid` on a signed-in route and
//!   `401` at sign-in and join; a body that is not what it should be, a code that is not six digits and a recovery code
//!   that is not 16 letters and digits `400 invalid` (nothing is counted); a lock `429 two_factor_locked` with
//!   `Retry-After`. 401 is otherwise the answer for a token that is revoked, and for a challenge that doesn't work.
//! - **Every change takes the account's lock in `craft_cloud_meta`** and checks again there what the handler checked
//!   before (the device still signed in, the password still the verified one, the account's current state). Handlers end
//!   the live connections of the devices a change signed out only after it committed.
//! - **Counting**: a wrong password counts against `password_changes` (as a password change does), a wrong second factor
//!   against the account (the lock) and, at sign-in's second step, join and the admin's confirmation, against
//!   [`AppState::second_factors`] ([`SECOND_FACTORS_PER_USER`] per account and [`SECOND_FACTORS_PER_IP`] per client
//!   address in five minutes; a right proof is given back). Wrong codes at the second step also count against the
//!   sign-in limiter.
//! - **Nothing secret is logged**: not a secret, a code, a recovery code, a challenge, the key or a password.
//! - **The key**: [`load_keys`] reads the key files, [`check_startup`] refuses to start a server that has secrets it can't
//!   open (all of them are opened, not a sample), and [`rekey`] rewrites the secrets under the current key.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, FromRequest, Request, State};
use axum::http::{HeaderMap, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_cloud_meta::{DeviceActor, DeviceAuth, Meta, MetaError, Proof, RECOVERY_CODES, TotpVerifier, UnreadableSecret, UserId};
use craft_cloud_proto::api::{
    LoginResponse, RecoveryCodes, SecondFactorRequest, TwoFactorConfirmRequest, TwoFactorProofRequest, TwoFactorSetupRequest, TwoFactorSetupResponse,
    TwoFactorStatus,
};
use craft_cloud_proto::join::{Code, INVITE_CHARS};

use crate::account::confirm_password;
use crate::api::{LOGINS_PER_IP, LOGINS_PER_USER, TINY_JSON_BYTES, body_too_slow};
use crate::auth::{new_token, token_hash};
use crate::config::Config;
use crate::error::ApiError;
use crate::ratelimit::{client_ip, ip_key};
use crate::secretbox::{KeyError, SecretBox, TwoFactorKey};
use crate::state::AppState;
use crate::totp;

/// Wrong second factors one account may give in [`AppState::second_factors`]'s window (five minutes).
pub const SECOND_FACTORS_PER_USER: u32 = 10;
/// Wrong second factors one client address may give in the same window.
pub const SECOND_FACTORS_PER_IP: u32 = 30;

type Shared = Arc<AppState>;

// ---- reading and checking what a client sends ----

/// Reads a tiny JSON body (at most [`TINY_JSON_BYTES`]) under the usual deadline. Whatever is wrong with its shape (not
/// JSON, a missing or mistyped field) is `400 invalid`, one too long `413 too_large`, one too slow `408`.
pub(crate) async fn read_tiny<T: serde::de::DeserializeOwned>(state: &AppState, request: Request) -> Result<T, ApiError> {
    let bytes = match tokio::time::timeout(state.limits.body_time(TINY_JSON_BYTES), Bytes::from_request(request, &())).await {
        Ok(bytes) => bytes?,
        Err(_) => return Err(body_too_slow()),
    };
    serde_json::from_slice(&bytes).map_err(|_| ApiError::invalid("the body must be a JSON object with the fields of this call"))
}

/// A second factor as a client sent it, checked for its shape only.
pub(crate) enum ProofInput {
    /// Six digits.
    Totp(String),
    /// The BLAKE3 hash of the normalised recovery code.
    Recovery([u8; 32]),
}

/// The second factor of a body: `totp_code` (six ASCII digits) or `recovery_code` (16 letters and digits, case, dashes and
/// spaces forgiven, `O` for `0` and `I`, `L` for `1`), never both. `None` when the body has neither. Anything else is `400
/// invalid`, before anything is counted.
pub(crate) fn parse_proof(totp_code: Option<String>, recovery_code: Option<String>) -> Result<Option<ProofInput>, ApiError> {
    match (totp_code, recovery_code) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(ApiError::invalid("send either totp_code or recovery_code, not both")),
        (Some(code), None) => {
            if !totp::is_code(&code) {
                return Err(ApiError::invalid("totp_code is the six digits from the authenticator app"));
            }
            Ok(Some(ProofInput::Totp(code)))
        }
        (None, Some(code)) => match Code::parse(&code, INVITE_CHARS) {
            Some(code) => Ok(Some(ProofInput::Recovery(code.hash()))),
            None => Err(ApiError::invalid("recovery_code is 16 letters and digits, like ABCD-EFGH-JKMN-PQRS")),
        },
    }
}

/// A second factor ready to hand to `craft_cloud_meta`: a recovery code as its hash, a TOTP code as the check that opens
/// the account's stored secret (under the account's lock, in the transaction) and compares.
pub(crate) enum Prepared {
    Totp(Box<TotpVerifier<'static>>),
    Recovery([u8; 32]),
}

impl Prepared {
    pub(crate) fn proof(&self) -> Proof<'_> {
        match self {
            Prepared::Totp(verify) => Proof::Totp(&**verify),
            Prepared::Recovery(hash) => Proof::Recovery(*hash),
        }
    }
}

impl ProofInput {
    pub(crate) fn prepare(self, state: &AppState, now_ms: i64) -> Prepared {
        match self {
            ProofInput::Recovery(hash) => Prepared::Recovery(hash),
            ProofInput::Totp(code) => Prepared::Totp(totp_check(state.two_factor.clone(), code, now_ms)),
        }
    }
}

/// The TOTP check for `code` at `now_ms`: opens the stored secret (sealed for the account in question), compares, and
/// hands back the time step. No key, or a secret the key doesn't open, is [`UnreadableSecret`].
fn totp_check(secrets: Option<Arc<SecretBox>>, code: String, now_ms: i64) -> Box<TotpVerifier<'static>> {
    Box::new(move |user: UserId, sealed: &[u8]| {
        let secrets = secrets.as_ref().ok_or(UnreadableSecret)?;
        let secret = secrets.open(user, sealed).map_err(|_| UnreadableSecret)?;
        Ok(totp::verify(&secret, &code, now_ms))
    })
}

fn random_failed(e: getrandom::Error) -> ApiError {
    tracing::error!(error = %e, "the system random source failed");
    ApiError::internal()
}

fn sealing_failed(_: crate::secretbox::SecretBoxError) -> ApiError {
    tracing::error!("a two-factor secret could not be sealed");
    ApiError::internal()
}

/// Ten new, distinct recovery codes.
fn new_recovery_codes() -> Result<Vec<Code>, ApiError> {
    let mut codes: Vec<Code> = Vec::with_capacity(RECOVERY_CODES);
    while codes.len() < RECOVERY_CODES {
        let code = Code::generate(INVITE_CHARS).map_err(random_failed)?;
        // A repeated code is all but impossible (80 bits), and is drawn again if it comes.
        if !codes.iter().any(|known| known.hash() == code.hash()) {
            codes.push(code);
        }
    }
    Ok(codes)
}

/// The codes as shown once, and their hashes as stored.
fn shown(codes: &[Code]) -> (RecoveryCodes, Vec<[u8; 32]>) {
    (RecoveryCodes { recovery_codes: codes.iter().map(Code::display).collect() }, codes.iter().map(Code::hash).collect())
}

fn no_second_factor() -> ApiError {
    ApiError::invalid("send a code in totp_code or a recovery code in recovery_code")
}

// ---- the account's own routes ----

/// `GET /api/v1/account/two-factor`.
pub async fn get_status(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>) -> Result<Json<TwoFactorStatus>, ApiError> {
    let info = state.meta.two_factor_info(auth.user, state.now_ms()).await?;
    Ok(Json(TwoFactorStatus {
        enabled: info.enabled_ms.is_some(),
        enabled_ms: info.enabled_ms,
        recovery_codes_left: info.recovery_codes_left,
        pending: info.pending,
    }))
}

/// `POST /api/v1/account/two-factor/setup`: the password starts a set-up and gets the new secret, shown once. Nothing is on
/// until `confirm`.
pub async fn setup(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    request: Request,
) -> Result<Json<TwoFactorSetupResponse>, ApiError> {
    let req: TwoFactorSetupRequest = read_tiny(&state, request).await?;
    let secrets = state.two_factor.clone().ok_or_else(ApiError::two_factor_unavailable)?;
    let password_hash = confirm_password(&state, &auth, req.password).await?;
    let secret = totp::generate_secret().map_err(random_failed)?;
    let sealed = secrets.seal(auth.user, secret.as_slice()).map_err(sealing_failed)?;
    let actor = DeviceActor { user: auth.user, device: auth.device, password_hash };
    let expires_ms = state.meta.two_factor_begin_setup(&actor, &sealed, state.now_ms()).await?;
    let encoded = totp::base32_encode(secret.as_slice());
    tracing::info!(device = %auth.device, "two-factor set-up started");
    Ok(Json(TwoFactorSetupResponse { otpauth_uri: totp::otpauth_uri(&auth.username, &encoded), secret: encoded, expires_ms }))
}

/// `POST /api/v1/account/two-factor/confirm`: the first code turns two-factor sign-in on and shows the recovery codes,
/// once. Every other device is signed out.
pub async fn confirm(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>, request: Request) -> Result<Json<RecoveryCodes>, ApiError> {
    let req: TwoFactorConfirmRequest = read_tiny(&state, request).await?;
    let Some(ProofInput::Totp(code)) = parse_proof(Some(req.totp_code), None)? else { return Err(no_second_factor()) };
    let now_ms = state.now_ms();
    let verify = totp_check(state.two_factor.clone(), code, now_ms);
    let (answer, hashes) = shown(&new_recovery_codes()?);
    let revoked = state.meta.two_factor_confirm(auth.user, auth.device, &*verify, &hashes, now_ms).await?;
    // Only after the commit: their live connections end now, not at their next request.
    end_live_connections(&state, &revoked);
    tracing::info!(device = %auth.device, revoked = revoked.len(), "two-factor sign-in turned on");
    Ok(Json(answer))
}

/// `POST /api/v1/account/two-factor/disable`: the password and a code or recovery code turn it off. Every other device is
/// signed out.
pub async fn disable(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>, request: Request) -> Result<StatusCode, ApiError> {
    let req: TwoFactorProofRequest = read_tiny(&state, request).await?;
    let proof = parse_proof(req.totp_code, req.recovery_code)?.ok_or_else(no_second_factor)?;
    let password_hash = confirm_password(&state, &auth, req.password).await?;
    let now_ms = state.now_ms();
    let prepared = proof.prepare(&state, now_ms);
    let actor = DeviceActor { user: auth.user, device: auth.device, password_hash };
    let revoked = state.meta.two_factor_disable(&actor, &prepared.proof(), now_ms).await?;
    end_live_connections(&state, &revoked);
    tracing::info!(device = %auth.device, revoked = revoked.len(), "two-factor sign-in turned off");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/account/two-factor/recovery-codes`: the password and a code make ten new recovery codes, shown once; the
/// old ones stop working.
pub async fn recovery_codes(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    request: Request,
) -> Result<Json<RecoveryCodes>, ApiError> {
    let req: TwoFactorProofRequest = read_tiny(&state, request).await?;
    if req.recovery_code.is_some() {
        return Err(ApiError::invalid("new recovery codes are made with a code from the authenticator app (totp_code)"));
    }
    let proof = parse_proof(req.totp_code, None)?.ok_or_else(no_second_factor)?;
    let password_hash = confirm_password(&state, &auth, req.password).await?;
    let now_ms = state.now_ms();
    let prepared = proof.prepare(&state, now_ms);
    let (answer, hashes) = shown(&new_recovery_codes()?);
    let actor = DeviceActor { user: auth.user, device: auth.device, password_hash };
    state.meta.two_factor_new_recovery_codes(&actor, &prepared.proof(), &hashes, now_ms).await?;
    tracing::info!(device = %auth.device, "new recovery codes made");
    Ok(Json(answer))
}

fn end_live_connections(state: &AppState, devices: &[craft_cloud_proto::DeviceId]) {
    for device in devices {
        state.live.revoke(*device);
    }
}

// ---- the second step of a sign-in ----

/// The hash of a challenge token as the client sent it; `None` for text that is no token (it is answered like an unknown
/// challenge).
fn challenge_hash(challenge: &str) -> Option<[u8; 32]> {
    let is_token = challenge.len() == 43 && URL_SAFE_NO_PAD.decode(challenge).is_ok();
    is_token.then(|| token_hash(challenge))
}

/// `POST /api/v1/auth/login/second-factor` (no sign-in): the challenge `POST /auth/login` gave and a code or recovery
/// code make the device, answered as a sign-in is. A challenge that is unknown, expired, used, from before a password
/// change or a reset, or fit no more is `401 unauthorized`, the same for each. A wrong code is `401 two_factor_invalid`
/// (five of them delete the challenge), a lock `429 two_factor_locked`.
pub async fn second_factor(
    State(state): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<LoginResponse>, ApiError> {
    let req: SecondFactorRequest = read_tiny(&state, request).await?;
    let proof = parse_proof(req.totp_code, req.recovery_code)?.ok_or_else(no_second_factor)?;
    // tokio's clock: the system's monotonic clock in the server, the simulated one in a simulation.
    let now = tokio::time::Instant::now().into_std();
    let now_ms = state.now_ms();
    let ip = ip_key(client_ip(peer, &headers, &state.trusted_proxies));
    // Counted before anything is looked at, so tries at the same moment are held to the limits too, and given back for a
    // right proof or a lock (which checked nothing).
    let Some(ip_counted) = state.second_factors.count(&ip, SECOND_FACTORS_PER_IP, now) else {
        return Err(ApiError::rate_limited("too many wrong codes from this address; wait a few minutes", None));
    };
    let gone = || ApiError::from(MetaError::ChallengeGone);
    let Some(hash) = challenge_hash(&req.challenge) else {
        crate::audit::challenge_failed(&state, None).await;
        return Err(gone());
    };
    let Some((user, username)) = state.meta.challenge_owner(&hash, now_ms).await? else {
        crate::audit::challenge_failed(&state, None).await;
        return Err(gone());
    };
    let user_key = format!("user:{username}");
    let Some(user_counted) = state.second_factors.count(&user_key, SECOND_FACTORS_PER_USER, now) else {
        state.second_factors.refund(&ip, ip_counted);
        // Ten wrong codes both fill this window and lock the account: say the lock, which tells how long.
        if let Some(locked_ms) = state.meta.two_factor_locked_for(user, now_ms).await? {
            return Err(ApiError::two_factor_locked(craft_cloud_meta::retry_after_secs(locked_ms)));
        }
        return Err(ApiError::rate_limited("too many wrong codes for this account; wait a few minutes", None));
    };
    let (token, new_token_hash) = new_token()?;
    let prepared = proof.prepare(&state, now_ms);
    let result = state.meta.complete_second_factor(&hash, &prepared.proof(), &new_token_hash, now_ms).await;
    match result {
        Ok(done) => {
            state.second_factors.refund(&ip, ip_counted);
            state.second_factors.refund(&user_key, user_counted);
            // The username is the only personal identifier the server could log here: kept to debug.
            tracing::debug!(%username, "sign-in with a second factor");
            tracing::info!(device = %done.device, "device signed in with a second factor");
            Ok(Json(LoginResponse { device: done.device, token }))
        }
        Err(MetaError::TwoFactorInvalid) => {
            // A wrong second factor counts against sign-in too.
            let _ = state.logins.allow(&ip, LOGINS_PER_IP, now);
            let _ = state.logins.allow(&user_key, LOGINS_PER_USER, now);
            Err(ApiError::from(MetaError::TwoFactorInvalid).on_public_route())
        }
        Err(MetaError::TwoFactorLocked { retry_after_ms }) => {
            state.second_factors.refund(&ip, ip_counted);
            state.second_factors.refund(&user_key, user_counted);
            Err(ApiError::from(MetaError::TwoFactorLocked { retry_after_ms }))
        }
        Err(MetaError::ChallengeGone) => {
            // The challenge was found a moment ago and has gone since (used, out of tries, expired, or the account changed).
            crate::audit::challenge_failed(&state, Some(user)).await;
            Err(gone())
        }
        Err(e) => Err(e.into()),
    }
}

// ---- the admin's confirmation ----

/// The second factor of an admin's confirmation (see [`crate::admin`]): when the admin's account has two-factor sign-in,
/// `proof` is checked and used up (`403 two_factor_invalid` when it is missing, wrong or a time step that was used, `429
/// two_factor_locked` while locked), counted against [`AppState::second_factors`] for the account. Whether the account
/// has it is read again under its lock when the proof is checked. The answer says whether it had: the change compares
/// that with the account's state in its own transaction.
pub(crate) async fn confirm_admin_second_factor(state: &AppState, auth: &DeviceAuth, proof: Option<ProofInput>) -> Result<bool, ApiError> {
    let enabled = state.meta.two_factor_enabled(auth.user).await?;
    if !enabled {
        // A code given to an account that has none is not looked at: the change compares the account's state again anyway.
        return Ok(false);
    }
    let Some(proof) = proof else {
        let mut e = ApiError::two_factor_invalid();
        e.message = "this account has two-factor sign-in: add a code in totp_code (or a recovery code in recovery_code)".to_owned();
        return Err(e);
    };
    let now = tokio::time::Instant::now().into_std();
    let key = format!("user:{}", auth.username);
    let now_ms = state.now_ms();
    let Some(counted) = state.second_factors.count(&key, SECOND_FACTORS_PER_USER, now) else {
        if let Some(locked_ms) = state.meta.two_factor_locked_for(auth.user, now_ms).await? {
            return Err(ApiError::two_factor_locked(craft_cloud_meta::retry_after_secs(locked_ms)));
        }
        return Err(ApiError::rate_limited("too many wrong codes; wait a few minutes", None));
    };
    let prepared = proof.prepare(state, now_ms);
    match state.meta.confirm_second_factor(auth.user, Some(&prepared.proof()), now_ms).await {
        Ok(had) => {
            state.second_factors.refund(&key, counted);
            Ok(had)
        }
        Err(MetaError::TwoFactorInvalid) => Err(MetaError::TwoFactorInvalid.into()),
        Err(e) => {
            state.second_factors.refund(&key, counted);
            Err(e.into())
        }
    }
}

// ---- the key ----

/// Where the server's key for two-factor secrets stands, from its config.
#[derive(Debug)]
pub enum KeyState {
    /// The config has no `[two_factor]` section.
    NotConfigured,
    /// The config names a key file that is not there.
    Missing(PathBuf),
    /// The key (and the older one, if named) is read.
    Loaded(Box<SecretBox>),
}

/// Reads the key files the config names. A key file that is not there is [`KeyState::Missing`] (whether that is fatal
/// depends on whether the database holds secrets, see [`check_startup`]); one that can't be read, has the wrong length, or
/// shares its name with the older key is an error, naming the file.
pub fn load_keys(config: &Config) -> Result<KeyState, KeyError> {
    let Some(section) = &config.two_factor else { return Ok(KeyState::NotConfigured) };
    let path = config.key_path(&section.key_file);
    let current = match TwoFactorKey::from_file(&path) {
        Ok(key) => key,
        Err(KeyError::Missing { path }) => return Ok(KeyState::Missing(path)),
        Err(e) => return Err(e),
    };
    warn_if_readable_by_others(&path);
    let old = match &section.key_file_old {
        Some(old) => {
            let old = config.key_path(old);
            warn_if_readable_by_others(&old);
            Some(TwoFactorKey::from_file(&old)?)
        }
        None => None,
    };
    Ok(KeyState::Loaded(Box::new(SecretBox::new(&current, old.as_ref())?)))
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(path = %path.display(), "the two-factor key file is readable by other users: make it owner-only (chmod 600)");
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &std::path::Path) {}

/// Refuses to start a server that holds two-factor secrets it can't open: with secrets in the database and no key (not
/// configured, or the file is missing), or a key that doesn't open **every** one of them. The message names the key file
/// the config expects. Without secrets there is nothing to open, and a server without a key runs, with set-up answering `503
/// two_factor_unavailable`. Returns the box to give [`AppState::with_two_factor`].
pub async fn check_startup(meta: &Meta, keys: KeyState, config: &Config, now_ms: i64) -> Result<Option<SecretBox>, String> {
    let secrets = meta.two_factor_secrets(now_ms).await.map_err(|e| e.to_string())?;
    let active: Vec<_> = secrets.iter().filter_map(|row| row.active.as_ref().map(|sealed| (row.user, sealed))).collect();
    let expected = config.two_factor.as_ref().map(|section| config.key_path(&section.key_file));
    match keys {
        KeyState::Loaded(secret_box) => {
            let unreadable = active.iter().filter(|(user, sealed)| secret_box.open(*user, sealed).is_err()).count();
            if unreadable > 0 {
                let path = expected.map(|p| p.display().to_string()).unwrap_or_default();
                return Err(format!(
                    "{unreadable} of {} two-factor secrets in the database can't be opened with the key in {path}{}: this is not the key they were sealed \
                     with. Put the right key there (and a previous one in key_file_old while rotating); the server won't start without it",
                    active.len(),
                    if config.two_factor.as_ref().is_some_and(|section| section.key_file_old.is_some()) { " or key_file_old" } else { "" },
                ));
            }
            Ok(Some(*secret_box))
        }
        KeyState::Missing(path) => {
            if active.is_empty() {
                tracing::warn!(path = %path.display(), "the two-factor key file is missing: two-factor sign-in can't be set up");
                Ok(None)
            } else {
                Err(format!(
                    "{} accounts use two-factor sign-in but the key file {} does not exist: restore it (it is not part of the database), or the server won't start",
                    active.len(),
                    path.display()
                ))
            }
        }
        KeyState::NotConfigured => {
            if active.is_empty() {
                Ok(None)
            } else {
                Err(format!(
                    "{} accounts use two-factor sign-in but the config has no [two_factor] key_file: set it to the key file the server had (it is not part of the \
                     database), or the server won't start",
                    active.len()
                ))
            }
        }
    }
}

/// What [`rekey`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rekeyed {
    /// Secrets rewritten under the current key.
    pub rewritten: usize,
    /// Secrets that were under it already.
    pub already_current: usize,
}

/// Rewrites every secret (the active ones and the set-ups', expired ones too) that is sealed with the older key under the
/// current one, one account at a time, each in a transaction that reads the account's ciphertexts fresh under its lock and
/// rewrites both, so a confirmation that races it can't leave one behind. It refuses, before changing anything, when a secret
/// is sealed with a key that is neither (a key that was lost) or can't be opened. It then scans again and goes round again
/// (at most five times) until no ciphertext is under any key but the current one, and fails if some still is: only a
/// successful return means `key_file_old` can be removed **and** that no server still running holds only the old key, which
/// is the operator's to see to (every instance must run with both keys before this, see `docs/server.md`). Neither the
/// secrets nor the keys are printed. It can be run again.
pub async fn rekey(meta: &Meta, secret_box: &SecretBox) -> Result<Rekeyed, String> {
    rekey_hooked(meta, secret_box, &|| {}).await
}

/// [`rekey`] with a test seam: `after_scan` is called once, after the first scan of what is stored and before anything is
/// rewritten, so a test can let other changes (a confirmation moving a set-up's secret) land exactly there. It blocks the
/// thread it runs on, so a test runs the rest on others.
pub async fn rekey_hooked(meta: &Meta, secret_box: &SecretBox, after_scan: &(dyn Fn() + Sync)) -> Result<Rekeyed, String> {
    let neither = || {
        "some secrets are sealed with a key that is neither key_file nor key_file_old: nothing was changed; put that key in key_file_old first"
            .to_owned()
    };
    let mut already_current = 0usize;
    for row in meta.two_factor_all_secrets().await.map_err(|e| e.to_string())? {
        for sealed in [row.active, row.pending].into_iter().flatten() {
            let id = SecretBox::key_id(&sealed);
            if id == Some(secret_box.current_id()) {
                // Under the current key: it must open, or the rest of the work would hide a lost key.
                secret_box.open(row.user, &sealed).map_err(|_| "a secret that carries the current key's name does not open with it".to_owned())?;
                already_current += 1;
            } else if id.is_some() && id == secret_box.old_id() {
                secret_box.open(row.user, &sealed).map_err(|_| "a secret does not open with key_file_old: nothing was changed".to_owned())?;
            } else {
                return Err(neither());
            }
        }
    }
    after_scan();
    let current = secret_box.current_id();
    let rewrite = |user: UserId, sealed: &[u8]| -> Result<Option<Vec<u8>>, UnreadableSecret> {
        if SecretBox::key_id(sealed) == Some(current) {
            return Ok(None);
        }
        let secret = secret_box.open(user, sealed).map_err(|_| UnreadableSecret)?;
        secret_box.seal(user, &secret).map(Some).map_err(|_| UnreadableSecret)
    };
    let mut rewritten = 0usize;
    for _round in 0..5 {
        let behind: Vec<UserId> = meta
            .two_factor_all_secrets()
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|row| [&row.active, &row.pending].into_iter().flatten().any(|sealed| SecretBox::key_id(sealed) != Some(current)))
            .map(|row| row.user)
            .collect();
        if behind.is_empty() {
            return Ok(Rekeyed { rewritten, already_current });
        }
        for user in behind {
            let (done, _) = meta.two_factor_rewrite(user, &rewrite).await.map_err(|e| e.to_string())?;
            rewritten += done;
        }
    }
    // One last look: a secret still under another key means the migration is not complete.
    let left = meta
        .two_factor_all_secrets()
        .await
        .map_err(|e| e.to_string())?
        .iter()
        .flat_map(|row| [&row.active, &row.pending])
        .flatten()
        .filter(|sealed| SecretBox::key_id(sealed) != Some(current))
        .count();
    if left == 0 {
        Ok(Rekeyed { rewritten, already_current })
    } else {
        Err(format!(
            "{left} secret(s) are still sealed with another key after rewriting: a server may still be sealing new ones with the old key. \
             Restart every instance with key_file and key_file_old, and run this again; do not remove key_file_old yet"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proof_has_the_shape_of_one_code_only() {
        assert!(matches!(parse_proof(None, None), Ok(None)));
        assert!(matches!(parse_proof(Some("123456".into()), None), Ok(Some(ProofInput::Totp(c))) if c == "123456"));
        for bad in ["12345", "1234567", "abcdef", " 12345", "١٢٣٤٥٦", ""] {
            assert!(parse_proof(Some(bad.into()), None).is_err(), "{bad:?}");
        }
        assert!(parse_proof(Some("123456".into()), Some("ABCD-EFGH-JKMN-PQRS".into())).is_err(), "both");
        let a = parse_proof(None, Some("abcd-efgh-jkmn-pqrs".into())).unwrap();
        let b = parse_proof(None, Some("ABCDEFGHJKMNPQRS".into())).unwrap();
        let (Some(ProofInput::Recovery(a)), Some(ProofInput::Recovery(b))) = (a, b) else { panic!("recovery codes") };
        assert_eq!(a, b, "case and dashes don't matter");
        // O is 0, and I and L are 1, as in the join codes.
        let zero = parse_proof(None, Some("0000-0000-0000-0000".into())).unwrap();
        let letter = parse_proof(None, Some("OOOO-OOOO-OOOO-OOOO".into())).unwrap();
        let (Some(ProofInput::Recovery(zero)), Some(ProofInput::Recovery(letter))) = (zero, letter) else { panic!("recovery codes") };
        assert_eq!(zero, letter);
        for bad in ["ABCD-EFGH-JKMN-PQR", "ABCD-EFGH-JKMN-PQRSA", "ABCD-EFGH-JKMN-PQRU", ""] {
            assert!(parse_proof(None, Some(bad.into())).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ten_distinct_recovery_codes_of_16_characters_are_shown_in_groups() {
        let codes = new_recovery_codes().unwrap();
        assert_eq!(codes.len(), 10);
        let (answer, hashes) = shown(&codes);
        assert_eq!(answer.recovery_codes.len(), 10);
        let unique: std::collections::BTreeSet<_> = hashes.iter().collect();
        assert_eq!(unique.len(), 10);
        for (shown, code) in answer.recovery_codes.iter().zip(&codes) {
            assert_eq!(shown.len(), 19, "16 characters in four groups");
            assert_eq!(shown.replace('-', ""), code.as_str());
            assert_eq!(Code::parse(shown, INVITE_CHARS).map(|c| c.hash()), Some(code.hash()));
        }
        // The hash is BLAKE3 of the normalised code, as the join codes' are, and reproducible.
        assert_eq!(hashes.first().copied(), codes.first().map(|c| *blake3::hash(c.as_str().as_bytes()).as_bytes()));
    }

    #[test]
    fn a_challenge_is_a_43_character_token_or_nothing() {
        let (token, hash) = new_token().unwrap();
        assert_eq!(challenge_hash(&token), Some(hash));
        for bad in ["", "short", &"x".repeat(44), &format!("{}=", &token[..42]), &format!("{}!", &token[..42])] {
            assert_eq!(challenge_hash(bad), None, "{bad:?}");
        }
    }
}
