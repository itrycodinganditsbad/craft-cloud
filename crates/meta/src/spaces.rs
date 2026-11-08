//! Team spaces and their members: who may do what in a space, and the changes that decide it.
//!
//! - **Access** is one function, [`Meta::space_access`], one indexed query: a personal space's owner is its `Owner`; in a
//!   team space the role of an *accepted* member counts (an archived space gives at most `Viewer`); a disabled or
//!   removed account has none anywhere. Nothing is cached.
//! - **Members** are rows of `space_members`. An invitation is a row without `accepted_ms` and gives no access. A
//!   declined invitation, a member who leaves and a removed member are deleted rows, and the audit log keeps the history.
//! - **A team space always has an accepted owner.** Lowering, removing or letting the last accepted owner leave is
//!   [`MetaError::LastOwner`]. Every change to a space's members locks the space's row first (`FOR UPDATE` on Postgres,
//!   the single writer on SQLite), re-reads the actor's own role inside the transaction, and counts the owners after
//!   that lock, so two changes that would each leave the other owner behind cannot both commit, and a change by someone
//!   whose role was lowered meanwhile is refused.
//! - **Who acts** is a [`SpaceActor`]: an account by its own space role, an administrator (checked again in the
//!   transaction like every admin change), or the command line. Only an administrator or the command line creates a
//!   team space, and the command line is the only actor whose invitation needs no acceptance.
//! - **Audit**: every change writes its event in its own transaction ([`AuditAction::SpaceCreated`] and the rest). A
//!   change that changes nothing writes none. Events hold IDs and role words only, never a space name.
//!
//! Lock order: users (by ID), then the space row, then its members. No other module takes them in another order.

use craft_cloud_proto::api::SpaceRole;
use craft_cloud_proto::{DeviceId, Epoch, NodeId, SpaceId, SpaceName};
use sqlx::Row;
use sqlx::any::AnyRow;

use crate::admin::{AdminActor, AdminVia};
use crate::audit::{AuditAction, AuditDetails, NewEvent, record_in};
use crate::codec::{to_i64, uuid_bytes, uuid_from};
use crate::users::{account_blocked, lock_user};
use crate::{Dialect, MIN_QUOTA_BYTES, Meta, MetaError, UserId, WriteTx};

/// Who makes a change to a team space or its members.
#[derive(Debug, Clone)]
pub enum SpaceActor {
    /// A signed-in account, acting by its role in the space (checked in the transaction). `device` is the device the
    /// request came with; it must still be signed in.
    User { user: UserId, device: Option<DeviceId> },
    /// An administrator, checked again in the transaction as for every admin change.
    Admin(AdminActor),
    /// The command line on the server's host: no account acts, and its events say so.
    Cli,
}

/// A member of a team space, or an account invited to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub user: UserId,
    pub username: String,
    pub role: SpaceRole,
    pub invited_ms: i64,
    /// The account that invited it; `None` for the command line.
    pub invited_by: Option<UserId>,
    /// `None` while the invitation is pending.
    pub accepted_ms: Option<i64>,
}

impl Member {
    /// Whether the account has accepted, which is what access needs.
    pub fn accepted(&self) -> bool {
        self.accepted_ms.is_some()
    }
}

/// A team space as a change finds it after locking its row.
struct Locked {
    archived: bool,
}

const MEMBER_SELECT: &str = "SELECT m.user_id, u.username, m.role, m.invited_ms, m.invited_by, m.accepted_ms \
     FROM space_members m JOIN users u ON u.id = m.user_id";

fn member_from(row: &AnyRow) -> Result<Member, MetaError> {
    Ok(Member {
        user: UserId(uuid_from(&row.try_get::<Vec<u8>, _>("user_id")?)?),
        username: row.try_get("username")?,
        role: parse_role(&row.try_get::<String, _>("role")?)?,
        invited_ms: row.try_get("invited_ms")?,
        invited_by: row.try_get::<Option<Vec<u8>>, _>("invited_by")?.as_deref().map(uuid_from).transpose()?.map(UserId),
        accepted_ms: row.try_get("accepted_ms")?,
    })
}

fn parse_role(word: &str) -> Result<SpaceRole, MetaError> {
    SpaceRole::parse(word).ok_or(MetaError::Corrupt("unknown space role"))
}

/// The event of a change `actor` makes. The command line has no actor, and its events have names of their own:
/// `cli.space_created` for a creation, `cli.space_member_changed` for every change to the members.
fn event_for(actor: &SpaceActor, action: AuditAction, space: SpaceId) -> NewEvent {
    let event = match actor {
        SpaceActor::User { user, device } => NewEvent::new(action).by(*user, *device),
        SpaceActor::Admin(admin) => {
            let device = match admin.via {
                AdminVia::Device(device) => Some(device),
                AdminVia::Web { .. } => None,
            };
            NewEvent::new(action).by(admin.user, device)
        }
        SpaceActor::Cli => NewEvent::new(match action {
            AuditAction::SpaceCreated => AuditAction::CliSpaceCreated,
            _ => AuditAction::CliSpaceMemberChanged,
        }),
    };
    event.on_space(space)
}

impl Meta {
    /// What `user` may do in `space`: `None` for no access at all, which is also the answer for a space that does not
    /// exist (a caller must not tell the two apart). See the module's rules.
    pub async fn space_access(&self, user: UserId, space: SpaceId) -> Result<Option<SpaceRole>, MetaError> {
        let row = sqlx::query(
            "SELECT s.kind, s.owner, s.archived_ms, m.role, m.accepted_ms, u.disabled_ms, u.removed_ms \
             FROM spaces s JOIN users u ON u.id = $1 LEFT JOIN space_members m ON m.space_id = s.id AND m.user_id = u.id \
             WHERE s.id = $2",
        )
        .bind(uuid_bytes(&user.0))
        .bind(uuid_bytes(space.as_uuid()))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        if row.try_get::<Option<i64>, _>("disabled_ms")?.is_some() || row.try_get::<Option<i64>, _>("removed_ms")?.is_some() {
            return Ok(None);
        }
        match row.try_get::<String, _>("kind")?.as_str() {
            "personal" => {
                let owner: Option<Vec<u8>> = row.try_get("owner")?;
                Ok(owner.as_deref().map(uuid_from).transpose()?.filter(|owner| *owner == user.0).map(|_| SpaceRole::Owner))
            }
            "team" => {
                let role: Option<String> = row.try_get("role")?;
                let accepted: Option<i64> = row.try_get("accepted_ms")?;
                let (Some(role), Some(_)) = (role, accepted) else { return Ok(None) };
                let role = parse_role(&role)?;
                let archived = row.try_get::<Option<i64>, _>("archived_ms")?.is_some();
                Ok(Some(if archived { role.min(SpaceRole::Viewer) } else { role }))
            }
            _ => Err(MetaError::Corrupt("unknown space kind")),
        }
    }

    /// The members of a team space, accepted and invited, by username; `NotFound` for a space that is not a team space.
    /// Who may ask is the caller's to decide ([`Meta::space_access`]).
    pub async fn list_members(&self, space: SpaceId) -> Result<Vec<Member>, MetaError> {
        let mut tx = self.read_snapshot().await?;
        let kind = sqlx::query("SELECT kind FROM spaces WHERE id = $1").bind(uuid_bytes(space.as_uuid())).fetch_optional(&mut *tx).await?;
        match kind {
            Some(row) if row.try_get::<String, _>("kind")? == "team" => {}
            _ => return Err(MetaError::NotFound("space")),
        }
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!("{MEMBER_SELECT} WHERE m.space_id = $1 ORDER BY u.username, m.user_id")))
            .bind(uuid_bytes(space.as_uuid()))
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        rows.iter().map(member_from).collect()
    }

    /// Creates a team space called `name` with `owner` as its first owner, who is a member from the start (naming the first
    /// owner is the consent step for that one row), and a root directory like every space. `quota_bytes` is the space's
    /// own limit (at least [`MIN_QUOTA_BYTES`]); `None`: the server's default applies. Only an administrator or the command
    /// line may ([`SpaceActor::User`] is `Forbidden`). An owner who is not an account that can sign in is `NotFound("user")`;
    /// a name another team space has, in any case, is `Conflict`.
    pub async fn create_team_space(
        &self,
        actor: &SpaceActor,
        name: &SpaceName,
        owner: UserId,
        quota_bytes: Option<u64>,
        now_ms: i64,
    ) -> Result<SpaceId, MetaError> {
        if let Some(bytes) = quota_bytes
            && !(MIN_QUOTA_BYTES..=i64::MAX as u64).contains(&bytes)
        {
            return Err(MetaError::Invalid("quota_bytes is at least one chunk (4194304) and at most 9223372036854775807"));
        }
        let mut w = match actor {
            SpaceActor::User { .. } => return Err(MetaError::Forbidden("only an administrator creates team spaces")),
            SpaceActor::Admin(admin) => self.begin_admin(admin, Some(owner), false).await?,
            SpaceActor::Cli => {
                let mut w = self.begin_write().await?;
                lock_user(&mut w.tx, owner).await?;
                w
            }
        };
        if !user_can_join(&mut w, owner).await? {
            return Err(MetaError::NotFound("user"));
        }
        let fold = name.fold_key();
        let taken = sqlx::query("SELECT 1 FROM spaces WHERE kind = 'team' AND name_fold = $1").bind(fold.as_str()).fetch_optional(&mut *w.tx).await?;
        if taken.is_some() {
            return Err(MetaError::Conflict("a team space with this name exists"));
        }
        let (space, root) = (SpaceId::generate(), NodeId::generate());
        sqlx::query(
            "INSERT INTO spaces (id, owner, epoch, seq, oldest_seq, root, quota_bytes, kind, name, name_fold, created_ms) \
             VALUES ($1, NULL, $2, 0, 0, $3, $4, 'team', $5, $6, $7)",
        )
        .bind(uuid_bytes(space.as_uuid()))
        .bind(uuid_bytes(Epoch::generate().as_uuid()))
        .bind(uuid_bytes(root.as_uuid()))
        .bind(quota_bytes.map(to_i64).transpose()?)
        .bind(name.as_str())
        .bind(fold.as_str())
        .bind(now_ms)
        .execute(&mut *w.tx)
        .await
        // The check above and this insert are in one transaction, but another process on one Postgres may have taken the
        // name in between: the unique index turns the loser into the same answer.
        .map_err(|e| match e {
            sqlx::Error::Database(ref db) if db.is_unique_violation() => MetaError::Conflict("a team space with this name exists"),
            other => MetaError::Db(other),
        })?;
        sqlx::query(
            "INSERT INTO nodes (space_id, id, parent, name, fold_key, is_dir, version, deleted, changed_seq) \
             VALUES ($1, $2, NULL, '', '', 1, NULL, 0, 0)",
        )
        .bind(uuid_bytes(space.as_uuid()))
        .bind(uuid_bytes(root.as_uuid()))
        .execute(&mut *w.tx)
        .await?;
        insert_member(&mut w, space, owner, SpaceRole::Owner, actor_user(actor), now_ms, true).await?;
        let details = AuditDetails { user: Some(owner), quota_bytes: quota_bytes.map(Some), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(actor, AuditAction::SpaceCreated, space).with(details)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(space)
    }

    /// Invites `target` to the team space as `role`: a pending invitation that gives no access until the account accepts
    /// ([`Meta::accept_invitation`]). The command line's invitation is accepted at once (whoever runs it has the files
    /// anyway). Owners and administrators may; an account that does not exist, is disabled or is marked for removal is
    /// `NotFound("user")` (one answer for all three); one that is a member or invited already is `Conflict`.
    pub async fn invite_member(&self, actor: &SpaceActor, space: SpaceId, target: UserId, role: SpaceRole, now_ms: i64) -> Result<Member, MetaError> {
        let (mut w, _) = self.begin_space_change(actor, space, Some(target), true).await?;
        if !user_can_join(&mut w, target).await? {
            return Err(MetaError::NotFound("user"));
        }
        if member_row(&mut w, space, target).await?.is_some() {
            return Err(MetaError::Conflict("this account is a member of the space or invited to it already"));
        }
        insert_member(&mut w, space, target, role, actor_user(actor), now_ms, matches!(actor, SpaceActor::Cli)).await?;
        let details = AuditDetails { user: Some(target), space_role_to: Some(role), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(actor, AuditAction::SpaceMemberInvited, space).with(details)).await?;
        let member = member_row(&mut w, space, target).await?.ok_or(MetaError::Corrupt("a new member is missing"))?;
        self.commit_audited(w, now_ms).await?;
        Ok(member)
    }

    /// The invited account accepts: it is a member with the invited role from now on, which is returned. No invitation
    /// (never made, withdrawn, declined, or accepted already) is `NotFound("invitation")` and writes nothing.
    pub async fn accept_invitation(&self, user: UserId, device: Option<DeviceId>, space: SpaceId, now_ms: i64) -> Result<SpaceRole, MetaError> {
        let actor = SpaceActor::User { user, device };
        let (mut w, _) = self.begin_space_change(&actor, space, None, false).await?;
        let row =
            sqlx::query("UPDATE space_members SET accepted_ms = $1 WHERE space_id = $2 AND user_id = $3 AND accepted_ms IS NULL RETURNING role")
                .bind(now_ms)
                .bind(uuid_bytes(space.as_uuid()))
                .bind(uuid_bytes(&user.0))
                .fetch_optional(&mut *w.tx)
                .await?
                .ok_or(MetaError::NotFound("invitation"))?;
        let role = parse_role(&row.try_get::<String, _>("role")?)?;
        let details = AuditDetails { user: Some(user), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(&actor, AuditAction::SpaceMemberJoined, space).with(details)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(role)
    }

    /// The invited account declines: the invitation is deleted. No invitation is `NotFound("invitation")` and writes
    /// nothing; a member who has accepted leaves with [`Meta::leave_space`].
    pub async fn decline_invitation(&self, user: UserId, device: Option<DeviceId>, space: SpaceId, now_ms: i64) -> Result<(), MetaError> {
        let actor = SpaceActor::User { user, device };
        let (mut w, _) = self.begin_space_change(&actor, space, None, false).await?;
        let done = sqlx::query("DELETE FROM space_members WHERE space_id = $1 AND user_id = $2 AND accepted_ms IS NULL")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::NotFound("invitation"));
        }
        let details = AuditDetails { user: Some(user), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(&actor, AuditAction::SpaceMemberDeclined, space).with(details)).await?;
        self.commit_audited(w, now_ms).await
    }

    /// Gives a member (or an invitation) another role, and returns the row. The role it has already is no change and
    /// writes no event. Lowering the last accepted owner is [`MetaError::LastOwner`]. A target without a row is
    /// `NotFound("member")`.
    pub async fn change_member_role(
        &self,
        actor: &SpaceActor,
        space: SpaceId,
        target: UserId,
        role: SpaceRole,
        now_ms: i64,
    ) -> Result<Member, MetaError> {
        let (mut w, _) = self.begin_space_change(actor, space, Some(target), true).await?;
        let current = member_row(&mut w, space, target).await?.ok_or(MetaError::NotFound("member"))?;
        if current.role == role {
            return Ok(current);
        }
        if current.role == SpaceRole::Owner && current.accepted() {
            keep_an_owner(&mut w, space).await?;
        }
        sqlx::query("UPDATE space_members SET role = $1 WHERE space_id = $2 AND user_id = $3")
            .bind(role.as_str())
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(&target.0))
            .execute(&mut *w.tx)
            .await?;
        let details = AuditDetails { user: Some(target), space_role_from: Some(current.role), space_role_to: Some(role), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(actor, AuditAction::SpaceMemberRoleChanged, space).with(details)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(Member { role, ..current })
    }

    /// Removes a member, or withdraws an invitation. Nothing in the space is deleted. Removing the last accepted owner is
    /// [`MetaError::LastOwner`]; a target without a row is `NotFound("member")`. An account removing its own row is
    /// [`Meta::leave_space`].
    pub async fn remove_member(&self, actor: &SpaceActor, space: SpaceId, target: UserId, now_ms: i64) -> Result<(), MetaError> {
        if let SpaceActor::User { user, device } = actor
            && *user == target
        {
            return self.leave_space(*user, *device, space, now_ms).await;
        }
        let (mut w, _) = self.begin_space_change(actor, space, Some(target), true).await?;
        let current = member_row(&mut w, space, target).await?.ok_or(MetaError::NotFound("member"))?;
        if current.role == SpaceRole::Owner && current.accepted() {
            keep_an_owner(&mut w, space).await?;
        }
        delete_member(&mut w, space, target).await?;
        let details = AuditDetails { user: Some(target), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(actor, AuditAction::SpaceMemberRemoved, space).with(details)).await?;
        self.commit_audited(w, now_ms).await
    }

    /// A member leaves, whatever their role. The last accepted owner leaving is [`MetaError::LastOwner`]. An account
    /// that is only invited declines instead ([`Meta::decline_invitation`]): `NotFound("member")` here.
    pub async fn leave_space(&self, user: UserId, device: Option<DeviceId>, space: SpaceId, now_ms: i64) -> Result<(), MetaError> {
        let actor = SpaceActor::User { user, device };
        let (mut w, _) = self.begin_space_change(&actor, space, None, false).await?;
        let current = member_row(&mut w, space, user).await?.filter(Member::accepted).ok_or(MetaError::NotFound("member"))?;
        if current.role == SpaceRole::Owner {
            keep_an_owner(&mut w, space).await?;
        }
        delete_member(&mut w, space, user).await?;
        record_in(&mut w.tx, now_ms, event_for(&actor, AuditAction::SpaceMemberLeft, space)).await?;
        self.commit_audited(w, now_ms).await
    }

    /// Starts the write transaction of a change to a team space's members: checks the actor, takes the locks in the module's
    /// order and, for a change only an owner may make (`owner_only`), reads the actor's role again under the space's lock.
    /// `target` is the account the change is about, if it has one. A space that is not a team space, or an account with
    /// no role in it, gets `NotFound("space")`: the two are not told apart; a member without the right role is `Forbidden`.
    async fn begin_space_change(
        &self,
        actor: &SpaceActor,
        space: SpaceId,
        target: Option<UserId>,
        owner_only: bool,
    ) -> Result<(WriteTx, Locked), MetaError> {
        let mut w = match actor {
            SpaceActor::Admin(admin) => self.begin_admin(admin, target, false).await?,
            SpaceActor::User { user, device } => {
                let mut w = self.begin_write().await?;
                let mut ids = vec![*user];
                ids.extend(target.filter(|target| target != user));
                ids.sort_by_key(|id| *id.0.as_bytes());
                for id in ids {
                    lock_user(&mut w.tx, id).await?;
                }
                // The account and the device the request came with must still be signed in.
                let known = sqlx::query("SELECT 1 FROM users WHERE id = $1").bind(uuid_bytes(&user.0)).fetch_optional(&mut *w.tx).await?;
                if known.is_none() || account_blocked(&mut w.tx, *user).await? {
                    return Err(MetaError::DeviceRevoked);
                }
                if let Some(device) = device {
                    let active = sqlx::query("SELECT 1 FROM devices WHERE id = $1 AND user_id = $2 AND revoked = 0")
                        .bind(uuid_bytes(device.as_uuid()))
                        .bind(uuid_bytes(&user.0))
                        .fetch_optional(&mut *w.tx)
                        .await?;
                    if active.is_none() {
                        return Err(MetaError::DeviceRevoked);
                    }
                }
                w
            }
            SpaceActor::Cli => {
                let mut w = self.begin_write().await?;
                if let Some(target) = target {
                    lock_user(&mut w.tx, target).await?;
                }
                w
            }
        };
        let sql = match self.dialect {
            Dialect::Postgres => "SELECT kind, archived_ms FROM spaces WHERE id = $1 FOR UPDATE",
            Dialect::Sqlite => "SELECT kind, archived_ms FROM spaces WHERE id = $1",
        };
        let row = sqlx::query(sql).bind(uuid_bytes(space.as_uuid())).fetch_optional(&mut *w.tx).await?;
        let locked = match row {
            Some(row) if row.try_get::<String, _>("kind")? == "team" => Locked { archived: row.try_get::<Option<i64>, _>("archived_ms")?.is_some() },
            _ => return Err(MetaError::NotFound("space")),
        };
        if let (SpaceActor::User { user, .. }, true) = (actor, owner_only) {
            // Under the space's lock, so a role lowered by a change that committed meanwhile is respected.
            let role = match member_row(&mut w, space, *user).await? {
                Some(member) if member.accepted() => Some(if locked.archived { member.role.min(SpaceRole::Viewer) } else { member.role }),
                _ => None,
            };
            match role {
                Some(SpaceRole::Owner) => {}
                Some(_) => return Err(MetaError::Forbidden("only an owner of the space may do this")),
                None => return Err(MetaError::NotFound("space")),
            }
        }
        Ok((w, locked))
    }
}

/// The account that acts, for a row that names who invited: none for the command line.
fn actor_user(actor: &SpaceActor) -> Option<UserId> {
    match actor {
        SpaceActor::User { user, .. } => Some(*user),
        SpaceActor::Admin(admin) => Some(admin.user),
        SpaceActor::Cli => None,
    }
}

/// Whether `user` exists and can sign in (not disabled, not marked for removal), read after its row was locked.
async fn user_can_join(w: &mut WriteTx, user: UserId) -> Result<bool, MetaError> {
    let found = sqlx::query("SELECT 1 FROM users WHERE id = $1").bind(uuid_bytes(&user.0)).fetch_optional(&mut *w.tx).await?;
    Ok(found.is_some() && !account_blocked(&mut w.tx, user).await?)
}

async fn member_row(w: &mut WriteTx, space: SpaceId, user: UserId) -> Result<Option<Member>, MetaError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!("{MEMBER_SELECT} WHERE m.space_id = $1 AND m.user_id = $2")))
        .bind(uuid_bytes(space.as_uuid()))
        .bind(uuid_bytes(&user.0))
        .fetch_optional(&mut *w.tx)
        .await?;
    row.as_ref().map(member_from).transpose()
}

async fn insert_member(
    w: &mut WriteTx,
    space: SpaceId,
    user: UserId,
    role: SpaceRole,
    invited_by: Option<UserId>,
    now_ms: i64,
    accepted: bool,
) -> Result<(), MetaError> {
    sqlx::query("INSERT INTO space_members (space_id, user_id, role, invited_ms, invited_by, accepted_ms) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(uuid_bytes(space.as_uuid()))
        .bind(uuid_bytes(&user.0))
        .bind(role.as_str())
        .bind(now_ms)
        .bind(invited_by.map(|by| uuid_bytes(&by.0)))
        .bind(accepted.then_some(now_ms))
        .execute(&mut *w.tx)
        .await?;
    Ok(())
}

async fn delete_member(w: &mut WriteTx, space: SpaceId, user: UserId) -> Result<(), MetaError> {
    sqlx::query("DELETE FROM space_members WHERE space_id = $1 AND user_id = $2")
        .bind(uuid_bytes(space.as_uuid()))
        .bind(uuid_bytes(&user.0))
        .execute(&mut *w.tx)
        .await?;
    Ok(())
}

/// Refuses with [`MetaError::LastOwner`] unless the space has an accepted owner besides the one the change takes away.
/// Called with the space's row locked, for a change to an accepted owner's row.
async fn keep_an_owner(w: &mut WriteTx, space: SpaceId) -> Result<(), MetaError> {
    let owners: i64 = sqlx::query("SELECT COUNT(*) AS n FROM space_members WHERE space_id = $1 AND role = 'owner' AND accepted_ms IS NOT NULL")
        .bind(uuid_bytes(space.as_uuid()))
        .fetch_one(&mut *w.tx)
        .await?
        .try_get("n")?;
    if owners < 2 {
        return Err(MetaError::LastOwner);
    }
    Ok(())
}
