//! Users, their spaces, and devices.
//!
//! Passwords are hashed by the caller (Argon2id, in the server); this module only stores the hash string.
//! Device tokens are random 256-bit values; only their hash is stored, and a device is found by that hash.

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
    fn as_str(self) -> &'static str {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub id: DeviceId,
    pub name: String,
    pub created_ms: i64,
    pub last_used_ms: i64,
    pub revoked: bool,
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
        let username = username.as_str();
        let mut w = self.begin_write().await?;
        let taken = sqlx::query("SELECT 1 FROM users WHERE username = $1").bind(username).fetch_optional(&mut *w.tx).await?;
        if taken.is_some() {
            return Err(MetaError::UsernameTaken);
        }
        sqlx::query("INSERT INTO users (id, username, password_hash, role, created_ms) VALUES ($1, $2, $3, $4, $5)")
            .bind(uuid_bytes(&user.0))
            .bind(username)
            .bind(password_hash)
            .bind(role.as_str())
            .bind(now_ms)
            .execute(&mut *w.tx)
            .await
            // Two signups racing for one name can both pass the check above; the unique index turns the loser's
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
            .execute(&mut *w.tx)
            .await?;
        sqlx::query(
            "INSERT INTO nodes (space_id, id, parent, name, fold_key, is_dir, version, deleted, changed_seq) \
             VALUES ($1, $2, NULL, '', '', 1, NULL, 0, 0)",
        )
        .bind(uuid_bytes(space.as_uuid()))
        .bind(uuid_bytes(root.as_uuid()))
        .execute(&mut *w.tx)
        .await?;
        w.commit().await?;
        Ok(UserRecord { id: user, username: username.to_owned(), password_hash: password_hash.to_owned(), role, space })
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

    pub async fn set_password_hash(&self, user: UserId, password_hash: &str) -> Result<(), MetaError> {
        let w = self.begin_write().await?;
        let mut w = w;
        let done = sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
            .bind(password_hash)
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::NotFound("user"));
        }
        w.commit().await
    }

    /// Registers a device for `user` under the hash of its token.
    pub async fn create_device(&self, user: UserId, name: &str, token_hash: &[u8; 32], now_ms: i64) -> Result<DeviceId, MetaError> {
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            return Err(MetaError::Invalid("device names are 1-128 bytes without control characters"));
        }
        let device = DeviceId::generate();
        let mut w = self.begin_write().await?;
        sqlx::query("INSERT INTO devices (id, user_id, name, token_hash, created_ms, last_used_ms, revoked) VALUES ($1, $2, $3, $4, $5, $5, 0)")
            .bind(uuid_bytes(device.as_uuid()))
            .bind(uuid_bytes(&user.0))
            .bind(name)
            .bind(token_hash.to_vec())
            .bind(now_ms)
            .execute(&mut *w.tx)
            .await?;
        w.commit().await?;
        Ok(device)
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
        let rows = sqlx::query("SELECT id, name, created_ms, last_used_ms, revoked FROM devices WHERE user_id = $1 ORDER BY created_ms, id")
            .bind(uuid_bytes(&user.0))
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(DeviceRecord {
                    id: DeviceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
                    name: row.try_get("name")?,
                    created_ms: row.try_get("created_ms")?,
                    last_used_ms: row.try_get("last_used_ms")?,
                    revoked: flag(row, "revoked")?,
                })
            })
            .collect()
    }

    /// Whether `device` exists and isn't revoked (a long-lived connection asks again now and then: the admin CLI may
    /// have revoked it from another process).
    pub async fn device_active(&self, device: DeviceId) -> Result<bool, MetaError> {
        let row =
            sqlx::query("SELECT 1 FROM devices WHERE id = $1 AND revoked = 0").bind(uuid_bytes(device.as_uuid())).fetch_optional(&self.pool).await?;
        Ok(row.is_some())
    }

    /// Revokes one of `user`'s devices. Revoking another user's device is `NotFound`.
    pub async fn revoke_device(&self, user: UserId, device: DeviceId) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        let done = sqlx::query("UPDATE devices SET revoked = 1 WHERE id = $1 AND user_id = $2")
            .bind(uuid_bytes(device.as_uuid()))
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::NotFound("device"));
        }
        w.commit().await
    }
}
