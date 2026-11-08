//! The team-space routes (`docs/design/collaboration.md`, 4.2): the account's spaces and invitations, a space's members,
//! usage, name and life cycle, and the administrator's list, creation, archive and owner routes.
//!
//! - **The space is in the path here**, not in the `craft-space` header: these routes are about the spaces themselves, so
//!   they sit outside the space layer ([`crate::space`]) and decide access with [`Meta::space_access`] or the member row.
//!   No access is `404 not_found`, the same answer as for a space that does not exist; a member whose role does not allow the
//!   change is `403 read_only`.
//! - **Who acts.** A change to a space's members, name or life cycle is made by one of its owners, by their own role and
//!   without a password, or by a server administrator, who carries the administrator confirmation of every admin change (the
//!   password, and the second factor when on; see [`crate::admin`]) and has no access to the space's content. An owner
//!   archiving their space gives their password. Changing a space's limit is an administrator's change always.
//! - **Invitations** are rate limited per account ([`INVITES_PER_MINUTE`]). Unknown, disabled and removed accounts are one
//!   answer (`404 not_found`).
//! - **Wake-ups.** Every change that can end someone's access or lower their role calls
//!   [`Notifier::members_changed`](crate::notify::Notifier::members_changed) after it committed, so the change-feed
//!   long-polls of that space look at their access again at once. Disabling or removing an account does the same for every
//!   space it was a member of ([`wake_spaces_of`]).
//! - Bodies are tiny ([`TINY_JSON_BYTES`]); the `DELETE` and archive routes may carry one (the confirmation) or none.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Extension, FromRequest, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Router, middleware};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_cloud_meta::{AdminVia, DeviceAuth, Member, Role, SpaceActor, SpaceSummary, UserId, UserSpace, valid_username};
use craft_cloud_proto::api::{
    AddSpaceMember, AdminCreateSpace, AdminSetSpaceOwner, AdminSpaceInfo, AdminSpaceList, ChangeSpaceMember, SpaceConfirm, SpaceList, SpaceListItem,
    SpaceMember, SpaceMembers, SpaceRole, SpaceUsage, UpdateSpace, UsageEstimateReason, UsageEstimates,
};
use craft_cloud_proto::colour::member_colour;
use craft_cloud_proto::{SpaceId, SpaceName};
use serde::Deserialize;
use uuid::Uuid;

use crate::account::confirm_password;
use crate::admin::{Confirmation, confirm_actor};
use crate::api::{TINY_JSON_BYTES, body_too_slow};
use crate::auth::require_admin;
use crate::error::ApiError;
use crate::space::read_only;
use crate::state::AppState;

type Shared = Arc<AppState>;

/// Invitations one account may make per minute (the window of [`AppState::space_invites`]).
pub const INVITES_PER_MINUTE: u32 = 30;
/// Spaces one page of `GET /admin/spaces` holds when `limit` is not given, and the most it may hold.
const DEFAULT_PAGE: u32 = 50;
const MAX_PAGE: u32 = 200;

/// The routes about the spaces an account is in, behind `require_device` only (the space is in the path).
pub fn routes() -> Router<Shared> {
    let tiny = || DefaultBodyLimit::max(TINY_JSON_BYTES);
    Router::new()
        .route("/spaces", get(list_spaces))
        .route("/spaces/{id}", axum::routing::patch(update_space).layer(tiny()))
        .route("/spaces/{id}/members", get(list_members).post(add_member).layer(tiny()))
        .route("/spaces/{id}/members/{user}", axum::routing::patch(change_member).delete(remove_member).layer(tiny()))
        .route("/spaces/{id}/accept", post(accept))
        .route("/spaces/{id}/decline", post(decline))
        .route("/spaces/{id}/usage", get(usage))
        .route("/spaces/{id}/archive", post(archive).layer(tiny()))
        .route("/spaces/{id}/unarchive", post(unarchive).layer(tiny()))
}

/// The administrator's routes, behind `require_admin` like the rest of `/admin`.
pub fn admin_routes() -> Router<Shared> {
    let tiny = || DefaultBodyLimit::max(TINY_JSON_BYTES);
    Router::new()
        .route("/admin/spaces", get(admin_list).post(admin_create).layer(tiny()))
        .route("/admin/spaces/{id}/archive", post(admin_archive).layer(tiny()))
        .route("/admin/spaces/{id}/unarchive", post(admin_unarchive).layer(tiny()))
        .route("/admin/spaces/{id}/owner", axum::routing::put(admin_set_owner).layer(tiny()))
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn(crate::web::reject_mixed_credentials))
}

// ---- helpers ----

fn space_id(id: Result<Path<SpaceId>, PathRejection>) -> Result<SpaceId, ApiError> {
    Ok(id?.0)
}

/// A JSON body of at most [`TINY_JSON_BYTES`] under the usual deadline; none at all is the type's default (for the routes whose
/// body only carries a confirmation an owner does not need).
async fn optional_body<T: serde::de::DeserializeOwned + Default>(state: &AppState, request: Request) -> Result<T, ApiError> {
    let bytes = match tokio::time::timeout(state.limits.body_time(TINY_JSON_BYTES), Bytes::from_request(request, &())).await {
        Ok(bytes) => bytes?,
        Err(_) => return Err(body_too_slow()),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::invalid("the body must be a JSON object with the fields of this call"))
}

fn confirmation(confirm: SpaceConfirm) -> Confirmation {
    Confirmation::new(confirm.password, confirm.totp_code, confirm.recovery_code)
}

fn user_actor(auth: &DeviceAuth) -> SpaceActor {
    SpaceActor::User { user: auth.user, device: Some(auth.device) }
}

/// Who makes a change to the space's members, name or life cycle: an owner, by their role; otherwise an administrator, with
/// the confirmation; a member who is neither is `read_only`, and anyone else gets the answer for a space that is not there.
async fn actor_for(state: &AppState, auth: &DeviceAuth, space: SpaceId, confirm: SpaceConfirm) -> Result<SpaceActor, ApiError> {
    let role = state.meta.space_member_role(auth.user, space).await?;
    // An administrator who sends the confirmation acts as an administrator, also in a space they own: the change is then
    // audited and confirmed as an administrator's (an owner's own change carries no password).
    let confirmed = confirm.password.is_some() || confirm.totp_code.is_some() || confirm.recovery_code.is_some();
    if auth.role == Role::Admin && confirmed {
        return admin_actor(state, auth, confirm).await;
    }
    if role == Some(SpaceRole::Owner) {
        return Ok(user_actor(auth));
    }
    if auth.role == Role::Admin {
        return admin_actor(state, auth, confirm).await;
    }
    Err(if role.is_some() { read_only() } else { ApiError::not_found("space") })
}

async fn admin_actor(state: &AppState, auth: &DeviceAuth, confirm: SpaceConfirm) -> Result<SpaceActor, ApiError> {
    let admin = confirm_actor(state, auth.user, &auth.username, AdminVia::Device(auth.device), confirmation(confirm)).await?;
    Ok(SpaceActor::Admin(admin))
}

fn item(space: UserSpace) -> SpaceListItem {
    SpaceListItem {
        id: space.id,
        kind: if space.personal { "personal" } else { "team" }.to_owned(),
        name: space.name,
        role: space.role,
        state: if space.invited { "invited" } else { "active" }.to_owned(),
        archived: space.archived,
        created_ms: space.created_ms,
        members: space.members,
        root: space.root,
        epoch: space.epoch,
        joined_ms: space.joined_ms,
    }
}

fn member_info(member: Member) -> SpaceMember {
    let state = if member.accepted() { "active" } else { "invited" }.to_owned();
    SpaceMember { user: member.user.0, username: member.username, role: member.role, state, colour: member_colour(member.user.0) }
}

fn summary(space: SpaceSummary) -> AdminSpaceInfo {
    AdminSpaceInfo {
        id: space.id,
        name: space.name,
        created_ms: space.created_ms,
        archived_ms: space.archived_ms,
        members: space.members,
        owners: space.owners,
        stored_bytes: space.stored_bytes,
        quota_bytes: space.quota_bytes,
    }
}

fn space_name(text: &str) -> Result<SpaceName, ApiError> {
    SpaceName::new(text).map_err(|e| ApiError::invalid(format!("a space's name: {e}")))
}

/// After a committed change: the space's long-polls look at their access again.
pub(crate) fn changed(state: &AppState, space: SpaceId) {
    state.notifier.members_changed(space);
}

/// After an account was disabled or marked for removal: the long-polls of every team space it was a member of look again,
/// and so do its live connections (a connection that has lost its access ends).
pub(crate) async fn wake_spaces_of(state: &AppState, user: UserId) {
    state.live.revoke_user(user);
    match state.meta.member_space_ids(user).await {
        Ok(spaces) => spaces.into_iter().for_each(|space| changed(state, space)),
        Err(e) => tracing::warn!(error = %e, "couldn't list the spaces to wake"),
    }
}

/// Counts one invitation against the account's minute.
fn count_invite(state: &AppState, user: UserId) -> Result<(), ApiError> {
    let key = format!("user:{}", user.0);
    let now = tokio::time::Instant::now().into_std();
    if state.space_invites.allow(&key, INVITES_PER_MINUTE, now) {
        Ok(())
    } else {
        let wait = state.space_invites.retry_after(&key, now);
        // Rounded up: told to wait 1 s when 1.4 s are left, a client would be refused again.
        let secs = wait.as_secs().saturating_add(u64::from(wait.subsec_nanos() > 0)).max(1);
        Err(ApiError::rate_limited("too many invitations; wait a minute", Some(u32::try_from(secs).unwrap_or(60))))
    }
}

// ---- the account's spaces ----

/// `GET /spaces`.
async fn list_spaces(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>) -> Result<Json<SpaceList>, ApiError> {
    let spaces = state.meta.user_spaces(auth.user).await?;
    Ok(Json(SpaceList { spaces: spaces.into_iter().map(item).collect() }))
}

/// `POST /spaces/{id}/accept`: the invited account joins; the answer is the space as `GET /spaces` lists it.
async fn accept(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
) -> Result<Json<SpaceListItem>, ApiError> {
    let space = space_id(id)?;
    state.meta.accept_invitation(auth.user, Some(auth.device), space, state.now_ms()).await?;
    changed(&state, space);
    let listed = state.meta.user_spaces(auth.user).await?.into_iter().find(|s| s.id == space).ok_or_else(|| ApiError::not_found("space"))?;
    Ok(Json(item(listed)))
}

/// `POST /spaces/{id}/decline`: 204.
async fn decline(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let space = space_id(id)?;
    state.meta.decline_invitation(auth.user, Some(auth.device), space, state.now_ms()).await?;
    changed(&state, space);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /spaces/{id}/usage`: any member.
async fn usage(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
) -> Result<Json<SpaceUsage>, ApiError> {
    let space = space_id(id)?;
    state.meta.space_access(auth.user, space).await?.ok_or_else(|| ApiError::not_found("space"))?;
    let estimate = state.meta.storage_estimate(space).await?;
    let index_complete = estimate.reason.is_none();
    Ok(Json(SpaceUsage {
        stored_bytes: estimate.stored_bytes,
        quota_bytes: estimate.quota_bytes,
        trash_bytes: estimate.trash_bytes,
        history_bytes: estimate.history_bytes,
        live_bytes: state.meta.space_live_bytes(space).await?,
        estimates: UsageEstimates { index_complete, reason: estimate.reason.map(|_| UsageEstimateReason::IndexIncomplete) },
    }))
}

// ---- members ----

/// `GET /spaces/{id}/members`: any member, invitations included.
async fn list_members(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
) -> Result<Json<SpaceMembers>, ApiError> {
    let space = space_id(id)?;
    state.meta.space_access(auth.user, space).await?.ok_or_else(|| ApiError::not_found("space"))?;
    let members = state.meta.list_members(space).await?;
    Ok(Json(SpaceMembers { members: members.into_iter().map(member_info).collect() }))
}

/// `POST /spaces/{id}/members`: an owner (or an administrator with the confirmation) invites an account. The answer is the
/// member row, `invited`.
async fn add_member(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
    request: Request,
) -> Result<Json<SpaceMember>, ApiError> {
    let space = space_id(id)?;
    let req: AddSpaceMember = crate::api::read_json(&state, request, TINY_JSON_BYTES).await?;
    if !valid_username(&req.username) {
        return Err(ApiError::invalid("usernames are 1-64 letters, digits, '.', '_' or '-'"));
    }
    let actor = actor_for(&state, &auth, space, req.confirm).await?;
    count_invite(&state, auth.user)?;
    // Unknown, disabled and removed accounts are one answer: the store says `NotFound("user")` for the last two as well.
    let target = state.meta.user_by_name(&req.username).await?.ok_or_else(|| ApiError::not_found("user"))?;
    let member = state.meta.invite_member(&actor, space, target.id, req.role, state.now_ms()).await?;
    changed(&state, space);
    Ok(Json(member_info(member)))
}

/// `PATCH /spaces/{id}/members/{user}`: a new role for a member or an invitation; lowering the last owner is `409 last_owner`.
async fn change_member(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    ids: Result<Path<(SpaceId, Uuid)>, PathRejection>,
    request: Request,
) -> Result<Json<SpaceMember>, ApiError> {
    let (space, target) = ids?.0;
    let req: ChangeSpaceMember = crate::api::read_json(&state, request, TINY_JSON_BYTES).await?;
    let actor = actor_for(&state, &auth, space, req.confirm).await?;
    let member = state.meta.change_member_role(&actor, space, UserId(target), req.role, state.now_ms()).await?;
    changed(&state, space);
    Ok(Json(member_info(member)))
}

/// `DELETE /spaces/{id}/members/{user}`: 204. An owner (or an administrator with the confirmation) removes a member or
/// withdraws an invitation; a member's own row is the member leaving (an invited account's own row is declined) and needs no
/// role. The last owner leaving or being removed is `409 last_owner`.
async fn remove_member(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    ids: Result<Path<(SpaceId, Uuid)>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let (space, target) = ids?.0;
    let confirm: SpaceConfirm = optional_body(&state, request).await?;
    let now = state.now_ms();
    if target == auth.user.0 {
        match state.meta.leave_space(auth.user, Some(auth.device), space, now).await {
            Err(craft_cloud_meta::MetaError::NotFound("member")) => state.meta.decline_invitation(auth.user, Some(auth.device), space, now).await?,
            other => other?,
        }
    } else {
        let actor = actor_for(&state, &auth, space, confirm).await?;
        state.meta.remove_member(&actor, space, UserId(target), now).await?;
    }
    changed(&state, space);
    Ok(StatusCode::NO_CONTENT)
}

// ---- name, limit, life cycle ----

/// `PATCH /spaces/{id}`: a new name (an owner, or an administrator with the confirmation) and/or a new limit (an
/// administrator with the confirmation). The answer is the space as the administrator's list shows it.
async fn update_space(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
    request: Request,
) -> Result<Json<AdminSpaceInfo>, ApiError> {
    let space = space_id(id)?;
    let req: UpdateSpace = crate::api::read_json(&state, request, TINY_JSON_BYTES).await?;
    if req.name.is_none() && req.quota_bytes.is_none() {
        return Err(ApiError::invalid("give a name or a quota_bytes to change"));
    }
    let name = req.name.as_deref().map(space_name).transpose()?;
    let now = state.now_ms();
    // Checked before anything is written, so a bad limit cannot follow a rename that already committed.
    if let Some(Some(quota)) = req.quota_bytes
        && !(craft_cloud_meta::MIN_QUOTA_BYTES..=i64::MAX as u64).contains(&quota)
    {
        return Err(ApiError::invalid("quota_bytes is at least one chunk (4194304) and at most 9223372036854775807"));
    }
    if let Some(quota) = req.quota_bytes {
        if auth.role != Role::Admin {
            // A member learns that this is not theirs to change; anyone else learns nothing about the space.
            return Err(if state.meta.space_member_role(auth.user, space).await?.is_some() {
                ApiError::forbidden("only an administrator changes a space's limit")
            } else {
                ApiError::not_found("space")
            });
        }
        let SpaceActor::Admin(admin) = admin_actor(&state, &auth, req.confirm).await? else { return Err(ApiError::internal()) };
        if let Some(name) = &name {
            state.meta.rename_team_space(&SpaceActor::Admin(admin.clone()), space, name, now).await?;
        }
        state.meta.set_space_quota(&admin, space, quota, now).await?;
    } else if let Some(name) = &name {
        let actor = actor_for(&state, &auth, space, req.confirm).await?;
        state.meta.rename_team_space(&actor, space, name, now).await?;
    }
    changed(&state, space);
    Ok(Json(summary(state.meta.admin_space(space).await?)))
}

/// An owner archives or unarchives their space with their own password.
async fn owner_archive(
    state: &AppState,
    auth: &DeviceAuth,
    id: Result<Path<SpaceId>, PathRejection>,
    request: Request,
    archived: bool,
) -> Result<StatusCode, ApiError> {
    let space = space_id(id)?;
    let confirm: SpaceConfirm = optional_body(state, request).await?;
    match state.meta.space_member_role(auth.user, space).await? {
        Some(SpaceRole::Owner) => {}
        Some(_) => return Err(read_only()),
        None => return Err(ApiError::not_found("space")),
    }
    let password = confirm.password.ok_or_else(|| ApiError::invalid("this change needs your current password in `password`"))?;
    confirm_password(state, auth.user, &auth.username, password).await?;
    state.meta.set_space_archived(&user_actor(auth), space, archived, state.now_ms()).await?;
    changed(state, space);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /spaces/{id}/archive`: 204. Every member becomes read-only and the space leaves the lists of all but its owners.
async fn archive(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    owner_archive(&state, &auth, id, request, true).await
}

/// `POST /spaces/{id}/unarchive`: 204.
async fn unarchive(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    owner_archive(&state, &auth, id, request, false).await
}

// ---- the administrator ----

#[derive(Debug, Deserialize)]
struct ListQuery {
    cursor: Option<String>,
    limit: Option<u32>,
    query: Option<String>,
}

/// `GET /admin/spaces?cursor&limit&query`: the team spaces by name, with counts and sizes. A cursor is opaque.
async fn admin_list(State(state): State<Shared>, q: Result<Query<ListQuery>, QueryRejection>) -> Result<Json<AdminSpaceList>, ApiError> {
    let Query(q) = q?;
    let limit = q.limit.unwrap_or(DEFAULT_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(ApiError::invalid(format!("limit is 1 to {MAX_PAGE}")));
    }
    let bad = || ApiError::invalid("the cursor is not one this server gave");
    let after = match q.cursor.as_deref() {
        Some(cursor) => Some(String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).map_err(|_| bad())?).map_err(|_| bad())?),
        None => None,
    };
    let search = q.query.as_deref().filter(|s| !s.is_empty());
    if let Some(search) = search
        && (search.len() > crate::admin::MAX_QUERY_BYTES || search.chars().any(char::is_control))
    {
        return Err(ApiError::invalid(format!("query is at most {} bytes of printable characters", crate::admin::MAX_QUERY_BYTES)));
    }
    let page = state.meta.admin_list_spaces(after.as_deref(), search, limit).await?;
    Ok(Json(AdminSpaceList {
        spaces: page.spaces.into_iter().map(summary).collect(),
        next: page.next_after.map(|key| URL_SAFE_NO_PAD.encode(key.as_bytes())),
    }))
}

/// `POST /admin/spaces`: a team space with its first owner, who is an accepted member from the start.
async fn admin_create(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    request: Request,
) -> Result<Json<AdminSpaceInfo>, ApiError> {
    let req: AdminCreateSpace = crate::api::read_json(&state, request, TINY_JSON_BYTES).await?;
    let name = space_name(&req.name)?;
    let actor = admin_actor(&state, &auth, req.confirm).await?;
    let space = state.meta.create_team_space(&actor, &name, UserId(req.owner), req.quota_bytes, state.now_ms()).await?;
    tracing::info!(actor = %auth.device, %space, "admin: team space created");
    Ok(Json(summary(state.meta.admin_space(space).await?)))
}

async fn admin_archive_to(
    state: &AppState,
    auth: &DeviceAuth,
    id: Result<Path<SpaceId>, PathRejection>,
    request: Request,
    archived: bool,
) -> Result<StatusCode, ApiError> {
    let space = space_id(id)?;
    let confirm: SpaceConfirm = crate::api::read_json(state, request, TINY_JSON_BYTES).await?;
    let actor = admin_actor(state, auth, confirm).await?;
    state.meta.set_space_archived(&actor, space, archived, state.now_ms()).await?;
    changed(state, space);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /admin/spaces/{id}/archive`: 204.
async fn admin_archive(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    admin_archive_to(&state, &auth, id, request, true).await
}

/// `POST /admin/spaces/{id}/unarchive`: 204.
async fn admin_unarchive(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    admin_archive_to(&state, &auth, id, request, false).await
}

/// `PUT /admin/spaces/{id}/owner`: 204. Makes an account an accepted owner (for a space whose owners are all gone).
async fn admin_set_owner(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<SpaceId>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let space = space_id(id)?;
    let req: AdminSetSpaceOwner = crate::api::read_json(&state, request, TINY_JSON_BYTES).await?;
    let SpaceActor::Admin(admin) = admin_actor(&state, &auth, req.confirm).await? else { return Err(ApiError::internal()) };
    state.meta.set_owner_accepted(&admin, space, UserId(req.user), state.now_ms()).await?;
    changed(&state, space);
    Ok(StatusCode::NO_CONTENT)
}
