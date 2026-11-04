//! Browser sessions of the web admin: the `web_sessions` table, signing in to one (with the second factor, when the
//! account has one), looking one up, logging out and pruning.
//!
//! - **Only a hash is stored.** The session token is 32 random bytes the server makes and puts in a cookie; the table keeps
//!   its BLAKE3 hash (`id_hash`), like a device's token. A row holds no password, no token, no address and no user agent.
//! - **A session lives exactly as long as the account's `auth_gen` it was made under.** Every change that raises
//!   `users.auth_gen` (a password change or reset, a disable, an enable, a removal, turning two-factor sign-in on or off, an
//!   admin's reset of it) ends the account's sessions with it, with no code of its own in those changes. A session is
//!   also over when the account is disabled or marked for removal, when it is logged out (`revoked_ms`), when it was
//!   made longer ago than the absolute lifetime, and when it was not seen for longer than the idle lifetime.
//! - **Only administrators sign in.** The check is made under the account's lock, together with the password's
//!   generation, so a demotion or a password change that ran while the (slow) password check did cannot be followed by a
//!   session. A refusal is `None` for every reason (wrong state, not an admin, blocked), which the caller answers like a
//!   wrong password: nothing says whether an account exists, is a member, or is disabled.
//! - **The second factor is the device sign-in's, not a copy of it.** A web sign-in's challenge is a row of
//!   `login_challenges` of its own kind (`web`), counted, locked and used up by the same code (`two_factor::check_proof`,
//!   `refuse_sign_in_proof`), and can never be completed into a device (nor a device sign-in's into a session).
//! - **Pruning is bounded.** [`Meta::web_sessions_prune`] deletes at most a given number of expired or revoked rows at a
//!   time, so the server can run it after a sign-in without stalling it, and the command line loops until it is done.

use craft_cloud_proto::DeviceId;
use sqlx::Row;

use crate::audit::{AuditAction, NewEvent, record_in};
use crate::codec::{to_u64, uuid_bytes, uuid_from};
use crate::two_factor::{
    CHALLENGE_TTL_MS, Checked, MAX_CHALLENGE_ATTEMPTS, MAX_OPEN_CHALLENGES, NewChallenge, Proof, TfRow, check_proof, is_enabled, load_row,
};
use crate::users::{account_blocked, lock_user};
use crate::{Meta, MetaError, Role, UserId, WriteTx};

/// A session to make: the hash of its token and how long it may live in all.
#[derive(Clone, PartialEq, Eq)]
pub struct NewWebSession {
    /// The BLAKE3 hash of the session token: all the server keeps of it.
    pub id_hash: [u8; 32],
    /// The most the session may live from now, whatever happens: its absolute lifetime.
    pub absolute_ms: i64,
}

impl std::fmt::Debug for NewWebSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewWebSession").field("id_hash", &"<redacted>").field("absolute_ms", &self.absolute_ms).finish()
    }
}

/// The result of a password sign-in to the web admin that checked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSignIn {
    /// The account has no second factor: the session is made.
    Session,
    /// The account needs its second factor: only the challenge is made, good until this time.
    Challenge { expires_ms: i64 },
}

/// A session that is valid now, with the account it belongs to as the database has it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSession {
    pub user: UserId,
    pub username: String,
    pub role: Role,
    pub created_ms: i64,
    pub last_seen_ms: i64,
    pub absolute_expires_ms: i64,
    /// When a second factor was given at sign-in; `None` for an account without one.
    pub mfa_verified_ms: Option<i64>,
}

/// The SQL that says a session is over, for `$1` the time now and `$2` the oldest `last_seen_ms` that is still idle-valid.
const OVER: &str = "revoked_ms IS NOT NULL OR absolute_expires_ms <= $1 OR last_seen_ms <= $2";

/// Inserts the session of `user` in `tx` (the account's row locked), under the account's current generation.
async fn insert_session(
    tx: &mut sqlx::Transaction<'static, sqlx::Any>,
    user: UserId,
    session: &NewWebSession,
    mfa_verified_ms: Option<i64>,
    now_ms: i64,
) -> Result<(), MetaError> {
    let absolute = now_ms.checked_add(session.absolute_ms).ok_or(MetaError::Invalid("the session's lifetime is too long"))?;
    let generation: i64 =
        sqlx::query("SELECT auth_gen FROM users WHERE id = $1").bind(uuid_bytes(&user.0)).fetch_one(&mut **tx).await?.try_get("auth_gen")?;
    sqlx::query(
        "INSERT INTO web_sessions (id_hash, user_id, auth_gen, created_ms, last_seen_ms, absolute_expires_ms, revoked_ms, mfa_verified_ms) \
         VALUES ($1, $2, $3, $4, $4, $5, NULL, $6)",
    )
    .bind(session.id_hash.to_vec())
    .bind(uuid_bytes(&user.0))
    .bind(generation)
    .bind(now_ms)
    .bind(absolute)
    .bind(mfa_verified_ms)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn is_admin(tx: &mut sqlx::Transaction<'static, sqlx::Any>, user: UserId) -> Result<bool, MetaError> {
    let row = sqlx::query("SELECT role FROM users WHERE id = $1").bind(uuid_bytes(&user.0)).fetch_optional(&mut **tx).await?;
    match row {
        Some(row) => Ok(Role::parse(&row.try_get::<String, _>("role")?)? == Role::Admin),
        None => Ok(false),
    }
}

impl Meta {
    /// A password sign-in to the web admin that checked out: under the account's lock the password hash and `auth_gen` the
    /// caller read before it verified the password are compared again, and the account must still be an administrator, not
    /// disabled and not marked for removal. Then, without two-factor sign-in, the session is made and `auth.login` is
    /// written (no device acts); with it only a challenge of kind `web` is made (the account's expired challenges are
    /// deleted and the oldest goes when it would have more than [`MAX_OPEN_CHALLENGES`]) and no session. `None` for
    /// everything that is not a sign-in, whatever the reason.
    pub async fn web_sign_in(
        &self,
        user: UserId,
        checked: (&str, i64),
        challenge: &NewChallenge,
        session: &NewWebSession,
        now_ms: i64,
    ) -> Result<Option<WebSignIn>, MetaError> {
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
        if same.is_none() || account_blocked(&mut w.tx, user).await? || !is_admin(&mut w.tx, user).await? {
            return Ok(None);
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
                "INSERT INTO login_challenges (id_hash, user_id, kind, client_key, device_name, attempts, expires_ms) \
                 VALUES ($1, $2, 'web', $3, '', 0, $4)",
            )
            .bind(challenge.id_hash.to_vec())
            .bind(uuid_bytes(&user.0))
            .bind(challenge.client_key.as_str())
            .bind(expires_ms)
            .execute(&mut *w.tx)
            .await?;
            w.commit().await?;
            return Ok(Some(WebSignIn::Challenge { expires_ms }));
        }
        insert_session(&mut w.tx, user, session, None, now_ms).await?;
        record_in(&mut w.tx, now_ms, NewEvent::new(AuditAction::AuthLogin).by(user, None::<DeviceId>)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(Some(WebSignIn::Session))
    }

    /// The account a web challenge was made for and its username, by a plain read, so the caller can count the try against
    /// the account before the real check. `None` for a challenge that is unknown, expired, used, or a device sign-in's.
    pub async fn web_challenge_owner(&self, id_hash: &[u8; 32], now_ms: i64) -> Result<Option<(UserId, String)>, MetaError> {
        let row = sqlx::query(
            "SELECT c.user_id, u.username FROM login_challenges c JOIN users u ON u.id = c.user_id \
             WHERE c.id_hash = $1 AND c.kind = 'web' AND c.expires_ms > $2",
        )
        .bind(id_hash.to_vec())
        .bind(now_ms)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some((UserId(uuid_from(&row.try_get::<Vec<u8>, _>("user_id")?)?), row.try_get("username")?)))
    }

    /// Finishes a web sign-in with the second factor, as [`Meta::complete_second_factor`] finishes a device's: the account's
    /// row is locked before the challenge or the factor is touched, the challenge and the account's current state (still an
    /// administrator, two-factor sign-in still on, not disabled) are checked again under the lock, and only then are the
    /// proof and the challenge used up and the session made, in this one transaction. A challenge that is unknown,
    /// expired, used or no longer fit is [`MetaError::ChallengeGone`], the same for every case, an account that is no longer
    /// an administrator or is blocked too; a wrong proof is [`MetaError::TwoFactorInvalid`] after its counters were
    /// committed on their own; a lock in force is [`MetaError::TwoFactorLocked`].
    pub async fn web_complete_second_factor(
        &self,
        challenge: &[u8; 32],
        proof: &Proof<'_>,
        session: &NewWebSession,
        now_ms: i64,
    ) -> Result<UserId, MetaError> {
        let Some((user, _)) = self.web_challenge_owner(challenge, now_ms).await? else { return Err(MetaError::ChallengeGone) };
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Err(MetaError::ChallengeGone);
        }
        let found = sqlx::query("SELECT attempts, expires_ms FROM login_challenges WHERE id_hash = $1 AND user_id = $2 AND kind = 'web'")
            .bind(challenge.to_vec())
            .bind(uuid_bytes(&user.0))
            .fetch_optional(&mut *w.tx)
            .await?;
        let Some(found) = found.filter(|row| row.try_get::<i64, _>("expires_ms").is_ok_and(|expires| expires > now_ms)) else {
            return Err(MetaError::ChallengeGone);
        };
        let attempts: i64 = found.try_get("attempts")?;
        let Some(row) = load_row(&mut w.tx, user).await?.filter(TfRow::active) else {
            // Turned off (or reset) since: the challenge is of no use any more.
            sqlx::query("DELETE FROM login_challenges WHERE id_hash = $1").bind(challenge.to_vec()).execute(&mut *w.tx).await?;
            w.commit().await?;
            return Err(MetaError::ChallengeGone);
        };
        if attempts >= MAX_CHALLENGE_ATTEMPTS || account_blocked(&mut w.tx, user).await? || !is_admin(&mut w.tx, user).await? {
            return Err(MetaError::ChallengeGone);
        }
        if let Checked::Wrong = check_proof(&mut w.tx, user, None, &row, proof, now_ms).await? {
            return Err(self.refuse_sign_in_proof(w, user, Some(challenge), proof, now_ms).await);
        }
        let used = sqlx::query("DELETE FROM login_challenges WHERE id_hash = $1 AND user_id = $2")
            .bind(challenge.to_vec())
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if used.rows_affected() != 1 {
            return Err(MetaError::ChallengeGone);
        }
        insert_session(&mut w.tx, user, session, Some(now_ms), now_ms).await?;
        record_in(&mut w.tx, now_ms, NewEvent::new(AuditAction::AuthLogin).by(user, None::<DeviceId>)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(user)
    }

    /// The session under this token hash, if it is valid at `now_ms`: not logged out, younger than its absolute lifetime,
    /// seen within `idle_ms`, made under the account's current `auth_gen`, and of an account that is neither disabled nor
    /// marked for removal. The account's role is the one the database has now.
    pub async fn web_session(&self, id_hash: &[u8; 32], now_ms: i64, idle_ms: i64) -> Result<Option<WebSession>, MetaError> {
        let row = sqlx::query(
            "SELECT s.user_id, s.created_ms, s.last_seen_ms, s.absolute_expires_ms, s.mfa_verified_ms, u.username, u.role \
             FROM web_sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.id_hash = $1 AND s.revoked_ms IS NULL AND s.absolute_expires_ms > $2 AND s.last_seen_ms > $3 \
             AND s.auth_gen = u.auth_gen AND u.disabled_ms IS NULL AND u.removed_ms IS NULL",
        )
        .bind(id_hash.to_vec())
        .bind(now_ms)
        .bind(now_ms.saturating_sub(idle_ms))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(WebSession {
            user: UserId(uuid_from(&row.try_get::<Vec<u8>, _>("user_id")?)?),
            username: row.try_get("username")?,
            role: Role::parse(&row.try_get::<String, _>("role")?)?,
            created_ms: row.try_get("created_ms")?,
            last_seen_ms: row.try_get("last_seen_ms")?,
            absolute_expires_ms: row.try_get("absolute_expires_ms")?,
            mfa_verified_ms: row.try_get("mfa_verified_ms")?,
        }))
    }

    /// Records that the session was just used (its idle time starts again). Never moves the time back.
    pub async fn web_session_seen(&self, id_hash: &[u8; 32], now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        sqlx::query("UPDATE web_sessions SET last_seen_ms = $1 WHERE id_hash = $2 AND last_seen_ms < $1 AND revoked_ms IS NULL")
            .bind(now_ms)
            .bind(id_hash.to_vec())
            .execute(&mut *w.tx)
            .await?;
        w.commit().await
    }

    /// Logs the session out. One that is gone or over already is left as it is, and the call succeeds.
    pub async fn web_logout(&self, id_hash: &[u8; 32], now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        sqlx::query("UPDATE web_sessions SET revoked_ms = $1 WHERE id_hash = $2 AND revoked_ms IS NULL")
            .bind(now_ms)
            .bind(id_hash.to_vec())
            .execute(&mut *w.tx)
            .await?;
        w.commit().await
    }

    /// Deletes at most `max_rows` sessions that are over (logged out, past their absolute lifetime, or not seen for `idle_ms`)
    /// and says how many went. Rows that are valid are never touched. The rows of sessions that an account change ended
    /// (`auth_gen`) are deleted when their other lifetimes run out, which is at most the absolute lifetime.
    pub async fn web_sessions_prune(&self, now_ms: i64, idle_ms: i64, max_rows: u32) -> Result<u64, MetaError> {
        let mut w = self.begin_write().await?;
        let done = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM web_sessions WHERE id_hash IN (SELECT id_hash FROM web_sessions WHERE {OVER} LIMIT $3)"
        )))
        .bind(now_ms)
        .bind(now_ms.saturating_sub(idle_ms))
        .bind(i64::from(max_rows))
        .execute(&mut *w.tx)
        .await?;
        w.commit().await?;
        Ok(done.rows_affected())
    }

    /// How many sessions [`Meta::web_sessions_prune`] would delete now (the command line's dry run).
    pub async fn web_sessions_count_over(&self, now_ms: i64, idle_ms: i64) -> Result<u64, MetaError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) AS n FROM web_sessions WHERE {OVER}")))
            .bind(now_ms)
            .bind(now_ms.saturating_sub(idle_ms))
            .fetch_one(&self.pool)
            .await?;
        to_u64(row.try_get("n")?)
    }

    /// How many sessions the account has that are valid at `now_ms`, for tests and the overview.
    pub async fn web_sessions_open(&self, user: UserId, now_ms: i64, idle_ms: i64) -> Result<u64, MetaError> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS n FROM web_sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.user_id = $1 AND s.revoked_ms IS NULL AND s.absolute_expires_ms > $2 AND s.last_seen_ms > $3 AND s.auth_gen = u.auth_gen",
        )
        .bind(uuid_bytes(&user.0))
        .bind(now_ms)
        .bind(now_ms.saturating_sub(idle_ms))
        .fetch_one(&self.pool)
        .await?;
        to_u64(row.try_get("n")?)
    }
}

/// Whether the session `id_hash` of `user` is valid at `now_ms`, read inside a change (see `begin_admin`) under the account's
/// lock: not logged out, under the account's current generation, inside its absolute lifetime and used within `idle_ms`. A
/// request that began while the session was valid (a slow body, a wait for a lock or a password check) therefore cannot change
/// anything after the session ended. The account's disabled and removed marks need no look: they raise `auth_gen`.
pub(crate) async fn session_is_live(w: &mut WriteTx, user: UserId, id_hash: &[u8; 32], now_ms: i64, idle_ms: i64) -> Result<bool, MetaError> {
    let row = sqlx::query(
        "SELECT 1 FROM web_sessions s JOIN users u ON u.id = s.user_id          WHERE s.id_hash = $1 AND s.user_id = $2 AND s.revoked_ms IS NULL AND s.auth_gen = u.auth_gen          AND s.absolute_expires_ms > $3 AND s.last_seen_ms > $4",
    )
    .bind(id_hash.to_vec())
    .bind(uuid_bytes(&user.0))
    .bind(now_ms)
    .bind(now_ms.saturating_sub(idle_ms))
    .fetch_optional(&mut *w.tx)
    .await?;
    Ok(row.is_some())
}
