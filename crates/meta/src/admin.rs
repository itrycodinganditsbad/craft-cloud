//! What an administrator does through the admin API: server counters, listing and creating accounts, changing a role or
//! a quota, disabling, enabling and removing an account, invites and password reset codes, and a user's devices.
//!
//! - **One `Meta` method per change, with a typed result**, and each writes its audit event in its own transaction, before
//!   the commit, so a change whose event cannot be written is not made. A change that changes nothing (disabling an account
//!   that is disabled, signing out a device that is out already, giving the role an account has) writes no event.
//! - **The acting admin is checked again inside every change** ([`AdminActor`], `begin_admin`): the role the middleware
//!   read and the password the handler verified (a slow Argon2id check) can be stale by the time the transaction runs.
//!   The device (or the browser session) must still be signed in, the account neither disabled nor marked for removal, the role still `admin`
//!   and the password hash still the one that was verified.
//! - **Lock order** in every change: (1) the `last_admin` transaction lock, if the change can take away an
//!   administrator; (2) the rows of the acting admin and of the target, in ascending ID; (3) rows derived from them
//!   (devices, invites, pairings). Sign-ins, redeems and pairings take the user's row first as well (see
//!   `users::lock_user`), so none of these can deadlock with another, and a disable can never be followed by a device
//!   that an earlier check let in.
//! - **The last administrator** cannot be demoted, disabled or removed. A count cannot be locked with `FOR UPDATE`, and
//!   locking the rows of two different targets does not serialise two changes that each look at the other's row. So
//!   every change that can take an administrator away takes one transaction-wide lock first and counts after it: on
//!   Postgres `pg_advisory_xact_lock`, on SQLite the write transaction (`BEGIN IMMEDIATE`, held by one writer at a
//!   time) already is that lock.

use craft_cloud_proto::limits::MAX_CHUNK_BYTES;
use craft_cloud_proto::{DeviceId, NodeId, SpaceId};
use sqlx::Row;
use uuid::Uuid;

use crate::audit::{AuditAction, AuditDetails, NewEvent, record_in};
use crate::codec::{to_i64, to_u64, uuid_bytes, uuid_from};
use crate::join::{Purpose, put_code};
use crate::two_factor;
use crate::users::{delete_join_codes, insert_user, lock_user, revoke_devices};
use crate::web::session_is_live;
use crate::{DeviceRecord, Dialect, Meta, MetaError, RevokeReason, Role, UserId, WriteTx, valid_username};

/// The key of the Postgres advisory lock that serialises changes which can take an administrator away. Arbitrary and
/// fixed, and different from the migration lock's.
const LAST_ADMIN_LOCK_KEY: i64 = 7240318150662;

/// The smallest quota an admin may give an account: one maximal chunk, like the server-wide cap.
pub const MIN_QUOTA_BYTES: u64 = MAX_CHUNK_BYTES as u64;

/// The longest username search an admin may make, in bytes.
pub const MAX_SEARCH_BYTES: usize = 64;

/// What an admin's request came with: the device token of the JSON API, or the browser session of the web admin.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AdminVia {
    /// The device the request came with; it must still be signed in.
    Device(DeviceId),
    /// The hash of the session token the request came with and the idle lifetime of sessions; the session must still be valid
    /// when the change runs (not logged out, under the account's current `auth_gen`, inside its absolute lifetime and not idle
    /// for `idle_ms`, all judged at the time the change is made, so a request that began before the session ended cannot
    /// change anything after). No device acts, so the events it writes name none.
    Web { id_hash: [u8; 32], idle_ms: i64 },
}

impl std::fmt::Debug for AdminVia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdminVia::Device(device) => f.debug_tuple("Device").field(device).finish(),
            AdminVia::Web { idle_ms, .. } => f.debug_struct("Web").field("id_hash", &"<redacted>").field("idle_ms", idle_ms).finish(),
        }
    }
}

/// The admin who asks, as the handler knew them when it verified their password.
#[derive(Clone, PartialEq, Eq)]
pub struct AdminActor {
    pub user: UserId,
    /// How the request signed in; it must still be so.
    pub via: AdminVia,
    /// The stored password hash the confirmation was verified against; it must still be the account's.
    pub password_hash: String,
    /// Whether the acting admin's account had two-factor sign-in on when their confirmation was checked
    /// ([`Meta::confirm_second_factor`]); it must still be so (turning it on or off meanwhile is `Conflict`).
    pub two_factor: bool,
}

impl std::fmt::Debug for AdminActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminActor")
            .field("user", &self.user)
            .field("via", &self.via)
            .field("password_hash", &"<redacted>")
            .field("two_factor", &self.two_factor)
            .finish()
    }
}

/// An account as the admin lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUser {
    pub id: UserId,
    pub username: String,
    pub role: Role,
    pub created_ms: i64,
    pub disabled_ms: Option<i64>,
    /// When the account was marked for removal (it is disabled too); the lists leave such accounts out, a single lookup does not.
    pub removed_ms: Option<i64>,
    /// Two-factor sign-in is on.
    pub two_factor_enabled: bool,
    /// Devices that are not revoked.
    pub devices_active: u64,
    pub stored_bytes: u64,
    /// The quota an admin set for the account (`None`: the server-wide cap, if any, applies).
    pub quota_bytes: Option<u64>,
    /// An unexpired invite is open (a reset code is not an invite).
    pub invite_pending: bool,
}

/// The counters `GET /admin/server` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerCounts {
    pub users: u64,
    pub devices_active: u64,
    pub stored_bytes: u64,
    pub live_bytes: u64,
}

/// One page of [`Meta::admin_list_users`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPage {
    pub users: Vec<AdminUser>,
    /// The username to continue after, when there are more.
    pub next_after: Option<String>,
}

/// A code an admin made: when it stops working. The code itself is never stored, only its hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Issued {
    pub expires_ms: i64,
}

/// How long a password reset code works: one hour.
pub const RESET_TTL_MS: i64 = 60 * 60 * 1000;

struct Target {
    role: Role,
    disabled: bool,
    removed: bool,
    has_password: bool,
}

const USER_SELECT: &str = "SELECT u.id, u.username, u.role, u.created_ms, u.disabled_ms, u.removed_ms, s.stored_bytes, s.quota_bytes, \
     (SELECT COUNT(*) FROM user_two_factor t WHERE t.user_id = u.id AND t.enabled_ms IS NOT NULL) AS two_factor, \
     (SELECT COUNT(*) FROM devices d WHERE d.user_id = u.id AND d.revoked = 0) AS devices_active, \
     (SELECT COUNT(*) FROM invites i WHERE i.user_id = u.id AND i.purpose = 'invite' AND i.expires_ms > $1) AS invites \
     FROM users u JOIN spaces s ON s.owner = u.id";

fn admin_user_from(row: &sqlx::any::AnyRow) -> Result<AdminUser, MetaError> {
    let quota: Option<i64> = row.try_get("quota_bytes")?;
    Ok(AdminUser {
        id: UserId(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
        username: row.try_get("username")?,
        role: Role::parse(&row.try_get::<String, _>("role")?)?,
        created_ms: row.try_get("created_ms")?,
        disabled_ms: row.try_get("disabled_ms")?,
        removed_ms: row.try_get("removed_ms")?,
        two_factor_enabled: row.try_get::<i64, _>("two_factor")? > 0,
        devices_active: to_u64(row.try_get("devices_active")?)?,
        stored_bytes: to_u64(row.try_get("stored_bytes")?)?,
        quota_bytes: quota.map(to_u64).transpose()?,
        invite_pending: row.try_get::<i64, _>("invites")? > 0,
    })
}

/// `text` as a `LIKE` pattern that matches it literally (`\` is the escape character, see the queries).
fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// An event the acting admin makes to `target`.
fn by_admin(actor: &AdminActor, action: AuditAction, target: UserId) -> NewEvent {
    let device = match actor.via {
        AdminVia::Device(device) => Some(device),
        AdminVia::Web { .. } => None,
    };
    NewEvent::new(action).by(actor.user, device).on_user(target)
}

fn same_user(a: UserId, b: UserId) -> bool {
    a.0 == b.0
}

impl Meta {
    /// Starts the write transaction of a change the admin `actor` makes (to `target`, if it has one): takes the
    /// locks in the order the module describes and checks the actor again. `reduces_admins`: the change can take an
    /// administrator away, so it takes the `last_admin` lock first.
    async fn begin_admin(&self, actor: &AdminActor, target: Option<UserId>, reduces_admins: bool) -> Result<WriteTx, MetaError> {
        let mut w = self.begin_write().await?;
        // SQLite: the write transaction already serialises every change, so nothing more is needed there.
        if reduces_admins && self.dialect == Dialect::Postgres {
            sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(LAST_ADMIN_LOCK_KEY).execute(&mut *w.tx).await?;
        }
        let mut ids = vec![actor.user];
        if let Some(target) = target
            && !same_user(target, actor.user)
        {
            ids.push(target);
        }
        ids.sort_by_key(|id| *id.0.as_bytes());
        for id in ids {
            // A target that isn't there is the caller's `NotFound`; the actor's own row is checked just below.
            lock_user(&mut w.tx, id).await?;
        }
        let row = sqlx::query("SELECT role, password_hash, disabled_ms, removed_ms FROM users WHERE id = $1")
            .bind(uuid_bytes(&actor.user.0))
            .fetch_optional(&mut *w.tx)
            .await?;
        let Some(row) = row else { return Err(MetaError::DeviceRevoked) };
        let still_signed_in = match actor.via {
            AdminVia::Device(device) => sqlx::query("SELECT 1 FROM devices WHERE id = $1 AND user_id = $2 AND revoked = 0")
                .bind(uuid_bytes(device.as_uuid()))
                .bind(uuid_bytes(&actor.user.0))
                .fetch_optional(&mut *w.tx)
                .await?
                .is_some(),
            // The time is read here, after every lock wait above and in the order the module describes, so a session that ended
            // while this change waited does not authorise it.
            AdminVia::Web { id_hash, idle_ms } => session_is_live(&mut w, actor.user, &id_hash, self.clock_ms(), idle_ms).await?,
        };
        if !still_signed_in || row.try_get::<Option<i64>, _>("disabled_ms")?.is_some() || row.try_get::<Option<i64>, _>("removed_ms")?.is_some() {
            return Err(MetaError::DeviceRevoked);
        }
        if Role::parse(&row.try_get::<String, _>("role")?)? != Role::Admin {
            return Err(MetaError::Forbidden("you are no longer an administrator"));
        }
        if row.try_get::<String, _>("password_hash")? != actor.password_hash {
            return Err(MetaError::Conflict("your password changed while this request was running; try again"));
        }
        // The confirmation asked for a second factor, or didn't, by what the account had then.
        if two_factor::is_enabled(&mut w.tx, actor.user).await? != actor.two_factor {
            return Err(MetaError::Conflict("your two-factor sign-in changed while this request was running; try again"));
        }
        Ok(w)
    }

    async fn target(w: &mut WriteTx, user: UserId) -> Result<Target, MetaError> {
        let row = sqlx::query("SELECT role, password_hash, disabled_ms, removed_ms FROM users WHERE id = $1")
            .bind(uuid_bytes(&user.0))
            .fetch_optional(&mut *w.tx)
            .await?
            .ok_or(MetaError::NotFound("user"))?;
        Ok(Target {
            role: Role::parse(&row.try_get::<String, _>("role")?)?,
            disabled: row.try_get::<Option<i64>, _>("disabled_ms")?.is_some(),
            removed: row.try_get::<Option<i64>, _>("removed_ms")?.is_some(),
            has_password: !row.try_get::<String, _>("password_hash")?.is_empty(),
        })
    }

    /// `LastAdmin` unless an administrator who is not disabled and not marked for removal is left, counted in `w`
    /// after the `last_admin` lock was taken and the change was made.
    async fn keep_an_admin(w: &mut WriteTx) -> Result<(), MetaError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM users WHERE role = 'admin' AND disabled_ms IS NULL AND removed_ms IS NULL")
            .fetch_one(&mut *w.tx)
            .await?;
        if row.try_get::<i64, _>("n")? < 1 { Err(MetaError::LastAdmin) } else { Ok(()) }
    }

    /// The counters of `GET /admin/server`: accounts that are not marked for removal, devices that are not revoked, and
    /// the bytes accounts store and their live documents hold.
    pub async fn admin_server_counts(&self) -> Result<ServerCounts, MetaError> {
        let count = |row: sqlx::any::AnyRow, column: &str| -> Result<u64, MetaError> { to_u64(row.try_get(column)?) };
        // Postgres sums BIGINT as NUMERIC, which the `Any` driver can't read: cast back.
        let users = sqlx::query("SELECT COUNT(*) AS n FROM users WHERE removed_ms IS NULL").fetch_one(&self.pool).await?;
        let devices = sqlx::query("SELECT COUNT(*) AS n FROM devices WHERE revoked = 0").fetch_one(&self.pool).await?;
        let bytes = sqlx::query(
            "SELECT CAST(COALESCE(SUM(stored_bytes), 0) AS BIGINT) AS stored, CAST(COALESCE(SUM(live_bytes), 0) AS BIGINT) AS live FROM spaces",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(ServerCounts {
            users: count(users, "n")?,
            devices_active: count(devices, "n")?,
            stored_bytes: to_u64(bytes.try_get("stored")?)?,
            live_bytes: to_u64(bytes.try_get("live")?)?,
        })
    }

    /// One page of accounts that are not marked for removal, by username ascending: those after `after` (exclusive)
    /// whose username contains `search` (case does not matter, `%` and `_` are ordinary characters), at most `limit`.
    /// `next_after` is set when there are more. A `search` over [`MAX_SEARCH_BYTES`] is `Invalid`.
    pub async fn admin_list_users(&self, after: Option<&str>, search: Option<&str>, limit: u32, now_ms: i64) -> Result<UserPage, MetaError> {
        let search = search.unwrap_or_default();
        if search.len() > MAX_SEARCH_BYTES {
            return Err(MetaError::Invalid("the search is at most 64 bytes"));
        }
        let pattern = format!("%{}%", escape_like(&search.to_ascii_lowercase()));
        let sql =
            format!("{USER_SELECT} WHERE u.removed_ms IS NULL AND u.username > $2 AND u.username LIKE $3 ESCAPE '\\' ORDER BY u.username LIMIT $4");
        let fetch = i64::from(limit).saturating_add(1);
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(now_ms)
            .bind(after.unwrap_or_default())
            .bind(pattern)
            .bind(fetch)
            .fetch_all(&self.pool)
            .await?;
        let mut users = rows.iter().map(admin_user_from).collect::<Result<Vec<_>, _>>()?;
        let more = users.len() > limit as usize;
        users.truncate(limit as usize);
        let next_after = if more { users.last().map(|u| u.username.clone()) } else { None };
        Ok(UserPage { users, next_after })
    }

    /// One account, marked for removal or not; `None` if there is no such user.
    pub async fn admin_user(&self, user: UserId, now_ms: i64) -> Result<Option<AdminUser>, MetaError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("{USER_SELECT} WHERE u.id = $2")))
            .bind(now_ms)
            .bind(uuid_bytes(&user.0))
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(admin_user_from).transpose()
    }

    /// Makes an account for `username` with `role`, no password and its own space, and an invite under `code_hash`
    /// (good for `ttl_ms`) the person redeems to choose a password. A name that is taken, by any account (one marked for
    /// removal too), is `UsernameTaken`.
    pub async fn admin_create_user(
        &self,
        actor: &AdminActor,
        username: &str,
        role: Role,
        code_hash: &[u8; 32],
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<(AdminUser, Issued), MetaError> {
        if !valid_username(username) {
            return Err(MetaError::Invalid("usernames are 1-64 letters, digits, '.', '_' or '-'"));
        }
        let username = username.to_ascii_lowercase();
        let expires_ms = now_ms.checked_add(ttl_ms).ok_or(MetaError::Invalid("the invite's lifetime is too long"))?;
        let mut w = self.begin_admin(actor, None, false).await?;
        let taken = sqlx::query("SELECT 1 FROM users WHERE username = $1").bind(username.as_str()).fetch_optional(&mut *w.tx).await?;
        if taken.is_some() {
            return Err(MetaError::UsernameTaken);
        }
        let ids = (UserId(Uuid::now_v7()), SpaceId::generate(), NodeId::generate());
        let user = insert_user(&mut w.tx, &username, "", role, now_ms, ids).await?.id;
        put_code(&mut w.tx, user, code_hash, Purpose::Invite, false, now_ms, expires_ms).await?;
        let created = by_admin(actor, AuditAction::AdminUserCreated, user).with(AuditDetails { role_to: Some(role), ..AuditDetails::default() });
        record_in(&mut w.tx, now_ms, created).await?;
        self.commit_audited(w, now_ms).await?;
        let made = self.admin_user(user, now_ms).await?.ok_or(MetaError::Corrupt("a new account is missing"))?;
        Ok((made, Issued { expires_ms }))
    }

    /// Changes an account's role and/or its quota: `role` `None` leaves the role, `quota` `None` leaves the quota,
    /// `Some(None)` removes it, `Some(Some(n))` sets it (at least [`MIN_QUOTA_BYTES`], at most `i64::MAX`). Demoting the
    /// last administrator is `LastAdmin`; an account marked for removal is `Conflict`.
    pub async fn admin_update_user(
        &self,
        actor: &AdminActor,
        target: UserId,
        role: Option<Role>,
        quota: Option<Option<u64>>,
        now_ms: i64,
    ) -> Result<AdminUser, MetaError> {
        if role.is_none() && quota.is_none() {
            return Err(MetaError::Invalid("give a role or a quota_bytes to change"));
        }
        if let Some(Some(bytes)) = quota
            && !(MIN_QUOTA_BYTES..=i64::MAX as u64).contains(&bytes)
        {
            return Err(MetaError::Invalid("quota_bytes is at least one chunk (4194304) and at most 9223372036854775807"));
        }
        let mut w = self.begin_admin(actor, Some(target), role == Some(Role::Member)).await?;
        let found = Self::target(&mut w, target).await?;
        if found.removed {
            return Err(MetaError::Conflict("this account is marked for removal"));
        }
        if let Some(role) = role {
            sqlx::query("UPDATE users SET role = $1 WHERE id = $2").bind(role.as_str()).bind(uuid_bytes(&target.0)).execute(&mut *w.tx).await?;
            if found.role == Role::Admin && role == Role::Member {
                Self::keep_an_admin(&mut w).await?;
            }
            // One event for the role and one for the quota when both change, in this transaction. A role the account has
            // already is no change and has none.
            if role != found.role {
                let details = AuditDetails { role_from: Some(found.role), role_to: Some(role), ..AuditDetails::default() };
                record_in(&mut w.tx, now_ms, by_admin(actor, AuditAction::AdminRoleChanged, target).with(details)).await?;
            }
        }
        if let Some(quota) = quota {
            let quota = quota.map(to_i64).transpose()?;
            let before: Option<i64> = sqlx::query("SELECT quota_bytes FROM spaces WHERE owner = $1")
                .bind(uuid_bytes(&target.0))
                .fetch_optional(&mut *w.tx)
                .await?
                .map(|row| row.try_get("quota_bytes"))
                .transpose()?
                .flatten();
            sqlx::query("UPDATE spaces SET quota_bytes = $1 WHERE owner = $2").bind(quota).bind(uuid_bytes(&target.0)).execute(&mut *w.tx).await?;
            if quota != before {
                let details = AuditDetails { quota_bytes: Some(quota.map(to_u64).transpose()?), ..AuditDetails::default() };
                record_in(&mut w.tx, now_ms, by_admin(actor, AuditAction::AdminQuotaChanged, target).with(details)).await?;
            }
        }
        self.commit_audited(w, now_ms).await?;
        self.admin_user(target, now_ms).await?.ok_or(MetaError::NotFound("user"))
    }

    /// Disables an account: sets `disabled_ms`, signs every device out ([`RevokeReason::Disabled`]) and deletes the
    /// account's invite, reset code and pairings. Disabling one's own account is `Invalid`, the last enabled
    /// administrator `LastAdmin`; an account that is disabled already is left as it is. Returns the devices signed out.
    pub async fn admin_disable_user(&self, actor: &AdminActor, target: UserId, now_ms: i64) -> Result<Vec<DeviceId>, MetaError> {
        if same_user(actor.user, target) {
            return Err(MetaError::Invalid("you can't disable your own account"));
        }
        let mut w = self.begin_admin(actor, Some(target), true).await?;
        let found = Self::target(&mut w, target).await?;
        sqlx::query("UPDATE users SET disabled_ms = COALESCE(disabled_ms, $1), auth_gen = auth_gen + 1 WHERE id = $2")
            .bind(now_ms)
            .bind(uuid_bytes(&target.0))
            .execute(&mut *w.tx)
            .await?;
        Self::keep_an_admin(&mut w).await?;
        let revoked = revoke_devices(&mut w.tx, target, None, RevokeReason::Disabled, now_ms).await?;
        delete_join_codes(&mut w.tx, target).await?;
        // The devices signed out with it are counted in the event. An account that was disabled already is no change.
        if !found.disabled {
            record_in(&mut w.tx, now_ms, by_admin(actor, AuditAction::AdminUserDisabled, target).with(AuditDetails::revoked(revoked.len()))).await?;
        }
        self.commit_audited(w, now_ms).await?;
        Ok(revoked)
    }

    /// Enables a disabled account. Devices that were signed out stay signed out. One marked for removal is `Conflict`.
    /// An account that is not disabled is left as it is and writes no event.
    pub async fn admin_enable_user(&self, actor: &AdminActor, target: UserId, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_admin(actor, Some(target), false).await?;
        let found = Self::target(&mut w, target).await?;
        if found.removed {
            return Err(MetaError::Conflict("this account is marked for removal"));
        }
        sqlx::query("UPDATE users SET disabled_ms = NULL, auth_gen = auth_gen + 1 WHERE id = $1")
            .bind(uuid_bytes(&target.0))
            .execute(&mut *w.tx)
            .await?;
        if found.disabled {
            record_in(&mut w.tx, now_ms, by_admin(actor, AuditAction::AdminUserEnabled, target)).await?;
        }
        self.commit_audited(w, now_ms).await
    }

    /// Marks an account for removal: as [`Meta::admin_disable_user`], and also sets `removed_ms`, which leaves it out of
    /// the lists and cannot be undone here. Nothing it stored is deleted; its username stays taken. Removing one's
    /// own account is `Invalid`, the last enabled administrator `LastAdmin`. Returns the devices signed out.
    pub async fn admin_remove_user(&self, actor: &AdminActor, target: UserId, now_ms: i64) -> Result<Vec<DeviceId>, MetaError> {
        if same_user(actor.user, target) {
            return Err(MetaError::Invalid("you can't remove your own account"));
        }
        let mut w = self.begin_admin(actor, Some(target), true).await?;
        let found = Self::target(&mut w, target).await?;
        sqlx::query(
            "UPDATE users SET removed_ms = COALESCE(removed_ms, $1), disabled_ms = COALESCE(disabled_ms, $1), auth_gen = auth_gen + 1 WHERE id = $2",
        )
        .bind(now_ms)
        .bind(uuid_bytes(&target.0))
        .execute(&mut *w.tx)
        .await?;
        Self::keep_an_admin(&mut w).await?;
        let revoked = revoke_devices(&mut w.tx, target, None, RevokeReason::Disabled, now_ms).await?;
        delete_join_codes(&mut w.tx, target).await?;
        // The event outlives the account, which stays only as a mark. Marked already: no change, no event.
        if !found.removed {
            record_in(&mut w.tx, now_ms, by_admin(actor, AuditAction::AdminUserRemoved, target).with(AuditDetails::revoked(revoked.len()))).await?;
        }
        self.commit_audited(w, now_ms).await?;
        Ok(revoked)
    }

    /// An invite under `code_hash` for an account that has no password yet (one nobody redeemed an invite for), replacing
    /// its open invite or reset code. An account with a password is `Conflict`: an invite adds a computer without asking
    /// for the password, which would give whoever holds it the account's files; a reset code is the way in there. A
    /// disabled account, or one marked for removal, is `Conflict` too.
    pub async fn admin_issue_invite(
        &self,
        actor: &AdminActor,
        target: UserId,
        code_hash: &[u8; 32],
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<Issued, MetaError> {
        let expires_ms = now_ms.checked_add(ttl_ms).ok_or(MetaError::Invalid("the invite's lifetime is too long"))?;
        let mut w = self.begin_admin(actor, Some(target), false).await?;
        let found = Self::target(&mut w, target).await?;
        if found.removed || found.disabled {
            return Err(MetaError::Conflict("this account is disabled or marked for removal"));
        }
        if found.has_password {
            return Err(MetaError::Conflict("this account has a password: make a password reset code for it instead"));
        }
        put_code(&mut w.tx, target, code_hash, Purpose::Invite, false, now_ms, expires_ms).await?;
        record_in(&mut w.tx, now_ms, by_admin(actor, AuditAction::AdminInviteIssued, target)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(Issued { expires_ms })
    }

    /// Deletes the account's open invite or reset code; `NotFound` when there is none (so nothing changed and nothing is
    /// written).
    pub async fn admin_revoke_invite(&self, actor: &AdminActor, target: UserId, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_admin(actor, Some(target), false).await?;
        Self::target(&mut w, target).await?;
        let done = sqlx::query("DELETE FROM invites WHERE user_id = $1").bind(uuid_bytes(&target.0)).execute(&mut *w.tx).await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::NotFound("invite"));
        }
        record_in(&mut w.tx, now_ms, by_admin(actor, AuditAction::AdminInviteRevoked, target)).await?;
        self.commit_audited(w, now_ms).await
    }

    /// A password reset code under `code_hash`, good for `ttl_ms`, replacing the account's open invite or reset code.
    /// Redeeming it sets a new password and signs every device of the account out ([`Meta::redeem`]). Nothing changes
    /// until then. With `reset_two_factor`, redeeming it also turns the account's two-factor sign-in off; without, an
    /// account that has it needs its second factor to redeem the code. Resetting one's own password is `Invalid` (change
    /// it), another administrator's `Forbidden` unless the acting admin has two-factor sign-in on themselves. A disabled
    /// account, or one marked for removal, is `Conflict`.
    pub async fn admin_issue_reset(
        &self,
        actor: &AdminActor,
        target: UserId,
        code_hash: &[u8; 32],
        reset_two_factor: bool,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<Issued, MetaError> {
        if same_user(actor.user, target) {
            return Err(MetaError::Invalid("change your own password in your account instead"));
        }
        let expires_ms = now_ms.checked_add(ttl_ms).ok_or(MetaError::Invalid("the code's lifetime is too long"))?;
        let mut w = self.begin_admin(actor, Some(target), false).await?;
        let found = Self::target(&mut w, target).await?;
        if found.removed || found.disabled {
            return Err(MetaError::Conflict("this account is disabled or marked for removal"));
        }
        Self::admin_target_needs_two_factor(&found, actor)?;
        put_code(&mut w.tx, target, code_hash, Purpose::Reset, reset_two_factor, now_ms, expires_ms).await?;
        record_in(&mut w.tx, now_ms, by_admin(actor, AuditAction::AdminPasswordResetIssued, target)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(Issued { expires_ms })
    }

    /// Takes away another account's two-factor sign-in: the secret, the codes and the challenges go, every device is signed
    /// out ([`RevokeReason::TwoFactor`]) and the invite and pairings are deleted, in one transaction (the target's lock
    /// and the acting admin's checks are as for every admin change). Returns the devices signed out, for the caller to end
    /// their live connections. Your own account is `Invalid` (turn it off in your account), an account without two-factor
    /// sign-in `Conflict` (nothing is signed out), another administrator's `Forbidden` unless the acting admin has
    /// two-factor sign-in on themselves.
    pub async fn admin_reset_two_factor(&self, actor: &AdminActor, target: UserId, now_ms: i64) -> Result<Vec<DeviceId>, MetaError> {
        if same_user(actor.user, target) {
            return Err(MetaError::Invalid("turn your own two-factor sign-in off in your account instead"));
        }
        let mut w = self.begin_admin(actor, Some(target), false).await?;
        let found = Self::target(&mut w, target).await?;
        Self::admin_target_needs_two_factor(&found, actor)?;
        let revoked = two_factor::reset_in(&mut w.tx, target, now_ms).await?.ok_or(MetaError::Conflict("this account has no two-factor sign-in"))?;
        record_in(&mut w.tx, now_ms, by_admin(actor, AuditAction::AdminTwoFactorReset, target)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(revoked)
    }

    /// Resetting another **administrator's** password or two-factor sign-in takes the acting admin's own two-factor
    /// sign-in: a stolen admin token and password alone can't take over the other admins' accounts. (`begin_admin` has
    /// compared `actor.two_factor` with the account's state under the lock, so it is the truth here.)
    fn admin_target_needs_two_factor(target: &Target, actor: &AdminActor) -> Result<(), MetaError> {
        if target.role == Role::Admin && !actor.two_factor {
            return Err(MetaError::Forbidden(
                "resetting another administrator needs two-factor sign-in on your own account: turn it on first, or use the command line on the server",
            ));
        }
        Ok(())
    }

    /// All of an account's devices, revoked ones too; `NotFound` for a user that isn't there.
    pub async fn admin_devices(&self, target: UserId) -> Result<Vec<DeviceRecord>, MetaError> {
        let found = sqlx::query("SELECT 1 FROM users WHERE id = $1").bind(uuid_bytes(&target.0)).fetch_optional(&self.pool).await?;
        if found.is_none() {
            return Err(MetaError::NotFound("user"));
        }
        self.list_devices(target).await
    }

    /// Signs one of `target`'s devices out ([`RevokeReason::Admin`]) and deletes the pairing it made. A device that is
    /// revoked already stays as it is. A device of another account, or an unknown user, is `NotFound`.
    pub async fn admin_revoke_device(&self, actor: &AdminActor, target: UserId, device: DeviceId, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_admin(actor, Some(target), false).await?;
        Self::target(&mut w, target).await?;
        let done = sqlx::query("UPDATE devices SET revoked = 1, revoked_ms = $1, revoke_reason = $2 WHERE id = $3 AND user_id = $4 AND revoked = 0")
            .bind(now_ms)
            .bind(RevokeReason::Admin.as_str())
            .bind(uuid_bytes(device.as_uuid()))
            .bind(uuid_bytes(&target.0))
            .execute(&mut *w.tx)
            .await?;
        if done.rows_affected() == 0 {
            let owned = sqlx::query("SELECT 1 FROM devices WHERE id = $1 AND user_id = $2")
                .bind(uuid_bytes(device.as_uuid()))
                .bind(uuid_bytes(&target.0))
                .fetch_optional(&mut *w.tx)
                .await?;
            if owned.is_none() {
                return Err(MetaError::NotFound("device"));
            }
        }
        sqlx::query("DELETE FROM pairings WHERE device_id = $1").bind(uuid_bytes(device.as_uuid())).execute(&mut *w.tx).await?;
        // The event is about the account, whose own activity then shows it, and names the device. A device signed out
        // already is no change.
        if done.rows_affected() > 0 {
            let details = AuditDetails { device: Some(device), reason: Some(RevokeReason::Admin), ..AuditDetails::default() };
            record_in(&mut w.tx, now_ms, by_admin(actor, AuditAction::AdminDeviceRevoked, target).with(details)).await?;
        }
        self.commit_audited(w, now_ms).await
    }
}
