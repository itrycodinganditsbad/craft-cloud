//! Two-factor sign-in: the TOTP secret (stored encrypted, the server's `secretbox` makes and opens the ciphertext), the
//! recovery codes (stored as BLAKE3 hashes), the lock after wrong codes, and the challenges of a password sign-in that
//! still needs its second factor.
//!
//! - **One transaction contract for everything that issues a device or consumes a proof.** The account's row is locked
//!   first (`users::lock_user`, the order every other transaction on the account uses), then what the caller checked
//!   before is read again under the lock (the device still signed in, the password still the one that was verified,
//!   the account's current two-factor state, the challenge), and only then is a challenge or a device made. A second
//!   factor and the challenge or invite it opens are used up in the same transaction as the device they let in: the
//!   proof is never spent by one commit and followed by another.
//! - **Proofs are single use.** A TOTP code is accepted for a time step greater than the last accepted one
//!   (`last_accepted_step`, updated with `WHERE last_accepted_step < step`), so two requests with one code can't both
//!   succeed, and a second confirmation in the same 30-second step needs the next code. Sign-in and the admin's
//!   confirmations share that counter. A recovery code is a row updated once (`WHERE used_ms IS NULL`).
//! - **Wrong proofs are counted in the transaction that refused them, under the account's lock.** Every check runs before
//!   anything is consumed, so the refusing transaction commits only the counters (the invite stays, the challenge stays,
//!   no device), and the next request, which waited for the lock, sees the attempts and the lock this one made. After
//!   [`LOCK_AFTER_FAILURES`] wrong proofs in a
//!   row the second factor is locked for [`LOCK_BASE_MS`], and each wrong proof after that doubles the time up to
//!   [`LOCK_MAX_MS`]. A lock is never for good. While it lasts every check answers [`MetaError::TwoFactorLocked`]
//!   without counting.
//! - **Lock order** inside a transaction: the account's row, then its two-factor row, then challenges (the failed-proof
//!   write takes the last two in that order and never the account's row, so it can't deadlock with them).
//! - **Turning it on or off, and a reset, end what an earlier check let through**: `users.auth_gen` goes up, every
//!   open challenge, invite and pairing of the account is deleted, and the devices are signed out (all of them for a
//!   disable made by someone else, a reset or the command line; all but the asking device for the account's own).

use craft_cloud_proto::DeviceId;
use sqlx::Row;

use crate::audit::{AuditAction, AuditDetails, AuditFailure, NewEvent, Step as AuditStep, record_in};
use crate::codec::{uuid_bytes, uuid_from};
use crate::users::{DeviceClient, account_blocked, check_device_name, delete_join_codes, insert_device, lock_user, revoke_devices};
use crate::{Meta, MetaError, RevokeReason, UserId, WriteTx};

/// How long a set-up stays open: ten minutes.
pub const SETUP_TTL_MS: i64 = 10 * 60 * 1000;
/// Wrong codes one set-up takes before it is used up.
pub const MAX_SETUP_ATTEMPTS: i64 = 5;
/// How long a challenge works: five minutes.
pub const CHALLENGE_TTL_MS: i64 = 5 * 60 * 1000;
/// Wrong codes one challenge takes before it is deleted.
pub const MAX_CHALLENGE_ATTEMPTS: i64 = 5;
/// Open challenges one account may have; making another replaces the oldest.
pub const MAX_OPEN_CHALLENGES: i64 = 5;
/// Wrong proofs in a row that lock the second factor.
pub const LOCK_AFTER_FAILURES: i64 = 10;
/// The first lock: 15 minutes. Every wrong proof after a lock ran out doubles it…
pub const LOCK_BASE_MS: i64 = 15 * 60 * 1000;
/// …up to 24 hours.
pub const LOCK_MAX_MS: i64 = 24 * 60 * 60 * 1000;
/// Recovery codes made at once.
pub const RECOVERY_CODES: usize = 10;

/// A stored secret that the caller's key could not open (the wrong key, a damaged value, another account's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnreadableSecret;

/// Checks a TOTP code against an account's stored secret (the ciphertext, which only the caller can open): the time step
/// of the code, if it is right for a step in the window, else `None`. Called with the transaction open, on the
/// ciphertext that is current under the account's lock.
pub type TotpVerifier<'a> = dyn Fn(UserId, &[u8]) -> Result<Option<i64>, UnreadableSecret> + Send + Sync + 'a;

/// Rewrites one stored ciphertext of an account: the one to store in its place, or `None` to leave it ([`Meta::two_factor_rewrite`]).
pub type Rewrite<'a> = dyn Fn(UserId, &[u8]) -> Result<Option<Vec<u8>>, UnreadableSecret> + Send + Sync + 'a;

/// A second factor a caller presents.
pub enum Proof<'a> {
    /// A TOTP code, as the check that knows it.
    Totp(&'a TotpVerifier<'a>),
    /// The BLAKE3 hash of a normalised recovery code.
    Recovery([u8; 32]),
}

/// The signed-in device that asks for a change to its account's two-factor sign-in, as the handler knew it when it
/// verified the password: all of it is checked again under the account's lock.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceActor {
    pub user: UserId,
    /// The device the request came with; it must still be signed in.
    pub device: DeviceId,
    /// The stored password hash the password was verified against; it must still be the account's.
    pub password_hash: String,
}

impl std::fmt::Debug for DeviceActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceActor").field("user", &self.user).field("device", &self.device).field("password_hash", &"<redacted>").finish()
    }
}

/// What `GET /account/two-factor` tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TwoFactorInfo {
    pub enabled_ms: Option<i64>,
    pub recovery_codes_left: u32,
    /// A set-up is open (not expired, not used up).
    pub pending: bool,
}

/// A new challenge for a password sign-in that needs a second factor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChallenge {
    /// The BLAKE3 hash of the challenge token: all the server keeps of it.
    pub id_hash: [u8; 32],
    /// The client address the challenge was made for. Only an additional binding: behind a proxy that hides addresses
    /// every client has the same one, so the secret token is what binds the challenge.
    pub client_key: String,
}

/// The result of a password sign-in that checked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignIn {
    /// The account has no second factor: the device is made.
    Device(DeviceId),
    /// The account needs its second factor: the challenge is made and expires at this time. No device.
    Challenge { expires_ms: i64 },
}

/// What [`Meta::complete_second_factor`] made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completed {
    pub user: UserId,
    pub device: DeviceId,
}

/// A stored secret for the key tools: the ciphertexts of one account. Its `Debug` output shows only their lengths.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredSecrets {
    pub user: UserId,
    /// The ciphertext of the active secret, if two-factor sign-in is on.
    pub active: Option<Vec<u8>>,
    /// The ciphertext of an open set-up's secret, if there is one that is not expired.
    pub pending: Option<Vec<u8>>,
}

impl std::fmt::Debug for StoredSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredSecrets")
            .field("user", &self.user)
            .field("active", &self.active.as_ref().map(|sealed| format!("<{} bytes>", sealed.len())))
            .field("pending", &self.pending.as_ref().map(|sealed| format!("<{} bytes>", sealed.len())))
            .finish()
    }
}

/// The row of `user_two_factor`.
pub(crate) struct TfRow {
    enabled_ms: Option<i64>,
    secret: Option<Vec<u8>>,
    pending: Option<Vec<u8>>,
    pending_device: Option<Vec<u8>>,
    pending_expires_ms: Option<i64>,
    last_step: i64,
    locked_until_ms: Option<i64>,
}

impl TfRow {
    /// Two-factor sign-in is on.
    pub(crate) fn active(&self) -> bool {
        self.enabled_ms.is_some() && self.secret.is_some()
    }
}

pub(crate) async fn load_row(tx: &mut sqlx::Transaction<'static, sqlx::Any>, user: UserId) -> Result<Option<TfRow>, MetaError> {
    let row = sqlx::query(
        "SELECT enabled_ms, secret_ciphertext, pending_ciphertext, pending_device, pending_expires_ms, last_accepted_step, \
         locked_until_ms FROM user_two_factor WHERE user_id = $1",
    )
    .bind(uuid_bytes(&user.0))
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some(TfRow {
        enabled_ms: row.try_get("enabled_ms")?,
        secret: row.try_get("secret_ciphertext")?,
        pending: row.try_get("pending_ciphertext")?,
        pending_device: row.try_get("pending_device")?,
        pending_expires_ms: row.try_get("pending_expires_ms")?,
        last_step: row.try_get("last_accepted_step")?,
        locked_until_ms: row.try_get("locked_until_ms")?,
    }))
}

/// Whether two-factor sign-in is on for `user`, read in `tx`.
pub(crate) async fn is_enabled(tx: &mut sqlx::Transaction<'static, sqlx::Any>, user: UserId) -> Result<bool, MetaError> {
    Ok(load_row(tx, user).await?.is_some_and(|row| row.active()))
}

/// How long the second factor stays locked after the `failures`-th wrong proof in a row (`None`: no lock yet).
fn lock_for(failures: i64) -> Option<i64> {
    if failures < LOCK_AFTER_FAILURES {
        return None;
    }
    let doublings = u32::try_from(failures.saturating_sub(LOCK_AFTER_FAILURES)).unwrap_or(u32::MAX).min(16);
    Some(LOCK_BASE_MS.saturating_mul(1i64 << doublings).min(LOCK_MAX_MS))
}

/// Seconds a client should wait before trying again, for a `Retry-After` header: rounded up, at least one.
pub fn retry_after_secs(retry_after_ms: i64) -> u32 {
    u32::try_from((retry_after_ms.max(0) + 999) / 1000).unwrap_or(u32::MAX).max(1)
}

pub(crate) enum Checked {
    Accepted,
    Wrong,
}

/// Checks `proof` against the account's two-factor row `row` and, when it is right, uses it up and clears the wrong-proof
/// counters, all in `tx` (the account's row is locked). A lock that is in force is [`MetaError::TwoFactorLocked`] and
/// nothing is checked or counted. A recovery code that is used up writes `auth.recovery_code_used` in `tx` (a code that
/// is not used up writes nothing); `device` is the device it was used from, where there is one.
pub(crate) async fn check_proof(
    tx: &mut sqlx::Transaction<'static, sqlx::Any>,
    user: UserId,
    device: Option<DeviceId>,
    row: &TfRow,
    proof: &Proof<'_>,
    now_ms: i64,
) -> Result<Checked, MetaError> {
    if let Some(until) = row.locked_until_ms
        && until > now_ms
    {
        return Err(MetaError::TwoFactorLocked { retry_after_ms: until - now_ms });
    }
    match proof {
        Proof::Totp(verify) => {
            let secret = row.secret.as_deref().ok_or(MetaError::Corrupt("a two-factor row has no secret"))?;
            let step = verify(user, secret).map_err(|_| MetaError::Corrupt("a stored two-factor secret can't be read"))?;
            // A step that was accepted before, or an older one, is a replay.
            let Some(step) = step.filter(|step| *step > row.last_step) else { return Ok(Checked::Wrong) };
            let done = sqlx::query(
                "UPDATE user_two_factor SET last_accepted_step = $1, failed_attempts = 0, locked_until_ms = NULL \
                 WHERE user_id = $2 AND last_accepted_step < $1",
            )
            .bind(step)
            .bind(uuid_bytes(&user.0))
            .execute(&mut **tx)
            .await?;
            Ok(if done.rows_affected() == 1 { Checked::Accepted } else { Checked::Wrong })
        }
        Proof::Recovery(hash) => {
            let done = sqlx::query("UPDATE user_recovery_codes SET used_ms = $1 WHERE user_id = $2 AND code_hash = $3 AND used_ms IS NULL")
                .bind(now_ms)
                .bind(uuid_bytes(&user.0))
                .bind(hash.to_vec())
                .execute(&mut **tx)
                .await?;
            if done.rows_affected() != 1 {
                return Ok(Checked::Wrong);
            }
            sqlx::query("UPDATE user_two_factor SET failed_attempts = 0, locked_until_ms = NULL WHERE user_id = $1")
                .bind(uuid_bytes(&user.0))
                .execute(&mut **tx)
                .await?;
            record_in(tx, now_ms, NewEvent::new(AuditAction::AuthRecoveryCodeUsed).by(user, device).on_user(user)).await?;
            Ok(Checked::Accepted)
        }
    }
}

/// Deletes the account's two-factor secret, recovery codes and open challenges, and raises `auth_gen`, in `tx`. Whether
/// there was anything to delete is the answer.
pub(crate) async fn remove_in(tx: &mut sqlx::Transaction<'static, sqlx::Any>, user: UserId) -> Result<bool, MetaError> {
    sqlx::query("DELETE FROM user_recovery_codes WHERE user_id = $1").bind(uuid_bytes(&user.0)).execute(&mut **tx).await?;
    let removed = sqlx::query("DELETE FROM user_two_factor WHERE user_id = $1").bind(uuid_bytes(&user.0)).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM login_challenges WHERE user_id = $1").bind(uuid_bytes(&user.0)).execute(&mut **tx).await?;
    sqlx::query("UPDATE users SET auth_gen = auth_gen + 1 WHERE id = $1").bind(uuid_bytes(&user.0)).execute(&mut **tx).await?;
    Ok(removed.rows_affected() > 0)
}

/// The account's two-factor sign-in removed by someone else (the command line, an admin): the secret, the codes and the
/// challenges go, every device is signed out ([`RevokeReason::TwoFactor`]) and the invite and pairings are deleted. The
/// devices signed out are returned, and the answer is `None` when the account had none (nothing is changed then).
pub(crate) async fn reset_in(tx: &mut sqlx::Transaction<'static, sqlx::Any>, user: UserId, now_ms: i64) -> Result<Option<Vec<DeviceId>>, MetaError> {
    if load_row(tx, user).await?.is_none() {
        return Ok(None);
    }
    remove_in(tx, user).await?;
    let revoked = revoke_devices(tx, user, None, RevokeReason::TwoFactor, now_ms).await?;
    delete_join_codes(tx, user).await?;
    Ok(Some(revoked))
}

/// Checks the device and the password a handler verified against what is stored now, under the account's lock.
async fn check_actor(w: &mut WriteTx, actor: &DeviceActor) -> Result<(), MetaError> {
    if !lock_user(&mut w.tx, actor.user).await? {
        return Err(MetaError::DeviceRevoked);
    }
    // The device must still be signed in; the no-op update locks its row too, so a revoke running now waits.
    let kept = sqlx::query("UPDATE devices SET revoked = 0 WHERE id = $1 AND user_id = $2 AND revoked = 0")
        .bind(uuid_bytes(actor.device.as_uuid()))
        .bind(uuid_bytes(&actor.user.0))
        .execute(&mut *w.tx)
        .await?;
    if kept.rows_affected() == 0 || account_blocked(&mut w.tx, actor.user).await? {
        return Err(MetaError::DeviceRevoked);
    }
    let same = sqlx::query("SELECT 1 FROM users WHERE id = $1 AND password_hash = $2")
        .bind(uuid_bytes(&actor.user.0))
        .bind(actor.password_hash.as_str())
        .fetch_optional(&mut *w.tx)
        .await?;
    if same.is_none() {
        return Err(MetaError::Conflict("the password changed while this request was running; try again"));
    }
    Ok(())
}

impl Meta {
    /// Whether `user` has two-factor sign-in on.
    pub async fn two_factor_enabled(&self, user: UserId) -> Result<bool, MetaError> {
        let row = sqlx::query("SELECT 1 FROM user_two_factor WHERE user_id = $1 AND enabled_ms IS NOT NULL")
            .bind(uuid_bytes(&user.0))
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// How many milliseconds `user`'s second factor stays locked after `now_ms` (`None`: it is not locked, or is off). A plain
    /// read, for a caller that wants to say "locked" rather than "too many tries".
    pub async fn two_factor_locked_for(&self, user: UserId, now_ms: i64) -> Result<Option<i64>, MetaError> {
        let row = sqlx::query("SELECT locked_until_ms FROM user_two_factor WHERE user_id = $1 AND enabled_ms IS NOT NULL")
            .bind(uuid_bytes(&user.0))
            .fetch_optional(&self.pool)
            .await?;
        let until: Option<i64> = row.map(|row| row.try_get("locked_until_ms")).transpose()?.flatten();
        Ok(until.filter(|until| *until > now_ms).map(|until| until - now_ms))
    }

    /// What `GET /account/two-factor` tells about `user`.
    pub async fn two_factor_info(&self, user: UserId, now_ms: i64) -> Result<TwoFactorInfo, MetaError> {
        let row = sqlx::query("SELECT enabled_ms, pending_ciphertext, pending_expires_ms FROM user_two_factor WHERE user_id = $1")
            .bind(uuid_bytes(&user.0))
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(TwoFactorInfo { enabled_ms: None, recovery_codes_left: 0, pending: false }) };
        let enabled_ms: Option<i64> = row.try_get("enabled_ms")?;
        let pending = row.try_get::<Option<Vec<u8>>, _>("pending_ciphertext")?.is_some()
            && row.try_get::<Option<i64>, _>("pending_expires_ms")?.is_some_and(|expires| expires > now_ms);
        let left = if enabled_ms.is_some() {
            let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM user_recovery_codes WHERE user_id = $1 AND used_ms IS NULL")
                .bind(uuid_bytes(&user.0))
                .fetch_one(&self.pool)
                .await?
                .try_get("n")?;
            u32::try_from(n).unwrap_or(u32::MAX)
        } else {
            0
        };
        Ok(TwoFactorInfo { enabled_ms, recovery_codes_left: left, pending })
    }

    /// Starts a set-up for `actor`'s account: `pending_ciphertext` (the new secret, sealed by the caller) is kept for
    /// [`SETUP_TTL_MS`] and bound to the asking device, and its expiry is returned. Only while two-factor sign-in is off
    /// (`Conflict` otherwise). A set-up that is open on another device is [`MetaError::TwoFactorPending`]; the same
    /// device may start again, which replaces the secret and clears its wrong codes. A device that was signed out is
    /// [`MetaError::DeviceRevoked`], a password that changed `Conflict`.
    pub async fn two_factor_begin_setup(&self, actor: &DeviceActor, pending_ciphertext: &[u8], now_ms: i64) -> Result<i64, MetaError> {
        let expires_ms = now_ms.checked_add(SETUP_TTL_MS).ok_or(MetaError::Invalid("the clock is out of range"))?;
        let mut w = self.begin_write().await?;
        check_actor(&mut w, actor).await?;
        if let Some(row) = load_row(&mut w.tx, actor.user).await? {
            if row.active() {
                return Err(MetaError::Conflict("two-factor sign-in is on already; turn it off first"));
            }
            let open = row.pending.is_some() && row.pending_expires_ms.is_some_and(|expires| expires > now_ms);
            if open && row.pending_device.as_deref() != Some(actor.device.as_uuid().as_bytes().as_slice()) {
                return Err(MetaError::TwoFactorPending);
            }
        }
        sqlx::query(
            "INSERT INTO user_two_factor (user_id, pending_ciphertext, pending_device, pending_expires_ms, pending_attempts) \
             VALUES ($1, $2, $3, $4, 0) \
             ON CONFLICT (user_id) DO UPDATE SET pending_ciphertext = $2, pending_device = $3, pending_expires_ms = $4, pending_attempts = 0",
        )
        .bind(uuid_bytes(&actor.user.0))
        .bind(pending_ciphertext.to_vec())
        .bind(uuid_bytes(actor.device.as_uuid()))
        .bind(expires_ms)
        .execute(&mut *w.tx)
        .await?;
        w.commit().await?;
        Ok(expires_ms)
    }

    /// Turns two-factor sign-in on with the first code of the set-up that `device` of `user` started: the code must be
    /// right for the open set-up's secret (at most [`MAX_SETUP_ATTEMPTS`] wrong ones, then the set-up is used up). In one
    /// transaction the pending secret becomes the active one, the account's recovery codes become `recovery_hashes`
    /// (`BLAKE3` hashes of the codes the caller shows once), `auth_gen` goes up, every other device is signed out
    /// ([`RevokeReason::TwoFactor`]) and the account's invite, pairings and challenges are deleted. Two confirmations
    /// at once turn it on once. Returns the devices signed out. No open set-up, one that was started on another device
    /// (the code is then not tried) or one that ran out is `Conflict`; a signed-out device [`MetaError::DeviceRevoked`];
    /// a wrong code [`MetaError::TwoFactorInvalid`].
    pub async fn two_factor_confirm(
        &self,
        user: UserId,
        device: DeviceId,
        verify: &TotpVerifier<'_>,
        recovery_hashes: &[[u8; 32]],
        now_ms: i64,
    ) -> Result<Vec<DeviceId>, MetaError> {
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Err(MetaError::DeviceRevoked);
        }
        let kept = sqlx::query("UPDATE devices SET revoked = 0 WHERE id = $1 AND user_id = $2 AND revoked = 0")
            .bind(uuid_bytes(device.as_uuid()))
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if kept.rows_affected() == 0 || account_blocked(&mut w.tx, user).await? {
            return Err(MetaError::DeviceRevoked);
        }
        let row = load_row(&mut w.tx, user).await?;
        let Some(row) = row.filter(|row| !row.active()) else {
            return Err(MetaError::Conflict("there is no two-factor set-up to confirm: start one first"));
        };
        let open = row.pending.is_some() && row.pending_expires_ms.is_some_and(|expires| expires > now_ms);
        if !open {
            return Err(MetaError::Conflict("the two-factor set-up ran out: start it again"));
        }
        if row.pending_device.as_deref() != Some(device.as_uuid().as_bytes().as_slice()) {
            return Err(MetaError::Conflict("this two-factor set-up was started on another device: confirm it there, or start it again here"));
        }
        let pending = row.pending.as_deref().ok_or(MetaError::Corrupt("an open set-up has no secret"))?;
        let step = verify(user, pending).map_err(|_| MetaError::Corrupt("a stored two-factor secret can't be read"))?;
        let Some(step) = step else {
            // Nothing was changed yet: the wrong code is counted and committed in this transaction, under the lock.
            count_wrong_setup_in(&mut w.tx, user, device).await?;
            w.commit().await?;
            return Err(MetaError::TwoFactorInvalid);
        };
        let done = sqlx::query(
            "UPDATE user_two_factor SET secret_ciphertext = pending_ciphertext, enabled_ms = $1, pending_ciphertext = NULL, pending_device = NULL, \
             pending_expires_ms = NULL, pending_attempts = 0, last_accepted_step = $2, failed_attempts = 0, locked_until_ms = NULL \
             WHERE user_id = $3 AND enabled_ms IS NULL AND pending_ciphertext IS NOT NULL",
        )
        .bind(now_ms)
        .bind(step)
        .bind(uuid_bytes(&user.0))
        .execute(&mut *w.tx)
        .await?;
        if done.rows_affected() != 1 {
            return Err(MetaError::Conflict("two-factor sign-in changed while this request was running; try again"));
        }
        replace_recovery_codes(&mut w.tx, user, recovery_hashes).await?;
        sqlx::query("UPDATE users SET auth_gen = auth_gen + 1 WHERE id = $1").bind(uuid_bytes(&user.0)).execute(&mut *w.tx).await?;
        sqlx::query("DELETE FROM login_challenges WHERE user_id = $1").bind(uuid_bytes(&user.0)).execute(&mut *w.tx).await?;
        let revoked = revoke_devices(&mut w.tx, user, Some(device), RevokeReason::TwoFactor, now_ms).await?;
        delete_join_codes(&mut w.tx, user).await?;
        record_in(&mut w.tx, now_ms, NewEvent::new(AuditAction::AccountTwoFactorEnabled).by(user, Some(device)).on_user(user)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(revoked)
    }

    /// Turns `actor`'s two-factor sign-in off with `proof` (a code or a recovery code): in one transaction the secret,
    /// the codes and the challenges go, `auth_gen` goes up, every other device is signed out ([`RevokeReason::TwoFactor`])
    /// and the invite and pairings are deleted. Returns the devices signed out. Off already is `Conflict`; a wrong
    /// proof [`MetaError::TwoFactorInvalid`] (counted, and it can lock); a lock in force [`MetaError::TwoFactorLocked`].
    pub async fn two_factor_disable(&self, actor: &DeviceActor, proof: &Proof<'_>, now_ms: i64) -> Result<Vec<DeviceId>, MetaError> {
        let mut w = self.begin_write().await?;
        check_actor(&mut w, actor).await?;
        let row = load_row(&mut w.tx, actor.user).await?.filter(TfRow::active);
        let Some(row) = row else { return Err(MetaError::Conflict("two-factor sign-in is not on")) };
        if let Checked::Wrong = check_proof(&mut w.tx, actor.user, Some(actor.device), &row, proof, now_ms).await? {
            return Err(self.refuse_proof(w, actor.user, None, now_ms).await);
        }
        remove_in(&mut w.tx, actor.user).await?;
        let revoked = revoke_devices(&mut w.tx, actor.user, Some(actor.device), RevokeReason::TwoFactor, now_ms).await?;
        delete_join_codes(&mut w.tx, actor.user).await?;
        let event = NewEvent::new(AuditAction::AccountTwoFactorDisabled).by(actor.user, Some(actor.device)).on_user(actor.user);
        record_in(&mut w.tx, now_ms, event).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(revoked)
    }

    /// Replaces `actor`'s recovery codes with `recovery_hashes` (the old ones stop working at once), after `proof`. Off
    /// is `Conflict`; the proof's errors are as for [`Meta::two_factor_disable`].
    pub async fn two_factor_new_recovery_codes(
        &self,
        actor: &DeviceActor,
        proof: &Proof<'_>,
        recovery_hashes: &[[u8; 32]],
        now_ms: i64,
    ) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        check_actor(&mut w, actor).await?;
        let row = load_row(&mut w.tx, actor.user).await?.filter(TfRow::active);
        let Some(row) = row else { return Err(MetaError::Conflict("two-factor sign-in is not on")) };
        if let Checked::Wrong = check_proof(&mut w.tx, actor.user, Some(actor.device), &row, proof, now_ms).await? {
            return Err(self.refuse_proof(w, actor.user, None, now_ms).await);
        }
        replace_recovery_codes(&mut w.tx, actor.user, recovery_hashes).await?;
        let event = NewEvent::new(AuditAction::AccountRecoveryCodesRegenerated).by(actor.user, Some(actor.device)).on_user(actor.user);
        record_in(&mut w.tx, now_ms, event).await?;
        self.commit_audited(w, now_ms).await
    }

    /// The confirmation an admin gives for a change: when `user` has two-factor sign-in on, `proof` is checked and used up
    /// (the time step or recovery code can't serve a second change), and the answer is `true`; when it is off, nothing is
    /// checked and the answer is `false`. Whether it is on is decided under the account's lock. A missing proof for an
    /// account that has it on is [`MetaError::TwoFactorInvalid`] (not counted: nothing was tried); the other errors are as
    /// for [`Meta::two_factor_disable`].
    pub async fn confirm_second_factor(&self, user: UserId, proof: Option<&Proof<'_>>, now_ms: i64) -> Result<bool, MetaError> {
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Err(MetaError::DeviceRevoked);
        }
        let Some(row) = load_row(&mut w.tx, user).await?.filter(TfRow::active) else { return Ok(false) };
        let Some(proof) = proof else { return Err(MetaError::TwoFactorInvalid) };
        if let Checked::Wrong = check_proof(&mut w.tx, user, None, &row, proof, now_ms).await? {
            return Err(self.refuse_proof(w, user, None, now_ms).await);
        }
        // The proof is used up here, in a transaction of its own that commits before the change it confirms: a recovery
        // code used for a change that then fails is still used, and its event stays.
        self.commit_audited(w, now_ms).await?;
        Ok(true)
    }

    /// Takes away `user`'s two-factor sign-in on the command line: the secret, the codes and the challenges go, the lock is
    /// gone with the row, every device is signed out ([`RevokeReason::TwoFactor`]) and the invite and pairings are
    /// deleted. Returns the devices signed out. `NotFound` for a user that isn't there, `Conflict` for one without
    /// two-factor sign-in (nothing is signed out then).
    pub async fn reset_two_factor(&self, user: UserId, now_ms: i64) -> Result<Vec<DeviceId>, MetaError> {
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Err(MetaError::NotFound("user"));
        }
        let revoked = reset_in(&mut w.tx, user, now_ms).await?.ok_or(MetaError::Conflict("this account has no two-factor sign-in"))?;
        let event = NewEvent::new(AuditAction::CliTwoFactorReset).on_user(user).with(AuditDetails::revoked(revoked.len()));
        record_in(&mut w.tx, now_ms, event).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(revoked)
    }

    /// A password sign-in that checked out, as [`Meta::sign_in_device`], for an account that may have two-factor sign-in:
    /// under the account's lock the password hash and `auth_gen` the caller read before verifying the password are
    /// compared again, then the account's **current** state decides. Without two-factor sign-in the device is made
    /// ([`SignIn::Device`]); with it only `challenge` is made ([`SignIn::Challenge`], good for [`CHALLENGE_TTL_MS`]; the
    /// account's expired challenges are deleted and the oldest goes when it would have more than
    /// [`MAX_OPEN_CHALLENGES`]) and no device. `None`, a disabled account and the errors are as for `sign_in_device`.
    #[allow(clippy::too_many_arguments)]
    pub async fn sign_in(
        &self,
        user: UserId,
        checked: (&str, i64),
        name: &str,
        client: &DeviceClient,
        token_hash: &[u8; 32],
        challenge: &NewChallenge,
        now_ms: i64,
    ) -> Result<Option<SignIn>, MetaError> {
        check_device_name(name)?;
        client.check()?;
        let (hash, auth_gen) = checked;
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Ok(None);
        }
        let same = sqlx::query("SELECT 1 FROM users WHERE id = $1 AND password_hash = $2 AND auth_gen = $3")
            .bind(uuid_bytes(&user.0))
            .bind(hash)
            .bind(auth_gen)
            .fetch_optional(&mut *w.tx)
            .await?;
        if same.is_none() {
            return Ok(None);
        }
        if account_blocked(&mut w.tx, user).await? {
            return Err(MetaError::AccountDisabled);
        }
        if is_enabled(&mut w.tx, user).await? {
            let expires_ms = now_ms.checked_add(CHALLENGE_TTL_MS).ok_or(MetaError::Invalid("the clock is out of range"))?;
            sqlx::query("DELETE FROM login_challenges WHERE expires_ms <= $1").bind(now_ms).execute(&mut *w.tx).await?;
            let open: i64 = sqlx::query("SELECT COUNT(*) AS n FROM login_challenges WHERE user_id = $1")
                .bind(uuid_bytes(&user.0))
                .fetch_one(&mut *w.tx)
                .await?
                .try_get("n")?;
            if open >= MAX_OPEN_CHALLENGES {
                sqlx::query(
                    "DELETE FROM login_challenges WHERE id_hash IN \
                     (SELECT id_hash FROM login_challenges WHERE user_id = $1 ORDER BY expires_ms ASC, id_hash ASC LIMIT $2)",
                )
                .bind(uuid_bytes(&user.0))
                .bind(open - MAX_OPEN_CHALLENGES + 1)
                .execute(&mut *w.tx)
                .await?;
            }
            sqlx::query(
                "INSERT INTO login_challenges (id_hash, user_id, kind, client_key, device_name, client_kind, platform, app_version, attempts, expires_ms) \
                 VALUES ($1, $2, 'login', $3, $4, $5, $6, $7, 0, $8)",
            )
            .bind(challenge.id_hash.to_vec())
            .bind(uuid_bytes(&user.0))
            .bind(challenge.client_key.as_str())
            .bind(name)
            .bind(client.kind.as_str())
            .bind(client.platform.as_str())
            .bind(client.app_version.as_str())
            .bind(expires_ms)
            .execute(&mut *w.tx)
            .await?;
            w.commit().await?;
            return Ok(Some(SignIn::Challenge { expires_ms }));
        }
        let device = DeviceId::generate();
        insert_device(&mut w.tx, device, user, name, client, token_hash, now_ms).await?;
        // A sign-in that made a device is a login; a challenge (above) is not: nobody is signed in yet.
        let login = NewEvent::new(AuditAction::AuthLogin).by(user, Some(device)).on_device(device).with(AuditDetails::client(client));
        record_in(&mut w.tx, now_ms, login).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(Some(SignIn::Device(device)))
    }

    /// The account a challenge was made for and its username, by a plain read: only so the caller can count the try against
    /// the account before the real check. `None` for a challenge that is unknown, expired or used.
    pub async fn challenge_owner(&self, id_hash: &[u8; 32], now_ms: i64) -> Result<Option<(UserId, String)>, MetaError> {
        let row = sqlx::query(
            "SELECT c.user_id, u.username FROM login_challenges c JOIN users u ON u.id = c.user_id WHERE c.id_hash = $1 AND c.expires_ms > $2",
        )
        .bind(id_hash.to_vec())
        .bind(now_ms)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some((UserId(uuid_from(&row.try_get::<Vec<u8>, _>("user_id")?)?), row.try_get("username")?)))
    }

    /// Finishes a sign-in with the second factor: the account is found by a plain read of the challenge, its row is locked
    /// **before** the challenge or the factor is touched, and under the lock the challenge (still there, not expired) and
    /// the account's **current** state (two-factor sign-in still on, not disabled) are checked again. Only then are the
    /// proof and the challenge used up and the device made, in this one transaction. A challenge that is unknown, expired,
    /// used or no longer fit is [`MetaError::ChallengeGone`], the same for every case; a wrong proof is
    /// [`MetaError::TwoFactorInvalid`] after its counters were committed on their own (the challenge's attempts, deleted at
    /// the fifth; the account's wrong proofs and the lock); a lock in force is [`MetaError::TwoFactorLocked`].
    pub async fn complete_second_factor(
        &self,
        challenge: &[u8; 32],
        proof: &Proof<'_>,
        token_hash: &[u8; 32],
        now_ms: i64,
    ) -> Result<Completed, MetaError> {
        let Some((user, _)) = self.challenge_owner(challenge, now_ms).await? else { return Err(MetaError::ChallengeGone) };
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Err(MetaError::ChallengeGone);
        }
        let found = sqlx::query(
            "SELECT device_name, client_kind, platform, app_version, attempts, expires_ms FROM login_challenges WHERE id_hash = $1 AND user_id = $2",
        )
        .bind(challenge.to_vec())
        .bind(uuid_bytes(&user.0))
        .fetch_optional(&mut *w.tx)
        .await?;
        let Some(found) = found.filter(|row| row.try_get::<i64, _>("expires_ms").is_ok_and(|expires| expires > now_ms)) else {
            return Err(MetaError::ChallengeGone);
        };
        let attempts: i64 = found.try_get("attempts")?;
        let row = load_row(&mut w.tx, user).await?.filter(TfRow::active);
        let Some(row) = row else {
            // Turned off (or reset) since: the challenge is of no use any more.
            sqlx::query("DELETE FROM login_challenges WHERE id_hash = $1").bind(challenge.to_vec()).execute(&mut *w.tx).await?;
            w.commit().await?;
            return Err(MetaError::ChallengeGone);
        };
        if attempts >= MAX_CHALLENGE_ATTEMPTS {
            return Err(MetaError::ChallengeGone);
        }
        let device = DeviceId::generate();
        if let Checked::Wrong = check_proof(&mut w.tx, user, Some(device), &row, proof, now_ms).await? {
            return Err(self.refuse_sign_in_proof(w, user, Some(challenge), proof, now_ms).await);
        }
        if account_blocked(&mut w.tx, user).await? {
            return Err(MetaError::AccountDisabled);
        }
        let used = sqlx::query("DELETE FROM login_challenges WHERE id_hash = $1 AND user_id = $2")
            .bind(challenge.to_vec())
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if used.rows_affected() != 1 {
            return Err(MetaError::ChallengeGone);
        }
        let client =
            DeviceClient { kind: found.try_get("client_kind")?, platform: found.try_get("platform")?, app_version: found.try_get("app_version")? };
        let name: String = found.try_get("device_name")?;
        insert_device(&mut w.tx, device, user, &name, &client, token_hash, now_ms).await?;
        let login = NewEvent::new(AuditAction::AuthLogin).by(user, Some(device)).on_device(device).with(AuditDetails::client(&client));
        record_in(&mut w.tx, now_ms, login).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(Completed { user, device })
    }

    /// `w` refused a wrong proof and has changed nothing (every caller checks the proof before it consumes anything): the wrong
    /// proof's counters are written and **committed in `w` itself**, with the account's row still locked, and the answer is
    /// [`MetaError::TwoFactorInvalid`] (or the error that stopped the counters being written). Counting under the same lock as
    /// the check means the next request sees the attempts and the lock this one made: no more than five wrong proofs per
    /// challenge, and the lock set by exactly the tenth.
    pub(crate) async fn refuse_proof(&self, mut w: WriteTx, user: UserId, challenge: Option<&[u8; 32]>, now_ms: i64) -> MetaError {
        let counted = async {
            let locked = count_wrong_in(&mut w.tx, user, challenge, now_ms).await?;
            w.commit().await?;
            Ok::<bool, MetaError>(locked)
        }
        .await;
        match counted {
            Ok(locked) => {
                if locked {
                    // After the counters and the lock are committed, like a failed sign-in's event: an error in the log
                    // is reported and changes neither the answer nor what was counted (failing closed here would let a
                    // fault in the log undo the lock).
                    if let Err(e) = self.audit_lock_set(user, now_ms).await {
                        tracing::warn!(error = %e, "couldn't write an audit event");
                    }
                }
                MetaError::TwoFactorInvalid
            }
            Err(e) => e,
        }
    }

    /// `auth.two_factor_locked`, in a transaction of its own. Nobody proved who they were: no actor.
    async fn audit_lock_set(&self, user: UserId, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        record_in(&mut w.tx, now_ms, NewEvent::new(AuditAction::AuthTwoFactorLocked).on_user(user)).await?;
        self.commit_audited(w, now_ms).await
    }

    /// [`Meta::refuse_proof`] for a proof given at sign-in (the second step, or a join): after the counters are committed
    /// and the answer is decided, the wrong proof is also counted in the audit log, `auth.second_factor_failed`, in a
    /// transaction of its own and aggregated per account and minute. Writing it can fail without changing the answer. A
    /// wrong proof on a signed-in route (turning it off, new recovery codes, an admin's confirmation) is not written one by
    /// one: the lock they can cause is (`auth.two_factor_locked`).
    pub(crate) async fn refuse_sign_in_proof(
        &self,
        w: WriteTx,
        user: UserId,
        challenge: Option<&[u8; 32]>,
        proof: &Proof<'_>,
        now_ms: i64,
    ) -> MetaError {
        let refused = self.refuse_proof(w, user, challenge, now_ms).await;
        if matches!(refused, MetaError::TwoFactorInvalid) {
            let step = match proof {
                Proof::Totp(_) => AuditStep::Totp,
                Proof::Recovery(_) => AuditStep::Recovery,
            };
            let failure = AuditFailure { action: AuditAction::AuthSecondFactorFailed, target: Some(user), step };
            if let Err(e) = self.audit_record_failure(failure, now_ms).await {
                tracing::warn!(error = %e, "couldn't write an audit event");
            }
        }
        refused
    }

    /// The ciphertexts of every account's secrets (the active ones, and the open set-ups' that are not expired), for the
    /// server to open them at start and for the key tools.
    pub async fn two_factor_secrets(&self, now_ms: i64) -> Result<Vec<StoredSecrets>, MetaError> {
        let rows = sqlx::query(
            "SELECT user_id, secret_ciphertext, pending_ciphertext, pending_expires_ms FROM user_two_factor \
             WHERE secret_ciphertext IS NOT NULL OR pending_ciphertext IS NOT NULL ORDER BY user_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let open = row.try_get::<Option<i64>, _>("pending_expires_ms")?.is_some_and(|expires| expires > now_ms);
                Ok(StoredSecrets {
                    user: UserId(uuid_from(&row.try_get::<Vec<u8>, _>("user_id")?)?),
                    active: row.try_get("secret_ciphertext")?,
                    pending: if open { row.try_get("pending_ciphertext")? } else { None },
                })
            })
            .collect()
    }

    /// Every account's ciphertexts, the open set-ups' and the expired ones too (a plain read, for the key tools).
    pub async fn two_factor_all_secrets(&self) -> Result<Vec<StoredSecrets>, MetaError> {
        let rows = sqlx::query(
            "SELECT user_id, secret_ciphertext, pending_ciphertext FROM user_two_factor \
             WHERE secret_ciphertext IS NOT NULL OR pending_ciphertext IS NOT NULL ORDER BY user_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(StoredSecrets {
                    user: UserId(uuid_from(&row.try_get::<Vec<u8>, _>("user_id")?)?),
                    active: row.try_get("secret_ciphertext")?,
                    pending: row.try_get("pending_ciphertext")?,
                })
            })
            .collect()
    }

    /// Rewrites `user`'s ciphertexts, the active secret and the set-up's both, **read fresh under the account's lock** (a
    /// confirmation that moved a set-up's secret to the active one meanwhile is seen): `rewrite` gets each and answers the
    /// ciphertext to store in its place, or `None` to leave it. Returns how many were rewritten and how many left. An account
    /// that is gone, or has none, is `(0, 0)`; a ciphertext `rewrite` can't handle is an error and changes nothing.
    pub async fn two_factor_rewrite(&self, user: UserId, rewrite: &Rewrite<'_>) -> Result<(usize, usize), MetaError> {
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Ok((0, 0));
        }
        let row = sqlx::query("SELECT secret_ciphertext, pending_ciphertext FROM user_two_factor WHERE user_id = $1")
            .bind(uuid_bytes(&user.0))
            .fetch_optional(&mut *w.tx)
            .await?;
        let Some(row) = row else { return Ok((0, 0)) };
        let (mut rewritten, mut kept) = (0, 0);
        for (column, update) in [
            ("secret_ciphertext", "UPDATE user_two_factor SET secret_ciphertext = $1 WHERE user_id = $2"),
            ("pending_ciphertext", "UPDATE user_two_factor SET pending_ciphertext = $1 WHERE user_id = $2"),
        ] {
            let Some(sealed) = row.try_get::<Option<Vec<u8>>, _>(column)? else { continue };
            match rewrite(user, &sealed).map_err(|_| MetaError::Corrupt("a stored two-factor secret can't be read"))? {
                Some(new) => {
                    sqlx::query(update).bind(new).bind(uuid_bytes(&user.0)).execute(&mut *w.tx).await?;
                    rewritten += 1;
                }
                None => kept += 1,
            }
        }
        w.commit().await?;
        Ok((rewritten, kept))
    }

    /// Replaces the ciphertext `old` of `user`'s active secret (`pending` false) or open set-up (`pending` true) by `new`, if
    /// it still is `old`; `false` when it is not (changed meanwhile). For rewriting secrets under a new key.
    pub async fn two_factor_replace_ciphertext(&self, user: UserId, pending: bool, old: &[u8], new: &[u8]) -> Result<bool, MetaError> {
        let sql = if pending {
            "UPDATE user_two_factor SET pending_ciphertext = $1 WHERE user_id = $2 AND pending_ciphertext = $3"
        } else {
            "UPDATE user_two_factor SET secret_ciphertext = $1 WHERE user_id = $2 AND secret_ciphertext = $3"
        };
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Ok(false);
        }
        let done = sqlx::query(sql).bind(new.to_vec()).bind(uuid_bytes(&user.0)).bind(old.to_vec()).execute(&mut *w.tx).await?;
        w.commit().await?;
        Ok(done.rows_affected() == 1)
    }
}

/// Counts a wrong code against the open set-up of `device`, in `tx`; the fifth uses the set-up up.
async fn count_wrong_setup_in(tx: &mut sqlx::Transaction<'static, sqlx::Any>, user: UserId, device: DeviceId) -> Result<(), MetaError> {
    sqlx::query(
        "UPDATE user_two_factor SET pending_attempts = pending_attempts + 1 \
         WHERE user_id = $1 AND enabled_ms IS NULL AND pending_device = $2 AND pending_ciphertext IS NOT NULL",
    )
    .bind(uuid_bytes(&user.0))
    .bind(uuid_bytes(device.as_uuid()))
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE user_two_factor SET pending_ciphertext = NULL, pending_device = NULL, pending_expires_ms = NULL, pending_attempts = 0 \
         WHERE user_id = $1 AND enabled_ms IS NULL AND pending_attempts >= $2",
    )
    .bind(uuid_bytes(&user.0))
    .bind(MAX_SETUP_ATTEMPTS)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Counts one wrong second factor of `user` in `tx` (the account's row is locked by it): the wrong proofs in a row go up,
/// and at [`LOCK_AFTER_FAILURES`] (and every one after) the lock is set or doubled; the challenge's attempts go up too, and
/// the challenge is deleted at [`MAX_CHALLENGE_ATTEMPTS`]. Lock order: the account's row, the two-factor row, the challenge.
/// Answers whether a lock was set or lengthened (for the caller to write its event once the counters are committed).
async fn count_wrong_in(
    tx: &mut sqlx::Transaction<'static, sqlx::Any>,
    user: UserId,
    challenge: Option<&[u8; 32]>,
    now_ms: i64,
) -> Result<bool, MetaError> {
    let mut locked = false;
    let counted = sqlx::query(
        "UPDATE user_two_factor SET failed_attempts = failed_attempts + 1 WHERE user_id = $1 AND enabled_ms IS NOT NULL \
         RETURNING failed_attempts, locked_until_ms",
    )
    .bind(uuid_bytes(&user.0))
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(row) = counted
        && let Some(duration) = lock_for(row.try_get("failed_attempts")?)
    {
        let until = now_ms.saturating_add(duration);
        let until = row.try_get::<Option<i64>, _>("locked_until_ms")?.map_or(until, |current| current.max(until));
        sqlx::query("UPDATE user_two_factor SET locked_until_ms = $1 WHERE user_id = $2")
            .bind(until)
            .bind(uuid_bytes(&user.0))
            .execute(&mut **tx)
            .await?;
        // Only here, where the lock is set or doubled, and never for a refusal while it is in force (`check_proof` stops
        // those before anything is counted).
        locked = true;
    }
    if let Some(challenge) = challenge {
        sqlx::query("UPDATE login_challenges SET attempts = attempts + 1 WHERE id_hash = $1").bind(challenge.to_vec()).execute(&mut **tx).await?;
        sqlx::query("DELETE FROM login_challenges WHERE id_hash = $1 AND attempts >= $2")
            .bind(challenge.to_vec())
            .bind(MAX_CHALLENGE_ATTEMPTS)
            .execute(&mut **tx)
            .await?;
    }
    Ok(locked)
}

/// Makes `hashes` the account's recovery codes, all unused, in `tx` (the old ones, used or not, are gone).
async fn replace_recovery_codes(tx: &mut sqlx::Transaction<'static, sqlx::Any>, user: UserId, hashes: &[[u8; 32]]) -> Result<(), MetaError> {
    sqlx::query("DELETE FROM user_recovery_codes WHERE user_id = $1").bind(uuid_bytes(&user.0)).execute(&mut **tx).await?;
    for hash in hashes {
        sqlx::query("INSERT INTO user_recovery_codes (user_id, code_hash, used_ms) VALUES ($1, $2, NULL)")
            .bind(uuid_bytes(&user.0))
            .bind(hash.to_vec())
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_starts_at_ten_and_doubles_up_to_a_day() {
        assert_eq!(lock_for(0), None);
        assert_eq!(lock_for(9), None);
        assert_eq!(lock_for(10), Some(15 * 60 * 1000));
        assert_eq!(lock_for(11), Some(30 * 60 * 1000));
        assert_eq!(lock_for(12), Some(60 * 60 * 1000));
        assert_eq!(lock_for(15), Some(8 * 60 * 60 * 1000), "8 hours");
        assert_eq!(lock_for(16), Some(16 * 60 * 60 * 1000), "16 hours");
        assert_eq!(lock_for(17), Some(LOCK_MAX_MS), "32 hours is over a day");
        assert_eq!(lock_for(i64::MAX), Some(LOCK_MAX_MS));
    }

    #[test]
    fn sealed_secrets_show_only_their_length() {
        let secrets = StoredSecrets { user: UserId(uuid::Uuid::nil()), active: Some(vec![7, 8, 9]), pending: None };
        let shown = format!("{secrets:?}");
        assert!(shown.contains("<3 bytes>") && !shown.contains("[7"), "{shown}");
        let device = DeviceActor { user: UserId(uuid::Uuid::nil()), device: DeviceId::generate(), password_hash: "$argon2id$secret".to_owned() };
        assert!(!format!("{device:?}").contains("argon2id"));
    }

    #[test]
    fn retry_after_rounds_up_and_is_never_zero() {
        assert_eq!(retry_after_secs(0), 1);
        assert_eq!(retry_after_secs(1), 1);
        assert_eq!(retry_after_secs(1000), 1);
        assert_eq!(retry_after_secs(1001), 2);
        assert_eq!(retry_after_secs(-5), 1);
    }
}
