//! Users, their spaces, and devices.
//!
//! Passwords are hashed by the caller (Argon2id, in the server); this module only stores the hash string. An account
//! an invite created has an empty one until the invite is redeemed: no password matches it, since it is no hash.
//! Device tokens are random 256-bit values; only their hash is stored, and a device is found by that hash.
//!
//! A device also records what client made it ([`DeviceClient`], shown in the device list and never a basis for a
//! permission), and, once revoked, when and why ([`RevokeReason`]). The `revoked` flag is the one that is checked:
//! the flag, the time and the reason are always set together, in one transaction, and only by the first revoke.

use craft_cloud_proto::{DeviceId, Epoch, NodeId, SpaceId};
use sqlx::Row;
use uuid::Uuid;

use crate::codec::{flag, uuid_bytes, uuid_from};
use crate::{Meta, MetaError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(pub Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    Member,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }

    fn parse(s: &str) -> Result<Self, MetaError> {
        match s {
            "admin" => Ok(Role::Admin),
            "member" => Ok(Role::Member),
            _ => Err(MetaError::Corrupt("unknown role")),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub id: UserId,
    pub username: String,
    pub password_hash: String,
    pub role: Role,
    pub space: SpaceId,
}

impl std::fmt::Debug for UserRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserRecord")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("password_hash", &"<redacted>")
            .field("role", &self.role)
            .field("space", &self.space)
            .finish()
    }
}

/// What `GET /account` tells about a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    pub id: UserId,
    pub username: String,
    pub role: Role,
    pub created_ms: i64,
    /// When the password last changed through the server (`None`: never, the password is still the first one).
    pub password_changed_ms: Option<i64>,
}

/// The client kinds a device may name when it signs in. Empty means the client didn't say.
pub const CLIENT_KINDS: [&str; 3] = ["agent", "desktop", "cli"];
/// The longest platform or app version, in bytes.
pub const MAX_CLIENT_FIELD_BYTES: usize = 32;

/// What a device says about the client it signed in with. Display only: no permission ever depends on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceClient {
    /// One of [`CLIENT_KINDS`], or empty.
    pub kind: String,
    pub platform: String,
    pub app_version: String,
}

impl DeviceClient {
    /// The client a sign-in names; a field left out is empty. An unknown kind, a field over
    /// [`MAX_CLIENT_FIELD_BYTES`] or a control character is `Invalid`.
    pub fn new(kind: Option<&str>, platform: Option<&str>, app_version: Option<&str>) -> Result<Self, MetaError> {
        let client = Self {
            kind: kind.unwrap_or_default().to_owned(),
            platform: platform.unwrap_or_default().to_owned(),
            app_version: app_version.unwrap_or_default().to_owned(),
        };
        client.check()?;
        Ok(client)
    }

    pub(crate) fn check(&self) -> Result<(), MetaError> {
        if !self.kind.is_empty() && !CLIENT_KINDS.contains(&self.kind.as_str()) {
            return Err(MetaError::Invalid("client_kind is agent, desktop or cli"));
        }
        for field in [&self.platform, &self.app_version] {
            if field.len() > MAX_CLIENT_FIELD_BYTES || field.chars().any(char::is_control) {
                return Err(MetaError::Invalid("platform and app_version are at most 32 bytes without control characters"));
            }
        }
        Ok(())
    }
}

/// Why a device was revoked, as stored in `devices.revoke_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    /// The user revoked this device (`DELETE /devices/{id}`).
    User,
    /// The user signed out every other device.
    Others,
    /// The user changed the password.
    Password,
    /// The admin revoked it on the command line.
    Admin,
    /// The account was disabled (reserved).
    Disabled,
    /// Two-factor sign-in was set up or changed (reserved).
    TwoFactor,
    /// The device sat idle too long (reserved).
    Idle,
    /// The admin reset the password on the command line.
    Reset,
}

impl RevokeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RevokeReason::User => "user",
            RevokeReason::Others => "others",
            RevokeReason::Password => "password",
            RevokeReason::Admin => "admin",
            RevokeReason::Disabled => "disabled",
            RevokeReason::TwoFactor => "two_factor",
            RevokeReason::Idle => "idle",
            RevokeReason::Reset => "reset",
        }
    }

    fn parse(s: &str) -> Result<Self, MetaError> {
        Ok(match s {
            "user" => RevokeReason::User,
            "others" => RevokeReason::Others,
            "password" => RevokeReason::Password,
            "admin" => RevokeReason::Admin,
            "disabled" => RevokeReason::Disabled,
            "two_factor" => RevokeReason::TwoFactor,
            "idle" => RevokeReason::Idle,
            "reset" => RevokeReason::Reset,
            _ => return Err(MetaError::Corrupt("unknown revoke reason")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub id: DeviceId,
    pub name: String,
    pub created_ms: i64,
    pub last_used_ms: i64,
    pub revoked: bool,
    /// What the device said about its client when it signed in (empty for devices from before it was recorded).
    pub client: DeviceClient,
    /// When it was revoked, and why; both `None` while it isn't (and for devices revoked before they were recorded).
    pub revoked_ms: Option<i64>,
    pub revoke_reason: Option<RevokeReason>,
}

/// Who a device token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuth {
    pub device: DeviceId,
    pub user: UserId,
    pub role: Role,
    pub space: SpaceId,
    /// The owner's username, in lowercase.
    pub username: String,
}

/// Usernames: 1–64 bytes of ASCII letters, digits, `.`, `_`, `-`. They are stored in lowercase, so names that differ
/// only in ASCII case are the same name.
pub fn valid_username(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl Meta {
    /// Creates a user with their own space and its root directory.
    pub async fn create_user(&self, username: &str, password_hash: &str, role: Role, now_ms: i64) -> Result<UserRecord, MetaError> {
        self.create_user_as(username, password_hash, role, now_ms, (UserId(Uuid::now_v7()), SpaceId::generate(), NodeId::generate())).await
    }

    /// [`Meta::create_user`] with the user's, space's and root's IDs given, so a simulation's seed replays exactly (the
    /// IDs of an app's recents documents derive from the space). A test seam (the `test-support` feature).
    #[cfg(feature = "test-support")]
    pub async fn create_user_with_ids(
        &self,
        username: &str,
        password_hash: &str,
        role: Role,
        now_ms: i64,
        ids: (UserId, SpaceId, NodeId),
    ) -> Result<UserRecord, MetaError> {
        self.create_user_as(username, password_hash, role, now_ms, ids).await
    }

    async fn create_user_as(
        &self,
        username: &str,
        password_hash: &str,
        role: Role,
        now_ms: i64,
        (user, space, root): (UserId, SpaceId, NodeId),
    ) -> Result<UserRecord, MetaError> {
        if !valid_username(username) {
            return Err(MetaError::Invalid("usernames are 1-64 letters, digits, '.', '_' or '-'"));
        }
        let username = username.to_ascii_lowercase();
        let mut w = self.begin_write().await?;
        let taken = sqlx::query("SELECT 1 FROM users WHERE username = $1").bind(username.as_str()).fetch_optional(&mut *w.tx).await?;
        if taken.is_some() {
            return Err(MetaError::UsernameTaken);
        }
        let record = insert_user(&mut w.tx, &username, password_hash, role, now_ms, (user, space, root)).await?;
        w.commit().await?;
        Ok(record)
    }

    pub async fn user_by_name(&self, username: &str) -> Result<Option<UserRecord>, MetaError> {
        let username = username.to_ascii_lowercase();
        let row = sqlx::query(
            "SELECT u.id, u.username, u.password_hash, u.role, s.id AS space FROM users u JOIN spaces s ON s.owner = u.id WHERE u.username = $1",
        )
        .bind(username.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(UserRecord {
            id: UserId(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
            username: row.try_get("username")?,
            password_hash: row.try_get("password_hash")?,
            role: Role::parse(&row.try_get::<String, _>("role")?)?,
            space: SpaceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("space")?)?),
        }))
    }

    /// Registers a device for `user` under the hash of its token, with no word on its client.
    pub async fn create_device(&self, user: UserId, name: &str, token_hash: &[u8; 32], now_ms: i64) -> Result<DeviceId, MetaError> {
        self.create_device_with(user, name, &DeviceClient::default(), token_hash, now_ms).await
    }

    /// [`Meta::create_device`] for a device that says what client it is.
    pub async fn create_device_with(
        &self,
        user: UserId,
        name: &str,
        client: &DeviceClient,
        token_hash: &[u8; 32],
        now_ms: i64,
    ) -> Result<DeviceId, MetaError> {
        check_device_name(name)?;
        client.check()?;
        let device = DeviceId::generate();
        let mut w = self.begin_write().await?;
        insert_device(&mut w.tx, device, user, name, client, token_hash, now_ms).await?;
        w.commit().await?;
        Ok(device)
    }

    /// Registers a device for `user` after a password sign-in, if the password it was checked against is still the
    /// account's: `hash` is the stored hash the caller verified. The account is locked first and the hash compared
    /// again in the same transaction, so a password change or reset that committed while the (slow) check ran is
    /// not followed by a device that check let in. `None`: the user is gone or the password changed meanwhile; the
    /// caller answers as for a wrong password.
    pub async fn sign_in_device(
        &self,
        user: UserId,
        hash: &str,
        name: &str,
        client: &DeviceClient,
        token_hash: &[u8; 32],
        now_ms: i64,
    ) -> Result<Option<DeviceId>, MetaError> {
        check_device_name(name)?;
        client.check()?;
        let device = DeviceId::generate();
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Ok(None);
        }
        let same = sqlx::query("SELECT 1 FROM users WHERE id = $1 AND password_hash = $2")
            .bind(uuid_bytes(&user.0))
            .bind(hash)
            .fetch_optional(&mut *w.tx)
            .await?;
        if same.is_none() {
            return Ok(None);
        }
        insert_device(&mut w.tx, device, user, name, client, token_hash, now_ms).await?;
        w.commit().await?;
        Ok(Some(device))
    }

    /// The device holding this token hash, if any and not revoked.
    pub async fn device_by_token(&self, token_hash: &[u8; 32]) -> Result<Option<DeviceAuth>, MetaError> {
        let row = sqlx::query(
            "SELECT d.id, d.user_id, u.role, u.username, s.id AS space FROM devices d JOIN users u ON u.id = d.user_id JOIN spaces s ON s.owner = u.id \
             WHERE d.token_hash = $1 AND d.revoked = 0",
        )
        .bind(token_hash.to_vec())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(DeviceAuth {
            device: DeviceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
            user: UserId(uuid_from(&row.try_get::<Vec<u8>, _>("user_id")?)?),
            role: Role::parse(&row.try_get::<String, _>("role")?)?,
            space: SpaceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("space")?)?),
            username: row.try_get("username")?,
        }))
    }

    /// Records that a device was just used. Cheap enough to call on every request.
    pub async fn touch_device(&self, device: DeviceId, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        sqlx::query("UPDATE devices SET last_used_ms = $1 WHERE id = $2").bind(now_ms).bind(uuid_bytes(device.as_uuid())).execute(&mut *w.tx).await?;
        w.commit().await
    }

    pub async fn list_devices(&self, user: UserId) -> Result<Vec<DeviceRecord>, MetaError> {
        let rows = sqlx::query(
            "SELECT id, name, created_ms, last_used_ms, revoked, client_kind, platform, app_version, revoked_ms, revoke_reason \
             FROM devices WHERE user_id = $1 ORDER BY created_ms, id",
        )
        .bind(uuid_bytes(&user.0))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(device_record).collect()
    }

    /// What `GET /account` tells about `user`; `NotFound` for a user that isn't there.
    pub async fn account(&self, user: UserId) -> Result<AccountRecord, MetaError> {
        let row = sqlx::query("SELECT username, role, created_ms, password_changed_ms FROM users WHERE id = $1")
            .bind(uuid_bytes(&user.0))
            .fetch_optional(&self.pool)
            .await?
            .ok_or(MetaError::NotFound("user"))?;
        Ok(AccountRecord {
            id: user,
            username: row.try_get("username")?,
            role: Role::parse(&row.try_get::<String, _>("role")?)?,
            created_ms: row.try_get("created_ms")?,
            password_changed_ms: row.try_get("password_changed_ms")?,
        })
    }

    /// Whether `device` exists and isn't revoked (a long-lived connection asks again now and then: the admin CLI may
    /// have revoked it from another process).
    pub async fn device_active(&self, device: DeviceId) -> Result<bool, MetaError> {
        let row =
            sqlx::query("SELECT 1 FROM devices WHERE id = $1 AND revoked = 0").bind(uuid_bytes(device.as_uuid())).fetch_optional(&self.pool).await?;
        Ok(row.is_some())
    }

    /// Revokes one of `user`'s devices as the user's own act ([`RevokeReason::User`]), by the system clock. See
    /// [`Meta::revoke_device_as`].
    pub async fn revoke_device(&self, user: UserId, device: DeviceId) -> Result<(), MetaError> {
        self.revoke_device_as(user, device, RevokeReason::User, wall_clock_ms()).await
    }

    /// Revokes one of `user`'s devices, and deletes the pairing it made, if any (its add-a-computer code dies with
    /// it). Revoking another user's device is `NotFound`. A device that is revoked already stays as it is, with its
    /// first time and reason, and the call succeeds.
    pub async fn revoke_device_as(&self, user: UserId, device: DeviceId, reason: RevokeReason, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Err(MetaError::NotFound("device"));
        }
        let done = sqlx::query("UPDATE devices SET revoked = 1, revoked_ms = $1, revoke_reason = $2 WHERE id = $3 AND user_id = $4 AND revoked = 0")
            .bind(now_ms)
            .bind(reason.as_str())
            .bind(uuid_bytes(device.as_uuid()))
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if done.rows_affected() == 0 {
            let owned = sqlx::query("SELECT 1 FROM devices WHERE id = $1 AND user_id = $2")
                .bind(uuid_bytes(device.as_uuid()))
                .bind(uuid_bytes(&user.0))
                .fetch_optional(&mut *w.tx)
                .await?;
            if owned.is_none() {
                return Err(MetaError::NotFound("device"));
            }
        }
        sqlx::query("DELETE FROM pairings WHERE device_id = $1").bind(uuid_bytes(device.as_uuid())).execute(&mut *w.tx).await?;
        w.commit().await
    }

    /// Renames one of `user`'s devices that isn't revoked and returns it. Another user's, a revoked or a missing
    /// device is `NotFound`, never a different answer, so nothing says which devices exist.
    pub async fn rename_device(&self, user: UserId, device: DeviceId, name: &str) -> Result<DeviceRecord, MetaError> {
        check_device_name(name)?;
        let mut w = self.begin_write().await?;
        let done = sqlx::query("UPDATE devices SET name = $1 WHERE id = $2 AND user_id = $3 AND revoked = 0")
            .bind(name)
            .bind(uuid_bytes(device.as_uuid()))
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::NotFound("device"));
        }
        let row = sqlx::query(
            "SELECT id, name, created_ms, last_used_ms, revoked, client_kind, platform, app_version, revoked_ms, revoke_reason \
             FROM devices WHERE id = $1",
        )
        .bind(uuid_bytes(device.as_uuid()))
        .fetch_one(&mut *w.tx)
        .await?;
        let record = device_record(&row)?;
        w.commit().await?;
        Ok(record)
    }

    /// Revokes every device of `user` but `keep` that isn't revoked (with `reason`), and deletes the pairings those
    /// devices made, in one transaction. Returns the devices revoked, for the caller to end their live connections.
    pub async fn revoke_other_devices(&self, user: UserId, keep: DeviceId, reason: RevokeReason, now_ms: i64) -> Result<Vec<DeviceId>, MetaError> {
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Ok(Vec::new());
        }
        let revoked = revoke_devices(&mut w.tx, user, Some(keep), reason, now_ms).await?;
        w.commit().await?;
        Ok(revoked)
    }

    /// [`Meta::revoke_other_devices`] for every device of `user` (the admin's command line).
    pub async fn revoke_all_devices(&self, user: UserId, reason: RevokeReason, now_ms: i64) -> Result<Vec<DeviceId>, MetaError> {
        let mut w = self.begin_write().await?;
        if !lock_user(&mut w.tx, user).await? {
            return Ok(Vec::new());
        }
        let revoked = revoke_devices(&mut w.tx, user, None, reason, now_ms).await?;
        w.commit().await?;
        Ok(revoked)
    }

    /// Changes `user`'s password to `new_hash`, in one transaction: sets the hash and `password_changed_ms`,
    /// revokes every device but `keep` ([`RevokeReason::Password`]), and deletes the account's open invite and all
    /// its pairings (a code made before the change must not work after it). `current_hash` is the hash the caller
    /// verified the current password against: if the password changed meanwhile, nothing changes and the answer is
    /// `Conflict`; if `keep` was revoked meanwhile, it is `DeviceRevoked`. Returns the devices revoked. A failure at
    /// any step changes nothing.
    pub async fn change_password(
        &self,
        user: UserId,
        current_hash: &str,
        new_hash: &str,
        keep: DeviceId,
        now_ms: i64,
    ) -> Result<Vec<DeviceId>, MetaError> {
        let mut w = self.begin_write().await?;
        // The user's row first, as everywhere (see `lock_user`).
        lock_user(&mut w.tx, user).await?;
        // The caller's device must still be signed in: its Argon2 checks took a while, and a revoke since would
        // otherwise leave a password changed by a device that is signed out. The no-op update locks the row on
        // both databases, so a revoke running now waits for this transaction (and then finds the device kept).
        let kept = sqlx::query("UPDATE devices SET revoked = 0 WHERE id = $1 AND user_id = $2 AND revoked = 0")
            .bind(uuid_bytes(keep.as_uuid()))
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if kept.rows_affected() == 0 {
            return Err(MetaError::DeviceRevoked);
        }
        let done = sqlx::query("UPDATE users SET password_hash = $1, password_changed_ms = $2 WHERE id = $3 AND password_hash = $4")
            .bind(new_hash)
            .bind(now_ms)
            .bind(uuid_bytes(&user.0))
            .bind(current_hash)
            .execute(&mut *w.tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::Conflict("the password changed while this request was running; try again"));
        }
        let revoked = revoke_devices(&mut w.tx, user, Some(keep), RevokeReason::Password, now_ms).await?;
        delete_join_codes(&mut w.tx, user).await?;
        w.commit().await?;
        Ok(revoked)
    }

    /// The admin's password reset: sets `new_hash` and `password_changed_ms`, deletes the account's open invite and
    /// pairings, and, unless `keep_devices`, revokes every device ([`RevokeReason::Reset`]), all in one transaction.
    /// `NotFound` for a user that isn't there. Returns the devices revoked.
    pub async fn reset_password(&self, user: UserId, new_hash: &str, keep_devices: bool, now_ms: i64) -> Result<Vec<DeviceId>, MetaError> {
        let mut w = self.begin_write().await?;
        lock_user(&mut w.tx, user).await?;
        let done = sqlx::query("UPDATE users SET password_hash = $1, password_changed_ms = $2 WHERE id = $3")
            .bind(new_hash)
            .bind(now_ms)
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::NotFound("user"));
        }
        let revoked = if keep_devices { Vec::new() } else { revoke_devices(&mut w.tx, user, None, RevokeReason::Reset, now_ms).await? };
        delete_join_codes(&mut w.tx, user).await?;
        w.commit().await?;
        Ok(revoked)
    }
}

/// Takes the row lock of `user` (a no-op update, so nothing changes), and says whether the user exists. Every
/// transaction that revokes devices or changes a password takes it first, then the rows of the devices, then those of
/// the pairings: one order for all of them, so two of them at once (a password change from one device, a sign-out of
/// the others from another, an admin reset) wait for each other on Postgres and never deadlock. SQLite writers are
/// serialised already.
pub(crate) async fn lock_user(tx: &mut sqlx::Transaction<'static, sqlx::Any>, user: UserId) -> Result<bool, MetaError> {
    let done = sqlx::query("UPDATE users SET username = username WHERE id = $1").bind(uuid_bytes(&user.0)).execute(&mut **tx).await?;
    Ok(done.rows_affected() > 0)
}

fn device_record(row: &sqlx::any::AnyRow) -> Result<DeviceRecord, MetaError> {
    Ok(DeviceRecord {
        id: DeviceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
        name: row.try_get("name")?,
        created_ms: row.try_get("created_ms")?,
        last_used_ms: row.try_get("last_used_ms")?,
        revoked: flag(row, "revoked")?,
        client: DeviceClient { kind: row.try_get("client_kind")?, platform: row.try_get("platform")?, app_version: row.try_get("app_version")? },
        revoked_ms: row.try_get("revoked_ms")?,
        revoke_reason: row.try_get::<Option<String>, _>("revoke_reason")?.as_deref().map(RevokeReason::parse).transpose()?,
    })
}

/// Revokes `user`'s devices that aren't revoked yet, except `keep`, and deletes the pairings of every revoked device
/// of theirs, in `tx`. Time and reason are set with the flag, and only here, so a device revoked before keeps its own.
async fn revoke_devices(
    tx: &mut sqlx::Transaction<'static, sqlx::Any>,
    user: UserId,
    keep: Option<DeviceId>,
    reason: RevokeReason,
    now_ms: i64,
) -> Result<Vec<DeviceId>, MetaError> {
    let rows =
        match keep {
            Some(keep) => sqlx::query(
                "UPDATE devices SET revoked = 1, revoked_ms = $1, revoke_reason = $2 WHERE user_id = $3 AND revoked = 0 AND id <> $4 RETURNING id",
            )
            .bind(now_ms)
            .bind(reason.as_str())
            .bind(uuid_bytes(&user.0))
            .bind(uuid_bytes(keep.as_uuid()))
            .fetch_all(&mut **tx)
            .await?,
            None => {
                sqlx::query("UPDATE devices SET revoked = 1, revoked_ms = $1, revoke_reason = $2 WHERE user_id = $3 AND revoked = 0 RETURNING id")
                    .bind(now_ms)
                    .bind(reason.as_str())
                    .bind(uuid_bytes(&user.0))
                    .fetch_all(&mut **tx)
                    .await?
            }
        };
    sqlx::query("DELETE FROM pairings WHERE device_id IN (SELECT id FROM devices WHERE user_id = $1 AND revoked = 1)")
        .bind(uuid_bytes(&user.0))
        .execute(&mut **tx)
        .await?;
    rows.iter().map(|row| Ok(DeviceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?))).collect()
}

/// Deletes `user`'s open invite and every pairing of theirs, in `tx`.
async fn delete_join_codes(tx: &mut sqlx::Transaction<'static, sqlx::Any>, user: UserId) -> Result<(), MetaError> {
    sqlx::query("DELETE FROM invites WHERE user_id = $1").bind(uuid_bytes(&user.0)).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM pairings WHERE user_id = $1").bind(uuid_bytes(&user.0)).execute(&mut **tx).await?;
    Ok(())
}

/// The system clock, for the call that takes no time of its own.
fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)).unwrap_or(0)
}

/// The device name rule: 1–128 bytes without control characters.
pub(crate) fn check_device_name(name: &str) -> Result<(), MetaError> {
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(MetaError::Invalid("device names are 1-128 bytes without control characters"));
    }
    Ok(())
}

/// Inserts a user (`username` already checked and in lowercase) with their space and its root directory, in `tx`.
/// A name taken meanwhile is `UsernameTaken`.
pub(crate) async fn insert_user(
    tx: &mut sqlx::Transaction<'static, sqlx::Any>,
    username: &str,
    password_hash: &str,
    role: Role,
    now_ms: i64,
    (user, space, root): (UserId, SpaceId, NodeId),
) -> Result<UserRecord, MetaError> {
    sqlx::query("INSERT INTO users (id, username, password_hash, role, created_ms) VALUES ($1, $2, $3, $4, $5)")
        .bind(uuid_bytes(&user.0))
        .bind(username)
        .bind(password_hash)
        .bind(role.as_str())
        .bind(now_ms)
        .execute(&mut **tx)
        .await
        // Two signups racing for one name can both pass a check before this; the unique index turns the loser's
        // insert into a unique violation, which is the same answer as the check.
        .map_err(|e| match e {
            sqlx::Error::Database(ref db) if db.is_unique_violation() => MetaError::UsernameTaken,
            other => MetaError::Db(other),
        })?;
    sqlx::query("INSERT INTO spaces (id, owner, epoch, seq, oldest_seq, root) VALUES ($1, $2, $3, 0, 0, $4)")
        .bind(uuid_bytes(space.as_uuid()))
        .bind(uuid_bytes(&user.0))
        .bind(uuid_bytes(Epoch::generate().as_uuid()))
        .bind(uuid_bytes(root.as_uuid()))
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO nodes (space_id, id, parent, name, fold_key, is_dir, version, deleted, changed_seq) \
         VALUES ($1, $2, NULL, '', '', 1, NULL, 0, 0)",
    )
    .bind(uuid_bytes(space.as_uuid()))
    .bind(uuid_bytes(root.as_uuid()))
    .execute(&mut **tx)
    .await?;
    Ok(UserRecord { id: user, username: username.to_owned(), password_hash: password_hash.to_owned(), role, space })
}

/// Inserts a device (its name already checked) in `tx`.
pub(crate) async fn insert_device(
    tx: &mut sqlx::Transaction<'static, sqlx::Any>,
    device: DeviceId,
    user: UserId,
    name: &str,
    client: &DeviceClient,
    token_hash: &[u8; 32],
    now_ms: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO devices (id, user_id, name, token_hash, created_ms, last_used_ms, revoked, client_kind, platform, app_version) \
         VALUES ($1, $2, $3, $4, $5, $5, 0, $6, $7, $8)",
    )
    .bind(uuid_bytes(device.as_uuid()))
    .bind(uuid_bytes(&user.0))
    .bind(name)
    .bind(token_hash.to_vec())
    .bind(now_ms)
    .bind(client.kind.as_str())
    .bind(client.platform.as_str())
    .bind(client.app_version.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}
