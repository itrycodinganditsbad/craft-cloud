//! Join codes: invites (made by the admin for an account) and pairings (add-a-computer codes, made by a signed-in
//! device). Redeeming either creates a device, as a sign-in does.
//!
//! - Codes are stored only as their BLAKE3 hash (`craft_cloud_proto::join::Code::hash`), and a code is found by it.
//! - Each is good for one use: [`Meta::redeem`] deletes the invite row, or marks the pairing used, with a conditional
//!   `DELETE … RETURNING` / `UPDATE … WHERE used_by IS NULL … RETURNING` in the transaction that creates the device.
//!   Of two redeems of one code at the same moment, the second finds nothing ([`MetaError::InvalidCode`]): SQLite runs
//!   them one after the other, and on Postgres the second statement waits for the first's row lock and then sees the
//!   row gone or used.
//! - An invite row has a purpose: `invite` (a new computer for the account: the command line's `user invite` and the
//!   admin API's invites) or `reset` (a password reset code made through the admin API, good for an hour). Redeeming
//!   a reset code always sets a new password, signs out every device of the account and deletes its pairings, and
//!   then, as every redeem does, creates the redeeming computer's device.
//! - A disabled account, or one marked for removal, redeems nothing ([`MetaError::AccountDisabled`]). That is decided
//!   after the account's row is locked, so a disable that commits meanwhile is not followed by a device.
//! - A pairing works only while the device that made it is signed in: revoking that device deletes it, and a redeem
//!   checks it too.
//! - Expired rows are deleted on the next write to their table (a new invite or a new pairing).

use craft_cloud_proto::api::PairingState;
use craft_cloud_proto::join::PAIRING_TTL_MS;
use craft_cloud_proto::{DeviceId, NodeId, SpaceId};
use sqlx::Row;
use uuid::Uuid;

use crate::codec::{uuid_bytes, uuid_from};
use crate::users::{DeviceClient, account_blocked, check_device_name, delete_join_codes, insert_device, insert_user, lock_user, revoke_devices};
use crate::{Meta, MetaError, RevokeReason, Role, UserId, valid_username};

/// What a row of `invites` is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// Adds a computer to the account (and, for an account without a password, sets it).
    Invite,
    /// Sets a new password and signs the account's devices out.
    Reset,
}

impl Purpose {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Purpose::Invite => "invite",
            Purpose::Reset => "reset",
        }
    }

    fn parse(s: &str) -> Result<Self, MetaError> {
        match s {
            "invite" => Ok(Purpose::Invite),
            "reset" => Ok(Purpose::Reset),
            _ => Err(MetaError::Corrupt("unknown invite purpose")),
        }
    }
}

/// Replaces `user`'s open invite or reset code with one under `code_hash`, in `tx`, after deleting every expired one.
pub(crate) async fn put_code(
    tx: &mut sqlx::Transaction<'static, sqlx::Any>,
    user: UserId,
    code_hash: &[u8; 32],
    purpose: Purpose,
    now_ms: i64,
    expires_ms: i64,
) -> Result<(), MetaError> {
    sqlx::query("DELETE FROM invites WHERE expires_ms <= $1").bind(now_ms).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM invites WHERE user_id = $1").bind(uuid_bytes(&user.0)).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO invites (user_id, code_hash, expires_ms, purpose) VALUES ($1, $2, $3, $4)")
        .bind(uuid_bytes(&user.0))
        .bind(code_hash.to_vec())
        .bind(expires_ms)
        .bind(purpose.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The account an invite is for, from [`Meta::create_invite`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InviteFor {
    pub user: UserId,
    /// The account is new: an invite created it (this one or an earlier one) and it has no password yet, so the
    /// invite is redeemed with a new password.
    pub new_account: bool,
}

/// What [`Meta::redeem`] created: a device of `user`'s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redeemed {
    pub user: UserId,
    pub device: DeviceId,
    /// The devices a reset code signed out (empty for every other code), for the caller to end their live connections.
    pub revoked: Vec<DeviceId>,
}

impl Meta {
    /// An invite for `username` under `code_hash`, good for `ttl_ms` from `now_ms`. It creates the account, with no
    /// password and the member role, if there is none of that name, and replaces the account's earlier invite.
    pub async fn create_invite(&self, username: &str, code_hash: &[u8; 32], now_ms: i64, ttl_ms: i64) -> Result<InviteFor, MetaError> {
        if !valid_username(username) {
            return Err(MetaError::Invalid("usernames are 1-64 letters, digits, '.', '_' or '-'"));
        }
        let username = username.to_ascii_lowercase();
        let expires_ms = now_ms.checked_add(ttl_ms).ok_or(MetaError::Invalid("the invite's lifetime is too long"))?;
        let mut w = self.begin_write().await?;
        sqlx::query("DELETE FROM invites WHERE expires_ms <= $1").bind(now_ms).execute(&mut *w.tx).await?;
        let found = sqlx::query("SELECT id, password_hash FROM users WHERE username = $1").bind(username.as_str()).fetch_optional(&mut *w.tx).await?;
        let (user, new_account) = match found {
            Some(row) => (UserId(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?), row.try_get::<String, _>("password_hash")?.is_empty()),
            None => {
                let ids = (UserId(Uuid::now_v7()), SpaceId::generate(), NodeId::generate());
                (insert_user(&mut w.tx, &username, "", Role::Member, now_ms, ids).await?.id, true)
            }
        };
        // An open reset code of the account is replaced as well: the command line's invite is an invite.
        put_code(&mut w.tx, user, code_hash, Purpose::Invite, now_ms, expires_ms).await?;
        w.commit().await?;
        Ok(InviteFor { user, new_account })
    }

    /// Revokes `username`'s invite; `false` when there was none. The account stays.
    pub async fn revoke_invite(&self, username: &str) -> Result<bool, MetaError> {
        let username = username.to_ascii_lowercase();
        let mut w = self.begin_write().await?;
        let done = sqlx::query("DELETE FROM invites WHERE user_id IN (SELECT id FROM users WHERE username = $1)")
            .bind(username.as_str())
            .execute(&mut *w.tx)
            .await?;
        w.commit().await?;
        Ok(done.rows_affected() > 0)
    }

    /// A pairing made by `device` (one of `user`'s, not revoked) under `code_hash`, good for
    /// [`PAIRING_TTL_MS`] from `now_ms`. It replaces the device's earlier pairing. Another user's or a revoked device
    /// is `NotFound`.
    pub async fn create_pairing(&self, user: UserId, device: DeviceId, code_hash: &[u8; 32], now_ms: i64) -> Result<Uuid, MetaError> {
        let id = Uuid::now_v7();
        let expires_ms = now_ms.checked_add(PAIRING_TTL_MS).ok_or(MetaError::Invalid("the clock is out of range"))?;
        let mut w = self.begin_write().await?;
        // Users first, as a sign-out does: a pairing is never made for a device a sign-out just revoked.
        lock_user(&mut w.tx, user).await?;
        if account_blocked(&mut w.tx, user).await? {
            return Err(MetaError::DeviceRevoked);
        }
        sqlx::query("DELETE FROM pairings WHERE expires_ms <= $1").bind(now_ms).execute(&mut *w.tx).await?;
        sqlx::query("DELETE FROM pairings WHERE device_id = $1").bind(uuid_bytes(device.as_uuid())).execute(&mut *w.tx).await?;
        let done = sqlx::query(
            "INSERT INTO pairings (id, user_id, device_id, code_hash, expires_ms, used_by) \
             SELECT $1, user_id, id, $2, $3, NULL FROM devices WHERE id = $4 AND user_id = $5 AND revoked = 0",
        )
        .bind(uuid_bytes(&id))
        .bind(code_hash.to_vec())
        .bind(expires_ms)
        .bind(uuid_bytes(device.as_uuid()))
        .bind(uuid_bytes(&user.0))
        .execute(&mut *w.tx)
        .await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::NotFound("device"));
        }
        w.commit().await?;
        Ok(id)
    }

    /// Where `user`'s pairing `id` stands, with the name of the device that joined once used. `None` for a pairing
    /// that is gone (cancelled, replaced, its device revoked, or deleted after it expired) and for another user's.
    pub async fn pairing_status(&self, user: UserId, id: Uuid, now_ms: i64) -> Result<Option<(PairingState, Option<String>)>, MetaError> {
        let row = sqlx::query(
            "SELECT p.expires_ms, p.used_by, d.name FROM pairings p LEFT JOIN devices d ON d.id = p.used_by WHERE p.id = $1 AND p.user_id = $2",
        )
        .bind(uuid_bytes(&id))
        .bind(uuid_bytes(&user.0))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let used_by: Option<Vec<u8>> = row.try_get("used_by")?;
        let expires_ms: i64 = row.try_get("expires_ms")?;
        Ok(Some(if used_by.is_some() {
            (PairingState::Used, row.try_get::<Option<String>, _>("name")?)
        } else if expires_ms <= now_ms {
            (PairingState::Expired, None)
        } else {
            (PairingState::Waiting, None)
        }))
    }

    /// Cancels `user`'s pairing `id`. Cancelling one that is gone, or another user's, does nothing.
    pub async fn delete_pairing(&self, user: UserId, id: Uuid) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        sqlx::query("DELETE FROM pairings WHERE id = $1 AND user_id = $2")
            .bind(uuid_bytes(&id))
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        w.commit().await
    }

    /// Whether the invite under `code_hash` needs a new password (its account has none, or it is a reset code, which
    /// always does): `None` when no unexpired invite has that hash (it may be a pairing's, or wrong). Lets the server
    /// answer `password_required` before it spends time hashing a password.
    pub async fn invite_needs_password(&self, code_hash: &[u8; 32], now_ms: i64) -> Result<Option<bool>, MetaError> {
        let row = sqlx::query(
            "SELECT u.password_hash, i.purpose FROM invites i JOIN users u ON u.id = i.user_id WHERE i.code_hash = $1 AND i.expires_ms > $2",
        )
        .bind(code_hash.to_vec())
        .bind(now_ms)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let purpose = Purpose::parse(&row.try_get::<String, _>("purpose")?)?;
        Ok(Some(purpose == Purpose::Reset || row.try_get::<String, _>("password_hash")?.is_empty()))
    }

    /// Redeems the code under `code_hash` for a new device named `device_name` with `token_hash`, in one transaction:
    /// an unexpired invite (its row deleted) or an unused, unexpired pairing whose maker is still signed in (marked
    /// used by the new device). An invite to an account without a password needs `new_password_hash`, which becomes the
    /// account's ([`MetaError::PasswordRequired`] without it, and nothing changes); for any other code it is ignored.
    /// A reset code always needs it: it becomes the account's password whatever the old one was, every device of the
    /// account is signed out first ([`Redeemed::revoked`]) and its pairings are deleted, so the new device is the only
    /// one signed in. A disabled account, or one marked for removal, is [`MetaError::AccountDisabled`] and nothing
    /// changes. Anything else is [`MetaError::InvalidCode`].
    pub async fn redeem(
        &self,
        code_hash: &[u8; 32],
        device_name: &str,
        token_hash: &[u8; 32],
        new_password_hash: Option<&str>,
        now_ms: i64,
    ) -> Result<Redeemed, MetaError> {
        self.redeem_with(code_hash, device_name, &DeviceClient::default(), token_hash, new_password_hash, now_ms).await
    }

    /// [`Meta::redeem`] for a device that says what client it is.
    pub async fn redeem_with(
        &self,
        code_hash: &[u8; 32],
        device_name: &str,
        client: &DeviceClient,
        token_hash: &[u8; 32],
        new_password_hash: Option<&str>,
        now_ms: i64,
    ) -> Result<Redeemed, MetaError> {
        check_device_name(device_name)?;
        client.check()?;
        let device = DeviceId::generate();
        let mut w = self.begin_write().await?;
        // The account's row is locked before the invite is consumed (see `lock_user`): a password reset takes the
        // user and then deletes the invite, so taking them the other way round could deadlock on Postgres. The invite
        // is found with a plain read first, and consumed under the lock with its conditions checked again.
        let found = sqlx::query("SELECT user_id, purpose FROM invites WHERE code_hash = $1 AND expires_ms > $2")
            .bind(code_hash.to_vec())
            .bind(now_ms)
            .fetch_optional(&mut *w.tx)
            .await?;
        let mut revoked = Vec::new();
        let user = match found {
            Some(row) => {
                let user = UserId(uuid_from(&row.try_get::<Vec<u8>, _>("user_id")?)?);
                let purpose = Purpose::parse(&row.try_get::<String, _>("purpose")?)?;
                lock_user(&mut w.tx, user).await?;
                let consumed = sqlx::query("DELETE FROM invites WHERE code_hash = $1 AND user_id = $2 AND expires_ms > $3 AND purpose = $4")
                    .bind(code_hash.to_vec())
                    .bind(uuid_bytes(&user.0))
                    .bind(now_ms)
                    .bind(purpose.as_str())
                    .execute(&mut *w.tx)
                    .await?;
                if consumed.rows_affected() == 0 {
                    // Taken, replaced or deleted (a reset) while this waited for the lock.
                    return Err(MetaError::InvalidCode);
                }
                // Decided under the account's lock: a disable that committed while this waited is seen here, and one
                // that follows waits for this transaction and then signs the new device out with the rest.
                if account_blocked(&mut w.tx, user).await? {
                    return Err(MetaError::AccountDisabled);
                }
                let current: String = sqlx::query("SELECT password_hash FROM users WHERE id = $1")
                    .bind(uuid_bytes(&user.0))
                    .fetch_optional(&mut *w.tx)
                    .await?
                    .ok_or(MetaError::Corrupt("an invite's account is missing"))?
                    .try_get("password_hash")?;
                match purpose {
                    Purpose::Reset => {
                        // Dropping the transaction puts the code back.
                        let new = new_password_hash.filter(|hash| !hash.is_empty()).ok_or(MetaError::PasswordRequired)?;
                        sqlx::query("UPDATE users SET password_hash = $1, password_changed_ms = $2, auth_gen = auth_gen + 1 WHERE id = $3")
                            .bind(new)
                            .bind(now_ms)
                            .bind(uuid_bytes(&user.0))
                            .execute(&mut *w.tx)
                            .await?;
                        // Every device goes, then the pairings: the new device is added after, so it is the only
                        // one signed in.
                        revoked = revoke_devices(&mut w.tx, user, None, RevokeReason::Reset, now_ms).await?;
                        delete_join_codes(&mut w.tx, user).await?;
                    }
                    Purpose::Invite if current.is_empty() => {
                        // Dropping the transaction puts the invite back.
                        let new = new_password_hash.filter(|hash| !hash.is_empty()).ok_or(MetaError::PasswordRequired)?;
                        sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
                            .bind(new)
                            .bind(uuid_bytes(&user.0))
                            .execute(&mut *w.tx)
                            .await?;
                    }
                    Purpose::Invite => {}
                }
                user
            }
            None => {
                // The same for a pairing: its account is locked before the device is added, so a reset that revokes
                // the devices of the account cannot miss one that is being added.
                let owner =
                    sqlx::query("SELECT user_id FROM pairings WHERE code_hash = $1").bind(code_hash.to_vec()).fetch_optional(&mut *w.tx).await?;
                let Some(owner) = owner else { return Err(MetaError::InvalidCode) };
                let owner = UserId(uuid_from(&owner.try_get::<Vec<u8>, _>("user_id")?)?);
                lock_user(&mut w.tx, owner).await?;
                if account_blocked(&mut w.tx, owner).await? {
                    return Err(MetaError::AccountDisabled);
                }
                let pairing = sqlx::query(
                    "UPDATE pairings SET used_by = $1 \
                     WHERE code_hash = $2 AND user_id = $3 AND used_by IS NULL AND expires_ms > $4 \
                     AND device_id IN (SELECT id FROM devices WHERE revoked = 0) RETURNING user_id",
                )
                .bind(uuid_bytes(device.as_uuid()))
                .bind(code_hash.to_vec())
                .bind(uuid_bytes(&owner.0))
                .bind(now_ms)
                .fetch_optional(&mut *w.tx)
                .await?
                .ok_or(MetaError::InvalidCode)?;
                UserId(uuid_from(&pairing.try_get::<Vec<u8>, _>("user_id")?)?)
            }
        };
        insert_device(&mut w.tx, device, user, device_name, client, token_hash, now_ms).await?;
        w.commit().await?;
        Ok(Redeemed { user, device, revoked })
    }
}
