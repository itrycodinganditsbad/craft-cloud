//! The security audit log: events in the `audit_events` table, written in the transaction of the change they record,
//! and read back by the account's own activity, the admin's list and export, and the command line.
//!
//! - **Names are a closed list** ([`AuditAction`]); no event has a name a client typed.
//! - **`details` holds typed values only** ([`AuditDetails`]): every field is a closed enumeration or a number, each
//!   event allows only its own fields ([`AuditAction::fields`]), and nothing a client sent as text ever reaches it. In
//!   particular `platform` is reduced to [`Platform`] (anything unknown is `other`), because a device may call its
//!   platform any printable text of 32 bytes, an address or a file name included. What is written is at most
//!   [`MAX_DETAILS_BYTES`] of JSON: when it would be more, whole fields go (the least important first) and
//!   `"truncated": true` says so; bytes are never cut off, which would leave invalid JSON.
//! - **No addresses, file or device names, user names of unknown accounts, secrets** are stored anywhere in a row.
//!   An account is named by its ID, which outlives the account (there are no foreign keys).
//! - **Changes write their event in their own transaction, before the commit, and fail with it** (fail closed): an event
//!   that cannot be written means the change is not made. A failed sign-in changes nothing, so its event is written
//!   after the fact in a transaction of its own ([`Meta::audit_record_failure`]), and a failure to write it is for the
//!   caller to log: it must never change the answer or undo the abuse protection around it.
//! - **Append-only, in the code.** Nothing here updates or deletes a row, with two named exceptions: the counter of an
//!   aggregated failure (one row per account, action, client kind and minute, `agg_count + 1` on a collision) and
//!   [`Meta::audit_prune`], which deletes events older than the retention in batches of their own, never inside the
//!   transaction of a security change. A test reads this crate's sources and allows exactly those two statements.

use craft_cloud_proto::api::SpaceRole;
use craft_cloud_proto::{DeviceId, SpaceId};
use serde_json::{Map, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::codec::{to_u64, uuid_bytes, uuid_from};
use crate::users::{DeviceClient, RevokeReason, Role, UserId};
use crate::{Meta, MetaError};

/// The most serialized UTF-8 bytes of an event's `details`.
pub const MAX_DETAILS_BYTES: usize = 2048;

/// Rows one pruning statement deletes.
pub const PRUNE_BATCH: i64 = 1000;
/// Batches one opportunistic pruning (after a write) runs before it leaves the rest to the next one.
pub const OPPORTUNISTIC_PRUNE_BATCHES: usize = 20;
/// How often at most a write starts a pruning.
pub const PRUNE_EVERY_MS: i64 = 60 * 60 * 1000;
/// The shortest and longest retention `[audit] retention_days` may name.
pub const MIN_RETENTION_DAYS: u32 = 1;
pub const MAX_RETENTION_DAYS: u32 = 3650;

const MINUTE_MS: i64 = 60_000;

/// What an event is. The names are the ones in `docs/design/sync.md`; a name is never reused for something else.
///
/// Not every name has a writer yet: the two-factor events (`auth.second_factor_failed`, `auth.recovery_code_used`,
/// `auth.two_factor_locked`, `account.two_factor_*`, `account.recovery_codes_regenerated`, `admin.two_factor_reset`,
/// `cli.two_factor_reset`) and `cli.user_removed` are written by the changes that add those operations. The names are
/// listed here so that the list a client may filter by does not change then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuditAction {
    AuthLogin,
    AuthLoginFailed,
    AuthSecondFactorFailed,
    AuthRecoveryCodeUsed,
    AuthTwoFactorLocked,
    DeviceRevoked,
    DeviceRevokedOthers,
    AccountPasswordChanged,
    AccountTwoFactorEnabled,
    AccountTwoFactorDisabled,
    AccountRecoveryCodesRegenerated,
    AdminUserCreated,
    AdminUserDisabled,
    AdminUserEnabled,
    AdminUserRemoved,
    AdminRoleChanged,
    AdminQuotaChanged,
    AdminInviteIssued,
    AdminInviteRevoked,
    AdminPasswordResetIssued,
    AdminTwoFactorReset,
    AdminDeviceRevoked,
    JoinInviteRedeemed,
    JoinResetRedeemed,
    JoinPairingRedeemed,
    CliPasswordReset,
    CliTwoFactorReset,
    CliDeviceRevoked,
    CliDevicesRevokedAll,
    CliUserRemoved,
    SpaceCreated,
    SpaceRenamed,
    SpaceArchived,
    SpaceUnarchived,
    SpaceQuotaChanged,
    SpaceMemberInvited,
    SpaceMemberJoined,
    SpaceMemberDeclined,
    SpaceMemberRoleChanged,
    SpaceMemberRemoved,
    SpaceMemberLeft,
    CliSpaceCreated,
    CliSpaceMemberChanged,
    CliSpaceArchived,
    CliLiveReset,
}

impl AuditAction {
    /// Every action, in the order the documentation lists them.
    pub const ALL: [AuditAction; 45] = [
        AuditAction::AuthLogin,
        AuditAction::AuthLoginFailed,
        AuditAction::AuthSecondFactorFailed,
        AuditAction::AuthRecoveryCodeUsed,
        AuditAction::AuthTwoFactorLocked,
        AuditAction::DeviceRevoked,
        AuditAction::DeviceRevokedOthers,
        AuditAction::AccountPasswordChanged,
        AuditAction::AccountTwoFactorEnabled,
        AuditAction::AccountTwoFactorDisabled,
        AuditAction::AccountRecoveryCodesRegenerated,
        AuditAction::AdminUserCreated,
        AuditAction::AdminUserDisabled,
        AuditAction::AdminUserEnabled,
        AuditAction::AdminUserRemoved,
        AuditAction::AdminRoleChanged,
        AuditAction::AdminQuotaChanged,
        AuditAction::AdminInviteIssued,
        AuditAction::AdminInviteRevoked,
        AuditAction::AdminPasswordResetIssued,
        AuditAction::AdminTwoFactorReset,
        AuditAction::AdminDeviceRevoked,
        AuditAction::JoinInviteRedeemed,
        AuditAction::JoinResetRedeemed,
        AuditAction::JoinPairingRedeemed,
        AuditAction::CliPasswordReset,
        AuditAction::CliTwoFactorReset,
        AuditAction::CliDeviceRevoked,
        AuditAction::CliDevicesRevokedAll,
        AuditAction::CliUserRemoved,
        AuditAction::SpaceCreated,
        AuditAction::SpaceRenamed,
        AuditAction::SpaceArchived,
        AuditAction::SpaceUnarchived,
        AuditAction::SpaceQuotaChanged,
        AuditAction::SpaceMemberInvited,
        AuditAction::SpaceMemberJoined,
        AuditAction::SpaceMemberDeclined,
        AuditAction::SpaceMemberRoleChanged,
        AuditAction::SpaceMemberRemoved,
        AuditAction::SpaceMemberLeft,
        AuditAction::CliSpaceCreated,
        AuditAction::CliSpaceMemberChanged,
        AuditAction::CliSpaceArchived,
        AuditAction::CliLiveReset,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AuditAction::AuthLogin => "auth.login",
            AuditAction::AuthLoginFailed => "auth.login_failed",
            AuditAction::AuthSecondFactorFailed => "auth.second_factor_failed",
            AuditAction::AuthRecoveryCodeUsed => "auth.recovery_code_used",
            AuditAction::AuthTwoFactorLocked => "auth.two_factor_locked",
            AuditAction::DeviceRevoked => "device.revoked",
            AuditAction::DeviceRevokedOthers => "device.revoked_others",
            AuditAction::AccountPasswordChanged => "account.password_changed",
            AuditAction::AccountTwoFactorEnabled => "account.two_factor_enabled",
            AuditAction::AccountTwoFactorDisabled => "account.two_factor_disabled",
            AuditAction::AccountRecoveryCodesRegenerated => "account.recovery_codes_regenerated",
            AuditAction::AdminUserCreated => "admin.user_created",
            AuditAction::AdminUserDisabled => "admin.user_disabled",
            AuditAction::AdminUserEnabled => "admin.user_enabled",
            AuditAction::AdminUserRemoved => "admin.user_removed",
            AuditAction::AdminRoleChanged => "admin.role_changed",
            AuditAction::AdminQuotaChanged => "admin.quota_changed",
            AuditAction::AdminInviteIssued => "admin.invite_issued",
            AuditAction::AdminInviteRevoked => "admin.invite_revoked",
            AuditAction::AdminPasswordResetIssued => "admin.password_reset_issued",
            AuditAction::AdminTwoFactorReset => "admin.two_factor_reset",
            AuditAction::AdminDeviceRevoked => "admin.device_revoked",
            AuditAction::JoinInviteRedeemed => "join.invite_redeemed",
            AuditAction::JoinResetRedeemed => "join.reset_redeemed",
            AuditAction::JoinPairingRedeemed => "join.pairing_redeemed",
            AuditAction::CliPasswordReset => "cli.password_reset",
            AuditAction::CliTwoFactorReset => "cli.two_factor_reset",
            AuditAction::CliDeviceRevoked => "cli.device_revoked",
            AuditAction::CliDevicesRevokedAll => "cli.devices_revoked_all",
            AuditAction::CliUserRemoved => "cli.user_removed",
            AuditAction::SpaceCreated => "space.created",
            AuditAction::SpaceRenamed => "space.renamed",
            AuditAction::SpaceArchived => "space.archived",
            AuditAction::SpaceUnarchived => "space.unarchived",
            AuditAction::SpaceQuotaChanged => "space.quota_changed",
            AuditAction::SpaceMemberInvited => "space.member_invited",
            AuditAction::SpaceMemberJoined => "space.member_joined",
            AuditAction::SpaceMemberDeclined => "space.member_declined",
            AuditAction::SpaceMemberRoleChanged => "space.member_role_changed",
            AuditAction::SpaceMemberRemoved => "space.member_removed",
            AuditAction::SpaceMemberLeft => "space.member_left",
            AuditAction::CliSpaceCreated => "cli.space_created",
            AuditAction::CliSpaceMemberChanged => "cli.space_member_changed",
            AuditAction::CliSpaceArchived => "cli.space_archived",
            AuditAction::CliLiveReset => "cli.live_reset",
        }
    }

    /// The action with this exact name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.as_str() == name)
    }

    /// The `details` fields this event may have; the writer drops any other. Listed in the order of importance, which is
    /// the order they are written in and the reverse of the order they are dropped in when the text is too long.
    pub fn fields(self) -> &'static [Field] {
        use Field::*;
        match self {
            AuditAction::AuthLogin | AuditAction::JoinInviteRedeemed | AuditAction::JoinPairingRedeemed => &[ClientKind, Platform],
            AuditAction::JoinResetRedeemed => &[Revoked, TwoFactorRemoved, ClientKind, Platform],
            AuditAction::AuthLoginFailed | AuditAction::AuthSecondFactorFailed => &[ClientKind, Step],
            AuditAction::AuthRecoveryCodeUsed => &[ClientKind],
            AuditAction::DeviceRevoked => &[Reason],
            AuditAction::DeviceRevokedOthers => &[Revoked, Reason],
            AuditAction::AdminUserCreated => &[RoleTo],
            AuditAction::AdminRoleChanged => &[RoleFrom, RoleTo],
            AuditAction::AdminQuotaChanged => &[QuotaBytes],
            AuditAction::AdminDeviceRevoked => &[Device, Reason],
            AuditAction::CliDeviceRevoked => &[Device, Reason],
            AuditAction::AccountPasswordChanged
            | AuditAction::AdminUserDisabled
            | AuditAction::AdminUserRemoved
            | AuditAction::CliPasswordReset
            | AuditAction::CliTwoFactorReset
            | AuditAction::CliDevicesRevokedAll
            | AuditAction::CliUserRemoved => &[Revoked],
            AuditAction::AccountTwoFactorEnabled
            | AuditAction::AccountTwoFactorDisabled
            | AuditAction::AccountRecoveryCodesRegenerated
            | AuditAction::AdminUserEnabled
            | AuditAction::AdminInviteIssued
            | AuditAction::AdminInviteRevoked
            | AuditAction::AdminPasswordResetIssued
            | AuditAction::AdminTwoFactorReset => &[],
            AuditAction::AuthTwoFactorLocked => &[ClientKind],
            AuditAction::SpaceCreated | AuditAction::CliSpaceCreated => &[User, QuotaBytes],
            AuditAction::SpaceRenamed
            | AuditAction::SpaceArchived
            | AuditAction::SpaceUnarchived
            | AuditAction::SpaceMemberLeft
            | AuditAction::CliSpaceArchived
            | AuditAction::CliLiveReset => &[],
            AuditAction::SpaceQuotaChanged => &[QuotaBytes],
            AuditAction::SpaceMemberInvited => &[User, SpaceRoleTo],
            AuditAction::SpaceMemberJoined | AuditAction::SpaceMemberDeclined | AuditAction::SpaceMemberRemoved => &[User],
            AuditAction::SpaceMemberRoleChanged | AuditAction::CliSpaceMemberChanged => &[User, SpaceRoleFrom, SpaceRoleTo],
        }
    }
}

/// What a `details` field is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    ClientKind,
    Platform,
    Reason,
    Step,
    RoleFrom,
    RoleTo,
    QuotaBytes,
    Revoked,
    Device,
    /// A reset code redeemed for an account that had two-factor sign-in turned it off.
    TwoFactorRemoved,
    /// The account a team space event is about.
    User,
    /// A role in a team space, written as `role_from` / `role_to` like an account's server role.
    SpaceRoleFrom,
    SpaceRoleTo,
}

/// The platforms an event names. A device may call its platform any printable text; only these reach the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Macos,
    Linux,
    Android,
    Ios,
    Other,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Windows => "windows",
            Platform::Macos => "macos",
            Platform::Linux => "linux",
            Platform::Android => "android",
            Platform::Ios => "ios",
            Platform::Other => "other",
        }
    }

    /// The platform a client named: `None` for none at all, one of the list when it is that (case does not matter),
    /// `Other` for anything else, whatever it looks like.
    pub fn normalize(platform: &str) -> Option<Self> {
        if platform.is_empty() {
            return None;
        }
        Some(
            [Platform::Windows, Platform::Macos, Platform::Linux, Platform::Android, Platform::Ios]
                .into_iter()
                .find(|known| known.as_str().eq_ignore_ascii_case(platform))
                .unwrap_or(Platform::Other),
        )
    }
}

/// The client kinds an event names. Device kinds are [`crate::CLIENT_KINDS`]; `Web` is assigned only by web sign-in code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    Agent,
    Desktop,
    Cli,
    Web,
}

impl ClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientKind::Agent => "agent",
            ClientKind::Desktop => "desktop",
            ClientKind::Cli => "cli",
            ClientKind::Web => "web",
        }
    }

    /// Parses device-sent kinds only. Web is assigned by the server, never accepted from a device.
    fn parse(kind: &str) -> Option<Self> {
        [ClientKind::Agent, ClientKind::Desktop, ClientKind::Cli].into_iter().find(|known| known.as_str() == kind)
    }
}

/// How far a failed sign-in got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Password,
    Totp,
    Recovery,
    ChallengeExpired,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Password => "password",
            Step::Totp => "totp",
            Step::Recovery => "recovery",
            Step::ChallengeExpired => "challenge_expired",
        }
    }
}

/// The typed values of an event. Which of them an event keeps is decided by [`AuditAction::fields`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditDetails {
    pub client_kind: Option<ClientKind>,
    pub platform: Option<Platform>,
    pub reason: Option<RevokeReason>,
    pub step: Option<Step>,
    pub role_from: Option<Role>,
    pub role_to: Option<Role>,
    /// `Some(None)`: the quota was removed.
    pub quota_bytes: Option<Option<u64>>,
    pub revoked: Option<u32>,
    pub device: Option<DeviceId>,
    /// Redeeming a reset code also took the account's two-factor sign-in away (only ever set to `true`).
    pub two_factor_removed: bool,
    /// The account a team space event is about (the member, or the first owner).
    pub user: Option<UserId>,
    pub space_role_from: Option<SpaceRole>,
    pub space_role_to: Option<SpaceRole>,
}

impl AuditDetails {
    /// What a device says about its client, reduced to the closed lists.
    pub fn client(client: &DeviceClient) -> Self {
        Self { client_kind: ClientKind::parse(&client.kind), platform: Platform::normalize(&client.platform), ..Self::default() }
    }

    pub fn revoked(count: usize) -> Self {
        Self { revoked: Some(u32::try_from(count).unwrap_or(u32::MAX)), ..Self::default() }
    }

    pub fn reason(reason: RevokeReason) -> Self {
        Self { reason: Some(reason), ..Self::default() }
    }

    pub fn step(step: Step) -> Self {
        Self { step: Some(step), ..Self::default() }
    }

    pub fn web() -> Self {
        Self { client_kind: Some(ClientKind::Web), ..Self::default() }
    }

    /// The value of `field`, if it is set.
    fn value(&self, field: Field) -> Option<Value> {
        match field {
            Field::ClientKind => self.client_kind.map(|v| Value::from(v.as_str())),
            Field::Platform => self.platform.map(|v| Value::from(v.as_str())),
            Field::Reason => self.reason.map(|v| Value::from(v.as_str())),
            Field::Step => self.step.map(|v| Value::from(v.as_str())),
            Field::RoleFrom => self.role_from.map(|v| Value::from(v.as_str())),
            Field::RoleTo => self.role_to.map(|v| Value::from(v.as_str())),
            Field::QuotaBytes => self.quota_bytes.map(|v| v.map_or(Value::Null, Value::from)),
            Field::Revoked => self.revoked.map(Value::from),
            Field::Device => self.device.map(|v| Value::from(v.to_string())),
            Field::TwoFactorRemoved => self.two_factor_removed.then_some(Value::Bool(true)),
            Field::User => self.user.map(|v| Value::from(v.0.to_string())),
            Field::SpaceRoleFrom => self.space_role_from.map(|v| Value::from(v.as_str())),
            Field::SpaceRoleTo => self.space_role_to.map(|v| Value::from(v.as_str())),
        }
    }

    fn name(field: Field) -> &'static str {
        match field {
            Field::ClientKind => "client_kind",
            Field::Platform => "platform",
            Field::Reason => "reason",
            Field::Step => "step",
            Field::RoleFrom => "role_from",
            Field::RoleTo => "role_to",
            Field::QuotaBytes => "quota_bytes",
            Field::Revoked => "revoked",
            Field::Device => "device",
            Field::TwoFactorRemoved => "two_factor_removed",
            Field::User => "user",
            Field::SpaceRoleFrom => "role_from",
            Field::SpaceRoleTo => "role_to",
        }
    }

    /// The JSON text stored for `action`: the fields it allows that are set, within [`MAX_DETAILS_BYTES`].
    pub fn to_json(&self, action: AuditAction) -> String {
        let fields = action.fields().iter().filter_map(|field| self.value(*field).map(|value| (Self::name(*field), value))).collect();
        serialize_fields(fields, MAX_DETAILS_BYTES)
    }
}

/// `fields` (most important first) as a JSON object of at most `max` bytes. When it would be longer, fields are dropped
/// from the end, one at a time, and `"truncated": true` is added; no text is ever cut short.
pub(crate) fn serialize_fields(mut fields: Vec<(&'static str, Value)>, max: usize) -> String {
    let mut truncated = false;
    loop {
        let mut object = Map::new();
        for (name, value) in &fields {
            object.insert((*name).to_owned(), value.clone());
        }
        if truncated {
            object.insert("truncated".to_owned(), Value::Bool(true));
        }
        let text = Value::Object(object).to_string();
        if text.len() <= max {
            return text;
        }
        if fields.pop().is_none() {
            // Not reachable for any `max` this crate uses (the flag alone is 18 bytes), but never invalid JSON.
            return "{\"truncated\":true}".to_owned();
        }
        truncated = true;
    }
}

/// Whether the event is about something that worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    Success,
    Failure,
}

impl AuditOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditOutcome::Success => "success",
            AuditOutcome::Failure => "failure",
        }
    }

    fn parse(s: &str) -> Result<Self, MetaError> {
        match s {
            "success" => Ok(AuditOutcome::Success),
            "failure" => Ok(AuditOutcome::Failure),
            _ => Err(MetaError::Corrupt("unknown audit outcome")),
        }
    }
}

/// What an event was done to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    User,
    Device,
    Space,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::User => "user",
            TargetKind::Device => "device",
            TargetKind::Space => "space",
        }
    }
}

/// Who made a change that a signed-in account or the command line can make, for its event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The account's own act, from a device the caller does not say.
    Account,
    /// The account's own act, from this device.
    Device(DeviceId),
    /// The admin's command line on the server's host: no account acts.
    Cli,
}

/// An event on its way into the log.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewEvent {
    pub action: AuditAction,
    pub actor_user: Option<UserId>,
    pub actor_device: Option<DeviceId>,
    pub target: Option<(TargetKind, Uuid)>,
    pub details: AuditDetails,
}

impl NewEvent {
    pub fn new(action: AuditAction) -> Self {
        Self { action, actor_user: None, actor_device: None, target: None, details: AuditDetails::default() }
    }

    pub fn by(mut self, user: UserId, device: Option<DeviceId>) -> Self {
        self.actor_user = Some(user);
        self.actor_device = device;
        self
    }

    pub fn on_user(mut self, user: UserId) -> Self {
        self.target = Some((TargetKind::User, user.0));
        self
    }

    pub fn on_space(mut self, space: SpaceId) -> Self {
        self.target = Some((TargetKind::Space, *space.as_uuid()));
        self
    }

    pub fn on_device(mut self, device: DeviceId) -> Self {
        self.target = Some((TargetKind::Device, *device.as_uuid()));
        self
    }

    pub fn with(mut self, details: AuditDetails) -> Self {
        self.details = details;
        self
    }
}

const INSERT: &str = concat!(
    "INSERT INTO audit_events (id, at_ms, actor_user, actor_device, action, target_kind, target_id, outcome, details, agg_key, agg_count) ",
    "VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 1)"
);

/// Inserts `event` on `conn` (a transaction's connection). `aggregate`: the key of an aggregated failure, whose row
/// only counts up when the key is there already; `None` for every other event, whose key is `NULL`.
async fn insert(
    conn: &mut sqlx::AnyConnection,
    event: &NewEvent,
    outcome: AuditOutcome,
    now_ms: i64,
    aggregate: Option<String>,
) -> Result<(), MetaError> {
    // Qualified: on Postgres a bare `agg_count` is ambiguous between the table's column and `excluded`'s. Only the
    // counter changes, so the row keeps the identity, time and details of its first try.
    let sql = if aggregate.is_some() {
        format!("{INSERT} ON CONFLICT (agg_key) DO UPDATE SET agg_count = audit_events.agg_count + 1")
    } else {
        INSERT.to_owned()
    };
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(uuid_bytes(&Uuid::now_v7()))
        .bind(now_ms)
        .bind(event.actor_user.map(|u| uuid_bytes(&u.0)))
        .bind(event.actor_device.map(|d| uuid_bytes(d.as_uuid())))
        .bind(event.action.as_str())
        .bind(event.target.map_or("", |(kind, _)| kind.as_str()))
        .bind(event.target.map(|(_, id)| uuid_bytes(&id)))
        .bind(outcome.as_str())
        .bind(event.details.to_json(event.action))
        .bind(aggregate)
        .execute(conn)
        .await?;
    Ok(())
}

/// Writes `event` as a success in `tx`, the transaction of the change it records. Its error fails the change.
pub(crate) async fn record_in(tx: &mut sqlx::Transaction<'static, sqlx::Any>, now_ms: i64, event: NewEvent) -> Result<(), MetaError> {
    insert(tx, &event, AuditOutcome::Success, now_ms, None).await
}

/// A failed sign-in step, counted by action, client kind and minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditFailure {
    /// `AuthLoginFailed` or `AuthSecondFactorFailed`; any other action is `Invalid`.
    pub action: AuditAction,
    /// The account the guess was about; `None` for a name no account has, which is all the log may say of it.
    pub target: Option<UserId>,
    pub step: Step,
}

/// Where a page of the log starts: strictly after (older than) this event, in the order `(at_ms, id)` descending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditCursor {
    pub at_ms: i64,
    pub id: Uuid,
}

impl AuditCursor {
    pub const BYTES: usize = 24;

    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut out = [0u8; Self::BYTES];
        out[..8].copy_from_slice(&self.at_ms.to_be_bytes());
        out[8..].copy_from_slice(self.id.as_bytes());
        out
    }

    /// `None` unless `bytes` is exactly what [`AuditCursor::to_bytes`] makes (and a time that can be one).
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; Self::BYTES] = bytes.try_into().ok()?;
        let at_ms = i64::from_be_bytes(bytes.get(..8)?.try_into().ok()?);
        let id = Uuid::from_slice(bytes.get(8..)?).ok()?;
        (at_ms >= 0).then_some(Self { at_ms, id })
    }
}

/// Which actions a list is limited to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionFilter {
    /// This action only.
    Exact(AuditAction),
    /// The actions whose name starts with this (it ends in a dot, like `auth.`).
    Prefix(&'static str),
}

impl ActionFilter {
    /// An exact name, or a prefix ending in a dot that at least one name starts with. Anything else is `None`, so
    /// nothing a client typed ever reaches a query.
    pub fn parse(text: &str) -> Option<Self> {
        if let Some(action) = AuditAction::parse(text) {
            return Some(ActionFilter::Exact(action));
        }
        if !text.ends_with('.') || !text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'.') {
            return None;
        }
        // The prefix is kept as the `'static` start of the first name it matches: the same characters, from the closed list.
        AuditAction::ALL
            .iter()
            .find(|action| action.as_str().starts_with(text))
            .and_then(|action| action.as_str().get(..text.len()))
            .map(ActionFilter::Prefix)
    }

    /// The names the filter lets through.
    pub fn actions(self) -> Vec<AuditAction> {
        match self {
            ActionFilter::Exact(action) => vec![action],
            ActionFilter::Prefix(prefix) => AuditAction::ALL.into_iter().filter(|action| action.as_str().starts_with(prefix)).collect(),
        }
    }
}

/// One page of the log, asked for.
#[derive(Debug, Clone, Copy, Default)]
pub struct AuditQuery {
    /// Events the account acted in or was the target of.
    pub user: Option<UserId>,
    pub action: Option<ActionFilter>,
    /// Only events before this time (the first call of a listing).
    pub before_ms: Option<i64>,
    /// Only events up to and including this time (an export's fixed end).
    pub until_ms: Option<i64>,
    /// Only events strictly after this one in the listing's order.
    pub after: Option<AuditCursor>,
    pub limit: u32,
}

/// An event as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub id: Uuid,
    pub at_ms: i64,
    pub actor_user: Option<UserId>,
    pub actor_device: Option<DeviceId>,
    pub action: AuditAction,
    /// `user`, `device`, `space`, or empty.
    pub target_kind: String,
    pub target_id: Option<Uuid>,
    pub outcome: AuditOutcome,
    /// A JSON object text, as written.
    pub details: String,
    /// The tries one aggregated row counts (1 for every other event).
    pub count: u64,
}

impl AuditEvent {
    /// Where a page continues after this event.
    pub fn cursor(&self) -> AuditCursor {
        AuditCursor { at_ms: self.at_ms, id: self.id }
    }
}

/// What a pruning did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    pub deleted: u64,
    /// There was more to delete than the batches allowed.
    pub more: bool,
}

const SELECT: &str = "SELECT id, at_ms, actor_user, actor_device, action, target_kind, target_id, outcome, details, agg_count FROM audit_events";

fn event_from(row: &sqlx::any::AnyRow) -> Result<AuditEvent, MetaError> {
    let actor_user: Option<Vec<u8>> = row.try_get("actor_user")?;
    let actor_device: Option<Vec<u8>> = row.try_get("actor_device")?;
    let target_id: Option<Vec<u8>> = row.try_get("target_id")?;
    Ok(AuditEvent {
        id: uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?,
        at_ms: row.try_get("at_ms")?,
        actor_user: actor_user.map(|b| uuid_from(&b).map(UserId)).transpose()?,
        actor_device: actor_device.map(|b| uuid_from(&b).map(DeviceId::from_uuid)).transpose()?,
        action: AuditAction::parse(&row.try_get::<String, _>("action")?).ok_or(MetaError::Corrupt("unknown audit action"))?,
        target_kind: row.try_get("target_kind")?,
        target_id: target_id.map(|b| uuid_from(&b)).transpose()?,
        outcome: AuditOutcome::parse(&row.try_get::<String, _>("outcome")?)?,
        details: row.try_get("details")?,
        count: to_u64(row.try_get("agg_count")?)?,
    })
}

enum Arg {
    Int(i64),
    Bytes(Vec<u8>),
    Text(String),
}

/// How long a pruning that is running holds the right to run, and what the last complete one's time was.
#[derive(Debug)]
pub(crate) struct PruneState {
    last_done_ms: std::sync::atomic::AtomicI64,
    running: std::sync::atomic::AtomicBool,
}

impl PruneState {
    const NEVER: i64 = i64::MIN;

    pub fn new() -> Self {
        Self { last_done_ms: std::sync::atomic::AtomicI64::new(Self::NEVER), running: std::sync::atomic::AtomicBool::new(false) }
    }
}

/// Gives the right to run back when dropped, whether the pruning finished, failed or was cancelled.
struct Running<'a>(&'a PruneState);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl Meta {
    /// The store that also prunes events older than `days` days after a write, at most once an hour and in bounded
    /// batches ([`Meta::audit_prune_if_due`]). Without it nothing is pruned but by [`Meta::audit_prune`].
    pub fn with_audit_retention_days(mut self, days: u32) -> Self {
        self.audit_retention_ms = Some(i64::from(days).saturating_mul(24 * 60 * 60 * 1000));
        self
    }

    /// Writes a failed sign-in step in a transaction of its own, after the fact: one row per account (or per minute, for
    /// names no account has), action, client kind and minute, whose `count` goes up by one with every further try. The caller logs a
    /// failure and goes on: the answer to the client and the limits around the sign-in do not depend on this.
    pub async fn audit_record_failure(&self, failure: AuditFailure, now_ms: i64) -> Result<(), MetaError> {
        self.audit_record_failure_kind(failure, None, now_ms).await
    }

    pub async fn audit_record_web_failure(&self, failure: AuditFailure, now_ms: i64) -> Result<(), MetaError> {
        self.audit_record_failure_kind(failure, Some(ClientKind::Web), now_ms).await
    }

    async fn audit_record_failure_kind(&self, failure: AuditFailure, kind: Option<ClientKind>, now_ms: i64) -> Result<(), MetaError> {
        if !matches!(failure.action, AuditAction::AuthLoginFailed | AuditAction::AuthSecondFactorFailed) {
            return Err(MetaError::Invalid("only failed sign-in steps are aggregated"));
        }
        let mut details = AuditDetails::step(failure.step);
        details.client_kind = kind;
        let mut event = NewEvent::new(failure.action).with(details);
        if let Some(user) = failure.target {
            event = event.on_user(user);
        }
        let who = failure.target.map_or_else(|| "-".to_owned(), |u| u.0.simple().to_string());
        let kind = kind.map_or("device", ClientKind::as_str);
        let key = format!("{}:{kind}:{who}:{}", failure.action.as_str(), now_ms.div_euclid(MINUTE_MS));
        let mut w = self.begin_write().await?;
        insert(&mut w.tx, &event, AuditOutcome::Failure, now_ms, Some(key)).await?;
        w.commit().await?;
        self.prune_soon(now_ms);
        Ok(())
    }

    /// One page of events, newest first, in the order `(at_ms, id)` descending: at most `query.limit` (and never more
    /// than 1000).
    pub async fn audit_list(&self, query: &AuditQuery) -> Result<Vec<AuditEvent>, MetaError> {
        let mut sql = format!("{SELECT} WHERE 1 = 1");
        let mut args = Vec::new();
        let mut next = |arg: Arg| -> usize {
            args.push(arg);
            args.len()
        };
        if let Some(user) = query.user {
            let n = next(Arg::Bytes(uuid_bytes(&user.0)));
            // An event also concerns the account it names as `user` in its details (a space member invited, removed ...).
            // Details are compact JSON, and an ID has no character that `LIKE` reads.
            let m = next(Arg::Text(format!("%\"user\":\"{}\"%", user.0.hyphenated())));
            sql.push_str(&format!(" AND (actor_user = ${n} OR target_id = ${n} OR details LIKE ${m})"));
        }
        if let Some(filter) = query.action {
            // The names come from the closed list, never from a request, so they are safe to write into the text.
            let names: Vec<String> = filter.actions().into_iter().map(|a| format!("'{}'", a.as_str())).collect();
            sql.push_str(&format!(" AND action IN ({})", names.join(", ")));
        }
        if let Some(before) = query.before_ms {
            let n = next(Arg::Int(before));
            sql.push_str(&format!(" AND at_ms < ${n}"));
        }
        if let Some(until) = query.until_ms {
            let n = next(Arg::Int(until));
            sql.push_str(&format!(" AND at_ms <= ${n}"));
        }
        if let Some(after) = query.after {
            let at = next(Arg::Int(after.at_ms));
            let id = next(Arg::Bytes(uuid_bytes(&after.id)));
            sql.push_str(&format!(" AND (at_ms < ${at} OR (at_ms = ${at} AND id < ${id}))"));
        }
        let limit = next(Arg::Int(i64::from(query.limit.min(1000))));
        sql.push_str(&format!(" ORDER BY at_ms DESC, id DESC LIMIT ${limit}"));
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for arg in args {
            q = match arg {
                Arg::Int(i) => q.bind(i),
                Arg::Bytes(b) => q.bind(b),
                Arg::Text(t) => q.bind(t),
            };
        }
        q.fetch_all(&self.pool).await?.iter().map(event_from).collect()
    }

    /// How many events are older than `before_ms` (what [`Meta::audit_prune`] would delete).
    pub async fn audit_count_before(&self, before_ms: i64) -> Result<u64, MetaError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM audit_events WHERE at_ms < $1").bind(before_ms).fetch_one(&self.pool).await?;
        to_u64(row.try_get("n")?)
    }

    /// Deletes every event older than `before_ms`, in batches of [`PRUNE_BATCH`] rows, each in a transaction of its own.
    /// Returns how many went. It is the one way a row is deleted (a test checks the sources).
    pub async fn audit_prune(&self, before_ms: i64) -> Result<u64, MetaError> {
        let mut total = 0u64;
        loop {
            let done = self.audit_prune_batches(before_ms, 64).await?;
            total = total.saturating_add(done.deleted);
            if !done.more {
                return Ok(total);
            }
        }
    }

    /// [`Meta::audit_prune`] for at most `max_batches` batches: `more` says whether events older than `before_ms` are
    /// left. A batch holds the write lock only for its own statement, so a sign-in never waits for a long pruning.
    pub async fn audit_prune_batches(&self, before_ms: i64, max_batches: usize) -> Result<Pruned, MetaError> {
        let mut deleted = 0u64;
        for _ in 0..max_batches {
            let mut w = self.begin_write().await?;
            let done = sqlx::query("DELETE FROM audit_events WHERE id IN (SELECT id FROM audit_events WHERE at_ms < $1 ORDER BY at_ms LIMIT $2)")
                .bind(before_ms)
                .bind(PRUNE_BATCH)
                .execute(&mut *w.tx)
                .await?;
            w.commit().await?;
            deleted = deleted.saturating_add(done.rows_affected());
            if done.rows_affected() < PRUNE_BATCH as u64 {
                return Ok(Pruned { deleted, more: false });
            }
        }
        Ok(Pruned { deleted, more: true })
    }

    /// Prunes events past the retention ([`Meta::with_audit_retention_days`]) if the last complete pruning was an hour or
    /// more before `now_ms` (or there was none), in at most [`OPPORTUNISTIC_PRUNE_BATCHES`] batches. The time of the last
    /// one is set only when it deleted everything that was due, so a backlog or an error lets the next write go on.
    /// Returns whether a pruning ran. Called after the commit of a write, never inside a transaction.
    pub async fn audit_prune_if_due(&self, now_ms: i64) -> Result<bool, MetaError> {
        self.audit_prune_if_due_in(now_ms, OPPORTUNISTIC_PRUNE_BATCHES).await
    }

    /// [`Meta::audit_prune_if_due`] with its own limit of batches, so that a test can make a backlog without thousands of rows.
    pub async fn audit_prune_if_due_in(&self, now_ms: i64, max_batches: usize) -> Result<bool, MetaError> {
        use std::sync::atomic::Ordering;
        let Some(keep_ms) = self.audit_retention_ms else { return Ok(false) };
        let state = &self.prune_state;
        let last = state.last_done_ms.load(Ordering::Acquire);
        if last != PruneState::NEVER && now_ms.saturating_sub(last) < PRUNE_EVERY_MS {
            return Ok(false);
        }
        if state.running.swap(true, Ordering::AcqRel) {
            return Ok(false);
        }
        let _running = Running(state);
        let done = self.audit_prune_batches(now_ms.saturating_sub(keep_ms), max_batches).await?;
        if !done.more {
            state.last_done_ms.store(now_ms, Ordering::Release);
        }
        Ok(true)
    }

    /// Commits a change that wrote an audit event, then lets the write start a pruning ([`Meta::prune_soon`]).
    pub(crate) async fn commit_audited(&self, w: crate::WriteTx, now_ms: i64) -> Result<(), MetaError> {
        w.commit().await?;
        self.prune_soon(now_ms);
        Ok(())
    }

    /// Starts [`Meta::audit_prune_if_due`] in the background, so the write that asked does not wait for it.
    pub(crate) fn prune_soon(&self, now_ms: i64) {
        if self.audit_retention_ms.is_none() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let meta = self.clone();
        runtime.spawn(async move {
            if let Err(e) = meta.audit_prune_if_due(now_ms).await {
                tracing::warn!(error = %e, "couldn't prune old audit events");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_a_unique_name_that_parses_back() {
        let mut seen = std::collections::HashSet::new();
        for action in AuditAction::ALL {
            assert!(seen.insert(action.as_str()), "{} twice", action.as_str());
            assert_eq!(AuditAction::parse(action.as_str()), Some(action));
            assert!(action.as_str().bytes().all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'.'));
        }
        assert_eq!(seen.len(), 45);
        assert_eq!(AuditAction::parse("auth.login "), None);
        assert_eq!(AuditAction::parse("AUTH.LOGIN"), None);
    }

    #[test]
    fn a_filter_is_a_name_or_a_prefix_of_names() {
        assert_eq!(ActionFilter::parse("auth.login"), Some(ActionFilter::Exact(AuditAction::AuthLogin)));
        let auth = ActionFilter::parse("auth.").unwrap();
        assert_eq!(auth.actions().len(), 5);
        assert!(auth.actions().iter().all(|a| a.as_str().starts_with("auth.")));
        assert_eq!(ActionFilter::parse("admin.").unwrap().actions().len(), 11);
        for bad in ["", "auth", "au.", "auth.%", "auth._", "nope.", "AUTH.", "auth.login'", "auth. ", "auth.logi", "'; DROP TABLE x; --"] {
            assert_eq!(ActionFilter::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_platform_is_reduced_to_the_closed_list() {
        assert_eq!(Platform::normalize(""), None);
        assert_eq!(Platform::normalize("Windows"), Some(Platform::Windows));
        assert_eq!(Platform::normalize("macos"), Some(Platform::Macos));
        // Hostile text a device may legally send: 32 bytes of printable characters or less.
        for hostile in [
            "10.0.0.1",
            "secret.psd",
            "fe80::1",
            "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}",
            "windows 11",
            "linux\u{7f}",
        ] {
            assert_eq!(Platform::normalize(hostile), Some(Platform::Other), "{hostile:?}");
        }
    }

    #[test]
    fn details_keep_only_the_fields_their_event_allows() {
        let all = AuditDetails {
            client_kind: Some(ClientKind::Agent),
            platform: Some(Platform::Linux),
            reason: Some(RevokeReason::Admin),
            step: Some(Step::Password),
            role_from: Some(Role::Member),
            role_to: Some(Role::Admin),
            quota_bytes: Some(None),
            revoked: Some(3),
            device: Some(DeviceId::from_uuid(Uuid::nil())),
            two_factor_removed: true,
            user: Some(UserId(Uuid::nil())),
            space_role_from: Some(SpaceRole::Viewer),
            space_role_to: Some(SpaceRole::Owner),
        };
        let keys = |action: AuditAction| -> Vec<String> {
            let value: Value = serde_json::from_str(&all.to_json(action)).unwrap();
            let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(keys(AuditAction::AuthLogin), ["client_kind", "platform"]);
        assert_eq!(keys(AuditAction::AuthLoginFailed), ["client_kind", "step"]);
        assert_eq!(keys(AuditAction::AuthRecoveryCodeUsed), ["client_kind"]);
        assert_eq!(keys(AuditAction::AdminRoleChanged), ["role_from", "role_to"]);
        assert_eq!(keys(AuditAction::AdminQuotaChanged), ["quota_bytes"]);
        assert_eq!(keys(AuditAction::AdminUserEnabled), Vec::<String>::new());
        assert_eq!(keys(AuditAction::CliDeviceRevoked), ["device", "reason"]);
        assert_eq!(keys(AuditAction::JoinResetRedeemed), ["client_kind", "platform", "revoked", "two_factor_removed"]);
        assert_eq!(keys(AuditAction::AccountTwoFactorEnabled), Vec::<String>::new());
        assert_eq!(keys(AuditAction::SpaceCreated), ["quota_bytes", "user"]);
        assert_eq!(keys(AuditAction::SpaceMemberInvited), ["role_to", "user"]);
        assert_eq!(keys(AuditAction::SpaceMemberRoleChanged), ["role_from", "role_to", "user"]);
        assert_eq!(keys(AuditAction::CliSpaceMemberChanged), ["role_from", "role_to", "user"]);
        assert_eq!(keys(AuditAction::SpaceMemberRemoved), ["user"]);
        assert_eq!(keys(AuditAction::SpaceRenamed), Vec::<String>::new());
        assert_eq!(all.to_json(AuditAction::AdminQuotaChanged), r#"{"quota_bytes":null}"#);
        assert_eq!(AuditDetails::revoked(2).to_json(AuditAction::AdminUserDisabled), r#"{"revoked":2}"#);
        assert_eq!(AuditDetails::default().to_json(AuditAction::AdminInviteIssued), "{}");
    }

    #[test]
    fn details_over_the_limit_lose_whole_fields_and_stay_valid_json() {
        // Multi-byte characters, so that cutting bytes anywhere would break the text.
        let long = "\u{20ac}".repeat(300);
        let fields = vec![
            ("first", Value::from("kept")),
            ("second", Value::from(long.clone())),
            ("third", Value::from(long.clone())),
            ("last", Value::from(long)),
        ];
        let text = serialize_fields(fields.clone(), MAX_DETAILS_BYTES);
        assert!(text.len() <= MAX_DETAILS_BYTES, "{}", text.len());
        let value: Value = serde_json::from_str(&text).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.get("truncated"), Some(&Value::Bool(true)));
        assert_eq!(object.get("first"), Some(&Value::from("kept")));
        // The least important go first, and a field that stays is whole.
        assert!(object.get("last").is_none());
        for (name, value) in &fields {
            if let Some(kept) = object.get(*name) {
                assert_eq!(kept, value);
            }
        }
        // Under the limit nothing is dropped and nothing is flagged.
        let small = serialize_fields(vec![("a", Value::from(1))], MAX_DETAILS_BYTES);
        assert_eq!(small, r#"{"a":1}"#);
        // Even one field over the limit leaves a valid object.
        let one = serialize_fields(vec![("only", Value::from("\u{20ac}".repeat(1000)))], MAX_DETAILS_BYTES);
        assert_eq!(one, r#"{"truncated":true}"#);
    }

    #[test]
    fn a_cursor_round_trips_and_refuses_other_bytes() {
        let cursor = AuditCursor { at_ms: 1_700_000_000_123, id: Uuid::now_v7() };
        assert_eq!(AuditCursor::from_bytes(&cursor.to_bytes()), Some(cursor));
        assert_eq!(AuditCursor::from_bytes(&[]), None);
        assert_eq!(AuditCursor::from_bytes(&cursor.to_bytes()[..23]), None);
        let mut longer = cursor.to_bytes().to_vec();
        longer.push(0);
        assert_eq!(AuditCursor::from_bytes(&longer), None);
        assert_eq!(AuditCursor::from_bytes(&AuditCursor { at_ms: -1, id: Uuid::nil() }.to_bytes()), None);
    }
}
