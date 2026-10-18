//! Join codes: invites (made by the admin for an account) and pairings (add-a-computer codes, made by a signed-in
//! device). Redeeming either creates a device, as a sign-in does.
//!
//! - Codes are stored only as their BLAKE3 hash (`craft_cloud_proto::join::Code::hash`), and a code is found by it.
//! - Each is good for one use: [`Meta::redeem`] deletes the invite row, or marks the pairing used, with a conditional
//!   `DELETE … RETURNING` / `UPDATE … WHERE used_by IS NULL … RETURNING` in the transaction that creates the device.
//!   Of two redeems of one code at the same moment, the second finds nothing ([`MetaError::InvalidCode`]): SQLite runs
//!   them one after the other, and on Postgres the second statement waits for the first's row lock and then sees the
//!   row gone or used.
//! - A pairing works only while the device that made it is signed in: revoking that device deletes it, and a redeem
//!   checks it too.
//! - Expired rows are deleted on the next write to their table (a new invite or a new pairing).

use craft_cloud_proto::api::PairingState;
use craft_cloud_proto::join::PAIRING_TTL_MS;
use craft_cloud_proto::{DeviceId, NodeId, SpaceId};
use sqlx::Row;
use uuid::Uuid;

use crate::codec::{uuid_bytes, uuid_from};
use crate::users::{check_device_name, insert_device, insert_user};
use crate::{Meta, MetaError, Role, UserId, valid_username};

/// The account an invite is for, from [`Meta::create_invite`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InviteFor {
    pub user: UserId,
    /// The account is new: an invite created it (this one or an earlier one) and it has no password yet, so the
    /// invite is redeemed with a new password.
    pub new_account: bool,
}

/// What [`Meta::redeem`] created: a device of `user`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Redeemed {
    pub user: UserId,
    pub device: DeviceId,
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
        sqlx::query("DELETE FROM invites WHERE user_id = $1").bind(uuid_bytes(&user.0)).execute(&mut *w.tx).await?;
        sqlx::query("INSERT INTO invites (user_id, code_hash, expires_ms) VALUES ($1, $2, $3)")
            .bind(uuid_bytes(&user.0))
            .bind(code_hash.to_vec())
            .bind(expires_ms)
            .execute(&mut *w.tx)
            .await?;
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

    /// Whether the invite under `code_hash` needs a new password (its account has none): `None` when no unexpired
    /// invite has that hash (it may be a pairing's, or wrong). Lets the server answer `password_required` before it
    /// spends time hashing a password.
    pub async fn invite_needs_password(&self, code_hash: &[u8; 32], now_ms: i64) -> Result<Option<bool>, MetaError> {
        let row = sqlx::query("SELECT u.password_hash FROM invites i JOIN users u ON u.id = i.user_id WHERE i.code_hash = $1 AND i.expires_ms > $2")
            .bind(code_hash.to_vec())
            .bind(now_ms)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| row.try_get::<String, _>("password_hash").map(|hash| hash.is_empty())).transpose().map_err(MetaError::from)
    }

    /// Redeems the code under `code_hash` for a new device named `device_name` with `token_hash`, in one transaction:
    /// an unexpired invite (its row deleted) or an unused, unexpired pairing whose maker is still signed in (marked
    /// used by the new device). An invite to an account without a password needs `new_password_hash`, which becomes the
    /// account's ([`MetaError::PasswordRequired`] without it, and nothing changes); for any other code it is ignored.
    /// Anything else is [`MetaError::InvalidCode`].
    pub async fn redeem(
        &self,
        code_hash: &[u8; 32],
        device_name: &str,
        token_hash: &[u8; 32],
        new_password_hash: Option<&str>,
        now_ms: i64,
    ) -> Result<Redeemed, MetaError> {
        check_device_name(device_name)?;
        let device = DeviceId::generate();
        let mut w = self.begin_write().await?;
        let invite = sqlx::query("DELETE FROM invites WHERE code_hash = $1 AND expires_ms > $2 RETURNING user_id")
            .bind(code_hash.to_vec())
            .bind(now_ms)
            .fetch_optional(&mut *w.tx)
            .await?;
        let user = match invite {
            Some(row) => {
                let user = UserId(uuid_from(&row.try_get::<Vec<u8>, _>("user_id")?)?);
                let current: String = sqlx::query("SELECT password_hash FROM users WHERE id = $1")
                    .bind(uuid_bytes(&user.0))
                    .fetch_optional(&mut *w.tx)
                    .await?
                    .ok_or(MetaError::Corrupt("an invite's account is missing"))?
                    .try_get("password_hash")?;
                if current.is_empty() {
                    // Dropping the transaction puts the invite back.
                    let new = new_password_hash.filter(|hash| !hash.is_empty()).ok_or(MetaError::PasswordRequired)?;
                    sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2").bind(new).bind(uuid_bytes(&user.0)).execute(&mut *w.tx).await?;
                }
                user
            }
            None => {
                let pairing = sqlx::query(
                    "UPDATE pairings SET used_by = $1 \
                     WHERE code_hash = $2 AND used_by IS NULL AND expires_ms > $3 \
                     AND device_id IN (SELECT id FROM devices WHERE revoked = 0) RETURNING user_id",
                )
                .bind(uuid_bytes(device.as_uuid()))
                .bind(code_hash.to_vec())
                .bind(now_ms)
                .fetch_optional(&mut *w.tx)
                .await?
                .ok_or(MetaError::InvalidCode)?;
                UserId(uuid_from(&pairing.try_get::<Vec<u8>, _>("user_id")?)?)
            }
        };
        insert_device(&mut w.tx, device, user, device_name, token_hash, now_ms).await?;
        w.commit().await?;
        Ok(Redeemed { user, device })
    }
}
