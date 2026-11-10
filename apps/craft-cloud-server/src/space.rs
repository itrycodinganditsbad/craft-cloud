//! The space layer: which space a request works on, and whether the signed-in account may do what the route does there.
//!
//! [`require_space`] runs after [`crate::auth::require_device`] on every space-scoped route and puts a [`SpaceAccess`] into the
//! request. A request names its space with the `craft-space` header ([`SPACE_HEADER`]); without it the space is the
//! device's personal space and nothing else is looked up, so an agent or app that never heard of team spaces behaves exactly
//! as before. Every answer echoes the space the server worked on, so a client talking to an older server (which ignores the
//! header and answers about the personal space) can tell.
//!
//! - **No access is `404 not_found`**, the same answer as for a space that does not exist. A member whose role is too low is
//!   `403 read_only`, never 401 (the agent reads every 401 on a signed-in route as "this device was signed out").
//! - **Handlers take their space from [`SpaceAccess`]**, never from the device token's own space. The device token's space
//!   stays the account's personal space for the account routes ([`DeviceAuth::personal_space`]).
//! - **The role is checked twice.** The layer reads it from the database on every request (nothing is cached) and refuses
//!   before the handler runs. A write then asks for [`SpaceAccess::meta`], a store handle whose write transaction reads the
//!   role again after locking the space's row, so a role lowered between the two is respected.
//! - **A malformed header** is `400 invalid` and has no echo (there is no space to echo). The layer's own refusals carry no
//!   `craft-epoch` either; the echo of the requested ID is all they say about the space.
//! - **The epoch header** of an answer is the working space's epoch, as the personal space's is for a request without the
//!   header.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use craft_cloud_meta::{DeviceAuth, Meta, UserId, WriteGuard};
use craft_cloud_proto::SpaceId;
use craft_cloud_proto::api::{EPOCH_HEADER, SPACE_HEADER, SpaceRole};

use crate::error::ApiError;
use crate::state::AppState;

/// What the layer decided about the request's space; in the request's extensions for the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceAccess {
    /// The space the request works on.
    pub space: SpaceId,
    /// The caller's role in it (`Owner` in the caller's own personal space).
    pub role: SpaceRole,
    /// Whether it is the caller's personal space.
    pub personal: bool,
    /// The least role the route needs, which `role` has reached.
    pub required: SpaceRole,
}

impl SpaceAccess {
    /// The store handle for a write of this route. In a team space it re-checks the caller's role inside the write
    /// transaction ([`Meta::guarded`]); in the caller's own personal space there is nothing to re-check (its owner is its
    /// only member), and the handle is the plain one.
    pub fn meta(&self, state: &AppState, user: UserId) -> Meta {
        if self.personal { state.meta.clone() } else { state.meta.guarded(WriteGuard { user, min: self.required }) }
    }
}

/// Marks a response the space layer refused itself, so that [`crate::auth::require_device`] adds no `craft-epoch` to it.
#[derive(Debug, Clone, Copy)]
pub struct NoEpoch;

/// The layer's state: the server and the role its route needs.
#[derive(Clone)]
pub struct SpaceGate {
    pub state: Arc<AppState>,
    pub min: SpaceRole,
}

/// `403 read_only`: a member whose role does not allow this.
pub fn read_only() -> ApiError {
    ApiError::new(axum::http::StatusCode::FORBIDDEN, craft_cloud_proto::api::codes::READ_ONLY, "your role in this space does not allow changes")
}

/// The space a request names: `Ok(None)` without the header, `400 invalid` for a header that is not exactly one space ID.
pub fn named_space(request: &Request) -> Result<Option<SpaceId>, ApiError> {
    let mut values = request.headers().get_all(SPACE_HEADER).iter();
    let Some(value) = values.next() else { return Ok(None) };
    let bad = || ApiError::invalid(format!("{SPACE_HEADER} is one space ID"));
    if values.next().is_some() {
        return Err(bad());
    }
    let text = value.to_str().map_err(|_| bad())?;
    // The hyphenated form only: the form the server echoes.
    if text.len() != 36 {
        return Err(bad());
    }
    text.parse::<SpaceId>().map(Some).map_err(|_| bad())
}

/// Resolves the request's space and the caller's role in it, refuses what the route's role does not allow, and echoes
/// the space. After [`crate::auth::require_device`]; a request that is not signed in is `401`.
pub async fn require_space(State(gate): State<SpaceGate>, mut request: Request, next: Next) -> Result<Response, ApiError> {
    let auth = request.extensions().get::<DeviceAuth>().cloned().ok_or_else(ApiError::unauthorized)?;
    let named = named_space(&request)?;
    let space = named.unwrap_or_else(|| auth.personal_space());
    let echo = HeaderValue::from_str(&space.to_string()).ok();
    let work = async {
        let (role, personal) = match named {
            // The device's own space, which no header names: its owner has every right (the device was just looked up with
            // an account that can sign in), so nothing more is read and the epoch is the one `require_device` adds.
            None => (SpaceRole::Owner, true),
            Some(space) => {
                let role = gate.state.meta.space_access(auth.user, space).await?.ok_or_else(|| ApiError::not_found("space"))?;
                (role, space == auth.personal_space())
            }
        };
        if role < gate.min {
            return Err(read_only());
        }
        let epoch = match named {
            Some(space) => Some(gate.state.meta.space_info(space).await?.epoch),
            None => None,
        };
        request.extensions_mut().insert(SpaceAccess { space, role, personal, required: gate.min });
        let mut response = next.run(request).await;
        if let Some(epoch) = epoch
            && let Ok(value) = HeaderValue::from_str(&epoch.to_string())
        {
            response.headers_mut().insert(EPOCH_HEADER, value);
        }
        Ok::<_, ApiError>(response)
    };
    let mut response = match work.await {
        Ok(response) => response,
        Err(e) => {
            // The layer's own refusal says nothing about the space's epoch (a 404 must not): the device layer leaves it out.
            let mut refusal = e.into_response();
            refusal.extensions_mut().insert(NoEpoch);
            refusal
        }
    };
    if let Some(echo) = echo {
        response.headers_mut().insert(SPACE_HEADER, echo);
    }
    Ok(response)
}
