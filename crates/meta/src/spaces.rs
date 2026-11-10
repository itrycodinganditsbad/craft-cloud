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
use crate::codec::{to_i64, to_u64, uuid_bytes, uuid_from};
use crate::users::{account_blocked, lock_user};
use crate::{Dialect, MIN_QUOTA_BYTES, Meta, MetaError, UserId, WriteTx};

/// Who makes a change to a team space or its members.
#[derive(Debug, Clone)]
pub enum SpaceActor {
    /// A signed-in account, acting by its role in the space (checked in the transaction). `device` is the device the
    /// request came with; it must still be signed in. `None` is for the command line's tests and callers that have no
    /// device (no session is checked); the JSON routes always pass the request's device, and no route of the space API takes
    /// a browser session (the admin pages' own routes carry theirs in [`AdminVia::Web`]).
    User { user: UserId, device: Option<DeviceId> },
    /// An administrator, checked again in the transaction as for every admin change.
    Admin(AdminActor),
    /// The command line on the server's host: no account acts, and its events say so.
    Cli,
}

/// The role a write needs and the account that asks, for [`Meta::guarded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteGuard {
    pub user: UserId,
    pub min: SpaceRole,
}

/// A space as `GET /spaces` lists it for one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSpace {
    pub id: SpaceId,
    pub personal: bool,
    /// The team space's name; `None` for a personal space.
    pub name: Option<String>,
    pub role: SpaceRole,
    /// An invitation the account has not accepted.
    pub invited: bool,
    pub archived: bool,
    pub created_ms: i64,
    /// Accepted members (1 for a personal space).
    pub members: u64,
    /// `None` until the invitation is accepted.
    pub root: Option<NodeId>,
    pub epoch: Option<Epoch>,
    /// When the account accepted its membership (milliseconds). A member who is removed and invited again gets a new one;
    /// `None` for a personal space and for an invitation.
    pub joined_ms: Option<i64>,
}

/// A team space as the administrator's list shows it: counts, never content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceSummary {
    pub id: SpaceId,
    pub name: String,
    /// The folded name, which the list is ordered and paged by.
    pub name_fold: String,
    pub created_ms: i64,
    pub archived_ms: Option<i64>,
    /// Accepted members, owners included.
    pub members: u64,
    /// Accepted owners whose accounts can sign in: an owner who is disabled or marked for removal has no access (see
    /// [`Meta::space_access`]) and does not count, so 0 means nobody can manage the space.
    pub owners: u64,
    pub stored_bytes: u64,
    pub quota_bytes: Option<u64>,
}

/// One page of [`Meta::admin_list_spaces`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpacePage {
    pub spaces: Vec<SpaceSummary>,
    /// Where the next page starts (the last name key of this one); `None` on the last page.
    pub next_after: Option<String>,
}

const SUMMARY_SELECT: &str = "SELECT s.id, s.name, s.name_fold, s.created_ms, s.archived_ms, s.stored_bytes, s.quota_bytes, \
     (SELECT CAST(COUNT(*) AS BIGINT) FROM space_members m WHERE m.space_id = s.id AND m.accepted_ms IS NOT NULL) AS members, \
     (SELECT CAST(COUNT(*) AS BIGINT) FROM space_members m JOIN users u ON u.id = m.user_id       WHERE m.space_id = s.id AND m.accepted_ms IS NOT NULL AND m.role = 'owner' AND u.disabled_ms IS NULL AND u.removed_ms IS NULL) AS owners \
     FROM spaces s WHERE s.kind = 'team'";

fn summary_from(row: &AnyRow) -> Result<SpaceSummary, MetaError> {
    Ok(SpaceSummary {
        id: SpaceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
        name: row.try_get::<Option<String>, _>("name")?.ok_or(MetaError::Corrupt("a team space has no name"))?,
        name_fold: row.try_get::<Option<String>, _>("name_fold")?.ok_or(MetaError::Corrupt("a team space has no name key"))?,
        created_ms: row.try_get("created_ms")?,
        archived_ms: row.try_get("archived_ms")?,
        members: to_u64(row.try_get("members")?)?,
        owners: to_u64(row.try_get("owners")?)?,
        stored_bytes: to_u64(row.try_get("stored_bytes")?)?,
        quota_bytes: row.try_get::<Option<i64>, _>("quota_bytes")?.map(to_u64).transpose()?,
    })
}

/// An accepted owner of a team space, with the state of the account (for the administrator's pages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceOwner {
    pub space: SpaceId,
    pub user: UserId,
    pub username: String,
    pub disabled_ms: Option<i64>,
    pub removed_ms: Option<i64>,
}

impl SpaceOwner {
    /// Whether the account can sign in, which is what an owner's power needs.
    pub fn active(&self) -> bool {
        self.disabled_ms.is_none() && self.removed_ms.is_none()
    }
}

/// `text` as a `LIKE` pattern that matches it literally (`\` is the escape character in the queries above).
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
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

/// What a change asks of a [`SpaceActor::User`] under the space's lock.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    /// A row of their own in the space, any state (accept, decline, leave).
    Row,
    /// An accepted `Owner` row, with the archive's cap on roles applied.
    Owner,
    /// An accepted `Owner` row, whatever the space's state: an owner may archive and unarchive their space.
    OwnerEvenIfArchived,
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
/// `cli.space_created` for a creation, `cli.space_member_changed` for an invitation, a role change or a removal and
/// `cli.space_archived` for an archive or unarchive. It makes
/// no other change (a member's own acts are theirs), and `Corrupt` says so if one is ever asked of it.
fn event_for(actor: &SpaceActor, action: AuditAction, space: SpaceId) -> Result<NewEvent, MetaError> {
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
            AuditAction::SpaceMemberInvited | AuditAction::SpaceMemberRoleChanged | AuditAction::SpaceMemberRemoved => {
                AuditAction::CliSpaceMemberChanged
            }
            AuditAction::SpaceArchived | AuditAction::SpaceUnarchived => AuditAction::CliSpaceArchived,
            _ => return Err(MetaError::Corrupt("the command line makes no such change")),
        }),
    };
    Ok(event.on_space(space))
}

impl Meta {
    /// What `user` may do in `space`: `None` for no access at all, which is also the answer for a space that does not
    /// exist (a caller must not tell the two apart). See the module's rules.
    pub async fn space_access(&self, user: UserId, space: SpaceId) -> Result<Option<SpaceRole>, MetaError> {
        let mut conn = self.pool.acquire().await?;
        access_on(&mut conn, user, space).await
    }

    /// A handle whose space writes (commit, restore, save, upload, removal from the trash) re-check `guard` inside their
    /// write transaction, after the space's row is locked: a role lowered, a member removed or a space archived by a change
    /// that committed between the server's access check and the write is respected. A refused write changes nothing and is
    /// `NotFound("space")` (no access any more) or [`MetaError::ReadOnly`]. Cheap: a clone with one field set. Only the
    /// space writes look at it.
    pub fn guarded(&self, guard: WriteGuard) -> Meta {
        Meta { guard: Some(guard), ..self.clone() }
    }

    /// Runs the guard of a [`Meta::guarded`] handle, if there is one, on the write transaction's connection. On Postgres
    /// the space's row is locked first, at the strength the write needs anyway (`FOR NO KEY UPDATE`: an upload updates the
    /// row's byte count, and a weaker lock taken first would make two concurrent uploads wait for each other's upgrade and
    /// deadlock); a change to the space's members, which takes `FOR UPDATE`, waits for it and the other way round. On SQLite
    /// the single writer already orders everything.
    pub(crate) async fn check_guard(&self, conn: &mut sqlx::AnyConnection, space: SpaceId) -> Result<(), MetaError> {
        let Some(guard) = self.guard else { return Ok(()) };
        if self.dialect == Dialect::Postgres {
            sqlx::query("SELECT 1 FROM spaces WHERE id = $1 FOR NO KEY UPDATE").bind(uuid_bytes(space.as_uuid())).fetch_optional(&mut *conn).await?;
        }
        match access_on(conn, guard.user, space).await? {
            None => Err(MetaError::NotFound("space")),
            Some(role) if role < guard.min => Err(MetaError::ReadOnly),
            Some(_) => Ok(()),
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
        let details = AuditDetails { user: Some(owner), quota_bytes: Some(quota_bytes), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(actor, AuditAction::SpaceCreated, space)?.with(details)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(space)
    }

    /// Invites `target` to the team space as `role`: a pending invitation that gives no access until the account accepts
    /// ([`Meta::accept_invitation`]). The command line's invitation is accepted at once (whoever runs it has the files
    /// anyway). Owners and administrators may; an account that does not exist, is disabled or is marked for removal is
    /// `NotFound("user")` (one answer for all three); one that is a member or invited already is `Conflict`.
    pub async fn invite_member(&self, actor: &SpaceActor, space: SpaceId, target: UserId, role: SpaceRole, now_ms: i64) -> Result<Member, MetaError> {
        let (mut w, _) = self.begin_space_change(actor, space, Some(target), Need::Owner).await?;
        if !user_can_join(&mut w, target).await? {
            return Err(MetaError::NotFound("user"));
        }
        if member_row(&mut w, space, target).await?.is_some() {
            return Err(MetaError::Conflict("this account is a member of the space or invited to it already"));
        }
        insert_member(&mut w, space, target, role, actor_user(actor), now_ms, matches!(actor, SpaceActor::Cli)).await?;
        let details = AuditDetails { user: Some(target), space_role_to: Some(role), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(actor, AuditAction::SpaceMemberInvited, space)?.with(details)).await?;
        let member = member_row(&mut w, space, target).await?.ok_or(MetaError::Corrupt("a new member is missing"))?;
        self.commit_audited(w, now_ms).await?;
        Ok(member)
    }

    /// The invited account accepts: it is a member with the invited role from now on, which is returned. No invitation
    /// (never made, withdrawn, declined, or accepted already) is `NotFound("invitation")` and writes nothing.
    pub async fn accept_invitation(&self, user: UserId, device: Option<DeviceId>, space: SpaceId, now_ms: i64) -> Result<SpaceRole, MetaError> {
        let actor = SpaceActor::User { user, device };
        let (mut w, locked) = self.begin_space_change(&actor, space, None, Need::Row).await?;
        // An archived space is not listed to an invitee (it is read-only for everyone): the answer is the one for no space.
        if locked.archived {
            return Err(MetaError::NotFound("space"));
        }
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
        record_in(&mut w.tx, now_ms, event_for(&actor, AuditAction::SpaceMemberJoined, space)?.with(details)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(role)
    }

    /// The invited account declines: the invitation is deleted. No invitation is `NotFound("invitation")` and writes
    /// nothing; a member who has accepted leaves with [`Meta::leave_space`].
    pub async fn decline_invitation(&self, user: UserId, device: Option<DeviceId>, space: SpaceId, now_ms: i64) -> Result<(), MetaError> {
        let actor = SpaceActor::User { user, device };
        let (mut w, locked) = self.begin_space_change(&actor, space, None, Need::Row).await?;
        if locked.archived {
            return Err(MetaError::NotFound("space"));
        }
        let done = sqlx::query("DELETE FROM space_members WHERE space_id = $1 AND user_id = $2 AND accepted_ms IS NULL")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(&user.0))
            .execute(&mut *w.tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::NotFound("invitation"));
        }
        let details = AuditDetails { user: Some(user), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(&actor, AuditAction::SpaceMemberDeclined, space)?.with(details)).await?;
        self.commit_audited(w, now_ms).await
    }

    /// Gives a member (or an invitation) another role, and returns the row. The role it has already is no change and
    /// writes no event, except that the command line also accepts a pending invitation (whoever runs it has the files, as
    /// with [`Meta::invite_member`]). Lowering the last accepted owner is [`MetaError::LastOwner`]. A target without a row is
    /// `NotFound("member")`.
    pub async fn change_member_role(
        &self,
        actor: &SpaceActor,
        space: SpaceId,
        target: UserId,
        role: SpaceRole,
        now_ms: i64,
    ) -> Result<Member, MetaError> {
        let (mut w, _) = self.begin_space_change(actor, space, Some(target), Need::Owner).await?;
        let current = member_row(&mut w, space, target).await?.ok_or(MetaError::NotFound("member"))?;
        let accept_now = matches!(actor, SpaceActor::Cli) && !current.accepted();
        if current.role == role && !accept_now {
            return Ok(current);
        }
        if current.role == SpaceRole::Owner && role != SpaceRole::Owner && current.accepted() {
            keep_an_owner(&mut w, space).await?;
        }
        sqlx::query("UPDATE space_members SET role = $1, accepted_ms = COALESCE(accepted_ms, $2) WHERE space_id = $3 AND user_id = $4")
            .bind(role.as_str())
            .bind(accept_now.then_some(now_ms))
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(&target.0))
            .execute(&mut *w.tx)
            .await?;
        let details = AuditDetails { user: Some(target), space_role_from: Some(current.role), space_role_to: Some(role), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(actor, AuditAction::SpaceMemberRoleChanged, space)?.with(details)).await?;
        self.commit_audited(w, now_ms).await?;
        Ok(Member { role, accepted_ms: current.accepted_ms.or(accept_now.then_some(now_ms)), ..current })
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
        let (mut w, _) = self.begin_space_change(actor, space, Some(target), Need::Owner).await?;
        let current = member_row(&mut w, space, target).await?.ok_or(MetaError::NotFound("member"))?;
        if current.role == SpaceRole::Owner && current.accepted() {
            keep_an_owner(&mut w, space).await?;
        }
        delete_member(&mut w, space, target).await?;
        let details = AuditDetails { user: Some(target), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(actor, AuditAction::SpaceMemberRemoved, space)?.with(details)).await?;
        self.commit_audited(w, now_ms).await
    }

    /// A member leaves, whatever their role. The last accepted owner leaving is [`MetaError::LastOwner`]. An account
    /// that is only invited declines instead ([`Meta::decline_invitation`]): `NotFound("member")` here.
    pub async fn leave_space(&self, user: UserId, device: Option<DeviceId>, space: SpaceId, now_ms: i64) -> Result<(), MetaError> {
        let actor = SpaceActor::User { user, device };
        let (mut w, _) = self.begin_space_change(&actor, space, None, Need::Row).await?;
        let current = member_row(&mut w, space, user).await?.filter(Member::accepted).ok_or(MetaError::NotFound("member"))?;
        if current.role == SpaceRole::Owner {
            keep_an_owner(&mut w, space).await?;
        }
        delete_member(&mut w, space, user).await?;
        record_in(&mut w.tx, now_ms, event_for(&actor, AuditAction::SpaceMemberLeft, space)?).await?;
        self.commit_audited(w, now_ms).await
    }

    /// Renames a team space. Owners and administrators may; a name another team space has, in any case, is `Conflict`; the
    /// name it has already, letter for letter, is no change and writes no event. The event holds no name.
    pub async fn rename_team_space(&self, actor: &SpaceActor, space: SpaceId, name: &SpaceName, now_ms: i64) -> Result<(), MetaError> {
        let (mut w, _) = self.begin_space_change(actor, space, None, Need::Owner).await?;
        let current = sqlx::query("SELECT name FROM spaces WHERE id = $1").bind(uuid_bytes(space.as_uuid())).fetch_one(&mut *w.tx).await?;
        if current.try_get::<Option<String>, _>("name")?.as_deref() == Some(name.as_str()) {
            return Ok(());
        }
        let fold = name.fold_key();
        let taken = sqlx::query("SELECT 1 FROM spaces WHERE kind = 'team' AND name_fold = $1 AND id <> $2")
            .bind(fold.as_str())
            .bind(uuid_bytes(space.as_uuid()))
            .fetch_optional(&mut *w.tx)
            .await?;
        if taken.is_some() {
            return Err(MetaError::Conflict("a team space with this name exists"));
        }
        sqlx::query("UPDATE spaces SET name = $1, name_fold = $2 WHERE id = $3")
            .bind(name.as_str())
            .bind(fold.as_str())
            .bind(uuid_bytes(space.as_uuid()))
            .execute(&mut *w.tx)
            .await
            .map_err(|e| match e {
                sqlx::Error::Database(ref db) if db.is_unique_violation() => MetaError::Conflict("a team space with this name exists"),
                other => MetaError::Db(other),
            })?;
        record_in(&mut w.tx, now_ms, event_for(actor, AuditAction::SpaceRenamed, space)?).await?;
        self.commit_audited(w, now_ms).await
    }

    /// Archives (`archived`) or unarchives a team space: every member's access is capped at `Viewer` while it is archived,
    /// and nothing is deleted. Owners and administrators may, and an owner may unarchive a space that caps their own role.
    /// The state it has already is no change and writes no event.
    pub async fn set_space_archived(&self, actor: &SpaceActor, space: SpaceId, archived: bool, now_ms: i64) -> Result<(), MetaError> {
        let (mut w, locked) = self.begin_space_change(actor, space, None, Need::OwnerEvenIfArchived).await?;
        if locked.archived == archived {
            return Ok(());
        }
        sqlx::query("UPDATE spaces SET archived_ms = $1 WHERE id = $2")
            .bind(archived.then_some(now_ms))
            .bind(uuid_bytes(space.as_uuid()))
            .execute(&mut *w.tx)
            .await?;
        let action = if archived { AuditAction::SpaceArchived } else { AuditAction::SpaceUnarchived };
        record_in(&mut w.tx, now_ms, event_for(actor, action, space)?).await?;
        self.commit_audited(w, now_ms).await
    }

    /// Sets a team space's own limit (`None`: the server's default applies again). Administrators only. The limit it has
    /// already is no change and writes no event.
    pub async fn set_space_quota(&self, admin: &AdminActor, space: SpaceId, quota_bytes: Option<u64>, now_ms: i64) -> Result<(), MetaError> {
        if let Some(bytes) = quota_bytes
            && !(MIN_QUOTA_BYTES..=i64::MAX as u64).contains(&bytes)
        {
            return Err(MetaError::Invalid("quota_bytes is at least one chunk (4194304) and at most 9223372036854775807"));
        }
        let actor = SpaceActor::Admin(admin.clone());
        let (mut w, _) = self.begin_space_change(&actor, space, None, Need::Row).await?;
        let current: Option<i64> = sqlx::query("SELECT quota_bytes FROM spaces WHERE id = $1")
            .bind(uuid_bytes(space.as_uuid()))
            .fetch_one(&mut *w.tx)
            .await?
            .try_get("quota_bytes")?;
        let wanted = quota_bytes.map(to_i64).transpose()?;
        if current == wanted {
            return Ok(());
        }
        sqlx::query("UPDATE spaces SET quota_bytes = $1 WHERE id = $2").bind(wanted).bind(uuid_bytes(space.as_uuid())).execute(&mut *w.tx).await?;
        let details = AuditDetails { quota_bytes: Some(quota_bytes), ..AuditDetails::default() };
        record_in(&mut w.tx, now_ms, event_for(&actor, AuditAction::SpaceQuotaChanged, space)?.with(details)).await?;
        self.commit_audited(w, now_ms).await
    }

    /// Makes `user` an accepted owner of the team space, whatever their row was (none, an invitation, any role): the
    /// administrator naming an owner is the consent step, as for the first owner at creation. For a space whose owners are
    /// all gone. An account that cannot sign in is `NotFound("user")`. An accepted owner already is no change and writes no
    /// event; otherwise the event is `space.member_role_changed` with `role_to` owner (and `role_from` if there was a row).
    pub async fn set_owner_accepted(&self, admin: &AdminActor, space: SpaceId, user: UserId, now_ms: i64) -> Result<(), MetaError> {
        let actor = SpaceActor::Admin(admin.clone());
        let (mut w, _) = self.begin_space_change(&actor, space, Some(user), Need::Row).await?;
        if !user_can_join(&mut w, user).await? {
            return Err(MetaError::NotFound("user"));
        }
        let current = member_row(&mut w, space, user).await?;
        match &current {
            Some(member) if member.role == SpaceRole::Owner && member.accepted() => return Ok(()),
            Some(_) => {
                sqlx::query("UPDATE space_members SET role = 'owner', accepted_ms = COALESCE(accepted_ms, $1) WHERE space_id = $2 AND user_id = $3")
                    .bind(now_ms)
                    .bind(uuid_bytes(space.as_uuid()))
                    .bind(uuid_bytes(&user.0))
                    .execute(&mut *w.tx)
                    .await?;
            }
            None => insert_member(&mut w, space, user, SpaceRole::Owner, Some(admin.user), now_ms, true).await?,
        }
        let details = AuditDetails {
            user: Some(user),
            space_role_from: current.as_ref().map(|member| member.role),
            space_role_to: Some(SpaceRole::Owner),
            ..AuditDetails::default()
        };
        record_in(&mut w.tx, now_ms, event_for(&actor, AuditAction::SpaceMemberRoleChanged, space)?.with(details)).await?;
        self.commit_audited(w, now_ms).await
    }

    /// The spaces `user` can see, for `GET /spaces`: their personal space first, then the team spaces they are a member of
    /// or invited to, by name. A pending invitation has no root and no epoch yet. An archived space is listed for every
    /// accepted member (with `archived`; its access is capped at viewer, so it is theirs to read); an invitation to an
    /// archived space is not listed.
    pub async fn user_spaces(&self, user: UserId) -> Result<Vec<UserSpace>, MetaError> {
        let mut tx = self.read_snapshot().await?;
        let mut out = Vec::new();
        let personal = sqlx::query("SELECT id, created_ms, root, epoch FROM spaces WHERE owner = $1 AND kind = 'personal'")
            .bind(uuid_bytes(&user.0))
            .fetch_all(&mut *tx)
            .await?;
        for row in &personal {
            out.push(UserSpace {
                id: SpaceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
                personal: true,
                name: None,
                role: SpaceRole::Owner,
                invited: false,
                archived: false,
                created_ms: row.try_get("created_ms")?,
                members: 1,
                root: Some(NodeId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("root")?)?)),
                epoch: Some(Epoch::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("epoch")?)?)),
                joined_ms: None,
            });
        }
        let teams = sqlx::query(
            "SELECT s.id, s.name, s.created_ms, s.archived_ms, s.root, s.epoch, m.role, m.accepted_ms, \
             (SELECT CAST(COUNT(*) AS BIGINT) FROM space_members c WHERE c.space_id = s.id AND c.accepted_ms IS NOT NULL) AS members \
             FROM space_members m JOIN spaces s ON s.id = m.space_id WHERE m.user_id = $1 AND s.kind = 'team' ORDER BY s.name_fold, s.id",
        )
        .bind(uuid_bytes(&user.0))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        for row in &teams {
            let role = parse_role(&row.try_get::<String, _>("role")?)?;
            let joined_ms = row.try_get::<Option<i64>, _>("accepted_ms")?;
            let invited = joined_ms.is_none();
            let archived = row.try_get::<Option<i64>, _>("archived_ms")?.is_some();
            if archived && invited {
                continue;
            }
            out.push(UserSpace {
                id: SpaceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
                personal: false,
                name: row.try_get("name")?,
                role,
                invited,
                archived,
                created_ms: row.try_get("created_ms")?,
                members: to_u64(row.try_get("members")?)?,
                root: if invited { None } else { Some(NodeId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("root")?)?)) },
                epoch: if invited { None } else { Some(Epoch::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("epoch")?)?)) },
                joined_ms,
            });
        }
        Ok(out)
    }

    /// One team space as the administrator's list shows it; `NotFound("space")` for a space that is not a team space.
    pub async fn admin_space(&self, space: SpaceId) -> Result<SpaceSummary, MetaError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("{SUMMARY_SELECT} AND s.id = $1")))
            .bind(uuid_bytes(space.as_uuid()))
            .fetch_optional(&self.pool)
            .await?
            .ok_or(MetaError::NotFound("space"))?;
        summary_from(&row)
    }

    /// The team spaces by name, for the administrator (no content, only counts): `limit` of them after the name key `after`,
    /// whose name contains `search` (case does not matter). `SpacePage::next_after` continues the list.
    pub async fn admin_list_spaces(&self, after: Option<&str>, search: Option<&str>, limit: u32) -> Result<SpacePage, MetaError> {
        let pattern = match search.filter(|s| !s.is_empty()) {
            Some(text) => {
                if text.len() > crate::MAX_SEARCH_BYTES {
                    return Err(MetaError::Invalid("the search is too long"));
                }
                format!("%{}%", escape_like(craft_cloud_proto::fold_search(text).as_str()))
            }
            None => "%".to_owned(),
        };
        let sql = format!("{SUMMARY_SELECT} AND s.name_fold > $1 AND s.name_fold LIKE $2 ESCAPE '\\' ORDER BY s.name_fold LIMIT $3");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(after.unwrap_or_default())
            .bind(pattern)
            .bind(i64::from(limit).saturating_add(1))
            .fetch_all(&self.pool)
            .await?;
        let mut spaces = rows.iter().map(summary_from).collect::<Result<Vec<_>, _>>()?;
        let more = spaces.len() > limit as usize;
        spaces.truncate(limit as usize);
        let next_after = if more { spaces.last().map(|s| s.name_fold.clone()) } else { None };
        Ok(SpacePage { spaces, next_after })
    }

    /// The accepted owners of `spaces` (team spaces; at most a page's worth, 200), by space and username, with the state of
    /// their accounts, in one query. For the administrator's pages; who may ask is the caller's to decide.
    pub async fn space_owners(&self, spaces: &[SpaceId]) -> Result<Vec<SpaceOwner>, MetaError> {
        if spaces.is_empty() {
            return Ok(Vec::new());
        }
        if spaces.len() > 200 {
            return Err(MetaError::Invalid("at most 200 spaces at a time"));
        }
        let marks = (1..=spaces.len()).map(|n| format!("${n}")).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT m.space_id, m.user_id, u.username, u.disabled_ms, u.removed_ms FROM space_members m JOIN users u ON u.id = m.user_id              WHERE m.role = 'owner' AND m.accepted_ms IS NOT NULL AND m.space_id IN ({marks}) ORDER BY m.space_id, u.username"
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for space in spaces {
            query = query.bind(uuid_bytes(space.as_uuid()));
        }
        let rows = query.fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| {
                Ok(SpaceOwner {
                    space: SpaceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("space_id")?)?),
                    user: UserId(uuid_from(&row.try_get::<Vec<u8>, _>("user_id")?)?),
                    username: row.try_get("username")?,
                    disabled_ms: row.try_get("disabled_ms")?,
                    removed_ms: row.try_get("removed_ms")?,
                })
            })
            .collect()
    }

    /// The bytes all of a space's live documents hold, for the space's usage.
    pub async fn space_live_bytes(&self, space: SpaceId) -> Result<u64, MetaError> {
        let row = sqlx::query("SELECT live_bytes FROM spaces WHERE id = $1")
            .bind(uuid_bytes(space.as_uuid()))
            .fetch_optional(&self.pool)
            .await?
            .ok_or(MetaError::NotFound("space"))?;
        to_u64(row.try_get("live_bytes")?)
    }

    /// The role `user` has as an accepted member of the team space, whatever the space's state (an archived space does not
    /// cap it here): for the server to decide whether the caller acts as an owner. `None` for no accepted row, a personal
    /// space or a space that does not exist.
    pub async fn space_member_role(&self, user: UserId, space: SpaceId) -> Result<Option<SpaceRole>, MetaError> {
        let row = sqlx::query(
            "SELECT m.role FROM space_members m JOIN spaces s ON s.id = m.space_id \
             WHERE m.space_id = $1 AND m.user_id = $2 AND m.accepted_ms IS NOT NULL AND s.kind = 'team'",
        )
        .bind(uuid_bytes(space.as_uuid()))
        .bind(uuid_bytes(&user.0))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| parse_role(&row.try_get::<String, _>("role")?)).transpose()
    }

    /// The team space with this name (in any case), for the command line.
    pub async fn team_space_by_name(&self, name: &SpaceName) -> Result<Option<SpaceId>, MetaError> {
        let row = sqlx::query("SELECT id FROM spaces WHERE kind = 'team' AND name_fold = $1")
            .bind(name.fold_key().as_str())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| Ok(SpaceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?))).transpose()
    }

    /// A team space's name; `None` for a personal space or a space that does not exist.
    pub async fn team_space_name(&self, space: SpaceId) -> Result<Option<String>, MetaError> {
        let row = sqlx::query("SELECT name FROM spaces WHERE id = $1 AND kind = 'team'")
            .bind(uuid_bytes(space.as_uuid()))
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| row.try_get::<Option<String>, _>("name")).transpose()?.flatten())
    }

    /// The team spaces `user` is an accepted member of: whose long-polls to wake when the account is disabled or removed.
    pub async fn member_space_ids(&self, user: UserId) -> Result<Vec<SpaceId>, MetaError> {
        let rows = sqlx::query("SELECT space_id FROM space_members WHERE user_id = $1 AND accepted_ms IS NOT NULL")
            .bind(uuid_bytes(&user.0))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(|row| Ok(SpaceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("space_id")?)?))).collect()
    }

    /// Starts the write transaction of a change to a team space's members: checks the actor, takes the locks in the module's
    /// order and, for a change only an owner may make (`need`), reads the actor's role again under the space's lock.
    /// `target` is the account the change is about, if it has one. A space that is not a team space, or an account with
    /// no role in it, gets `NotFound("space")`: the two are not told apart; a member without the right role is `Forbidden`.
    async fn begin_space_change(
        &self,
        actor: &SpaceActor,
        space: SpaceId,
        target: Option<UserId>,
        need: Need,
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
        if let SpaceActor::User { user, .. } = actor {
            let member = member_row(&mut w, space, *user).await?;
            match need {
                // Accepting, declining and leaving are the caller's own rows: an account with no row at all learns nothing
                // about the space (the answer is the one for a space that does not exist), and only the wrong state of its
                // own row is `NotFound("invitation")` or `NotFound("member")`.
                Need::Row => {
                    if member.is_none() {
                        return Err(MetaError::NotFound("space"));
                    }
                }
                // Under the space's lock, so a role lowered by a change that committed meanwhile is respected.
                Need::Owner | Need::OwnerEvenIfArchived => {
                    let capped = locked.archived && need == Need::Owner;
                    let role = member.filter(Member::accepted).map(|member| if capped { member.role.min(SpaceRole::Viewer) } else { member.role });
                    match role {
                        Some(SpaceRole::Owner) => {}
                        Some(_) => return Err(MetaError::ReadOnly),
                        None => return Err(MetaError::NotFound("space")),
                    }
                }
            }
        }
        Ok((w, locked))
    }
}

/// [`Meta::space_access`] on a given connection (also a write transaction's).
async fn access_on(conn: &mut sqlx::AnyConnection, user: UserId, space: SpaceId) -> Result<Option<SpaceRole>, MetaError> {
    let row = sqlx::query(
        "SELECT s.kind, s.owner, s.archived_ms, m.role, m.accepted_ms, u.disabled_ms, u.removed_ms \
         FROM spaces s JOIN users u ON u.id = $1 LEFT JOIN space_members m ON m.space_id = s.id AND m.user_id = u.id \
         WHERE s.id = $2",
    )
    .bind(uuid_bytes(&user.0))
    .bind(uuid_bytes(space.as_uuid()))
    .fetch_optional(&mut *conn)
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
