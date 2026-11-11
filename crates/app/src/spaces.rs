//! Team spaces through the agent's local API (`/v1/spaces`): the spaces of the account with what this computer does with
//! each, accepting and declining an invitation (passed on to the server), and turning the sync of a team space on or
//! off here. A call that asks the server ([`Client::accept_space`], [`Client::decline_space`]) waits for it: call it off
//! the UI thread. An agent from before team spaces answers [`codes::AGENT_TOO_OLD`] to all of them.
//!
//! The members and the usage of a space, leaving one, renaming it and the administrator's team space calls are passed on
//! to the server by the agent (`docs/agent.md`); the server decides who may do what: a viewer who tries to invite is
//! `Refused { code: "read_only" }`, an owner who removes the last owner is `last_owner`, and so on. The administrator's
//! confirmation (password, and a code with two-factor sign-in) goes into the body of each change and is kept by neither the
//! agent nor this crate.

use craft_cloud_api::SpaceId;
use craft_cloud_api::api::{
    AddSpaceMember, AdminCreateSpace, AdminSetSpaceOwner, AdminSpaceInfo, AdminSpaceList, ChangeSpaceMember, SpaceConfirm, SpaceListItem,
    SpaceMember, SpaceMembers, SpaceUsage, UpdateSpace,
};
use craft_cloud_api::local::{
    ApproveDeletesAnswer, ApproveDeletesRequest, FromFolderRequest, FromFolderStatus, SpaceInfo, SpaceList, SpaceSyncRequest, paths,
};

use crate::admin::UserQuery;
use crate::{Client, Error, Uuid, codes, http};

/// An older agent's "no such call" as [`codes::AGENT_TOO_OLD`]. The server's own `not_found` (no such invitation) has a
/// message of its own and passes through.
fn too_old(e: Error) -> Error {
    match e {
        Error::Refused { code, message, .. } if code == craft_cloud_api::api::codes::NOT_FOUND && message == "no such call" => Error::Refused {
            code: codes::AGENT_TOO_OLD.into(),
            message: "the Craft Cloud agent on this computer is too old for team spaces: update Craft Cloud".into(),
            name_taken: None,
        },
        other => other,
    }
}

fn space_path(id: SpaceId, call: &str) -> String {
    format!("{}/{id}/{call}", paths::SPACES)
}

fn body<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    serde_json::to_vec(value).map_err(|e| Error::Protocol(e.to_string()))
}

impl Client {
    /// The personal space and the team spaces of the account, invitations among them, as the agent last saw the
    /// server's list, with the folder, the sync and its state on this computer. Answered by the agent itself (a few
    /// seconds at most): fine from a UI thread.
    pub fn spaces(&self) -> Result<SpaceList, Error> {
        self.call("GET", paths::SPACES, None, http::COMMAND_TIMEOUT).map_err(too_old)
    }

    /// Accepts an invitation to a team space (the server's answer: the space as it lists it). The space then starts with
    /// syncing off on this computer: [`Client::set_space_sync`] turns it on.
    pub fn accept_space(&self, id: SpaceId) -> Result<SpaceListItem, Error> {
        self.call("POST", &space_path(id, "accept"), None, http::SLOW_TIMEOUT).map_err(too_old)
    }

    /// Declines an invitation to a team space.
    pub fn decline_space(&self, id: SpaceId) -> Result<(), Error> {
        self.answer("POST", &space_path(id, "decline"), None, http::SLOW_TIMEOUT).map(|_| ()).map_err(too_old)
    }

    /// Turns the sync of a team space on this computer on or off (optionally with another folder, see
    /// [`SpaceSyncRequest`]). Answers the space as `GET /v1/spaces` lists it.
    pub fn set_space_sync(&self, id: SpaceId, request: &SpaceSyncRequest) -> Result<SpaceInfo, Error> {
        let body = serde_json::to_vec(request).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", &space_path(id, "sync"), Some(body), http::SLOW_TIMEOUT).map_err(too_old)
    }

    /// Starts "make a team space from this folder": a new team space with this account as its first owner, the folder's files moved
    /// into it, the old folder deleted here once the team space holds every file ([`FromFolderRequest`]). It is an administrator's
    /// call (`request.confirm` is the administrator's own). The agent checks the folder and answers at once; the work goes on in
    /// the agent and [`Client::status`] reports it in `from_folder`. A server that has no team spaces is
    /// [`codes::SERVER_TOO_OLD`], an agent without this call [`codes::AGENT_TOO_OLD`].
    pub fn make_space_from_folder(&self, request: &FromFolderRequest) -> Result<FromFolderStatus, Error> {
        self.call("POST", paths::SPACES_FROM_FOLDER, Some(body(request)?), http::SLOW_TIMEOUT).map_err(too_old)
    }

    /// Approves the deletions the mass-delete guard holds back in a team space, as `craft-cloud-agent sync
    /// --approve-deletes --space` does. With `set`, only that set (the identifier the list was shown with) and only if it is
    /// the one waiting; without, the set the previous call of this kind listed (the first call lists and approves nothing).
    /// The running agent carries the approved deletions out in its next pass. `approved` in the answer says whether an
    /// approval was handed over.
    pub fn approve_space_deletes(&self, id: SpaceId, set: Option<&str>) -> Result<ApproveDeletesAnswer, Error> {
        let body = serde_json::to_vec(&ApproveDeletesRequest { set: set.map(str::to_owned) }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", &space_path(id, "approve-deletes"), Some(body), http::COMMAND_TIMEOUT).map_err(too_old)
    }

    /// The members of a team space and the accounts invited to it, with their roles. Any member may ask.
    pub fn space_members(&self, id: SpaceId) -> Result<SpaceMembers, Error> {
        self.call("GET", &space_path(id, "members"), None, http::SLOW_TIMEOUT).map_err(too_old)
    }

    /// Invites an account to a team space with a role (an owner's call; `request.confirm` stays empty). The account joins
    /// when it accepts.
    pub fn add_space_member(&self, id: SpaceId, request: &AddSpaceMember) -> Result<SpaceMember, Error> {
        self.call("POST", &space_path(id, "members"), Some(body(request)?), http::SLOW_TIMEOUT).map_err(too_old)
    }

    /// Gives a member (or an invited account) another role (an owner's call).
    pub fn change_space_member(&self, id: SpaceId, user: Uuid, request: &ChangeSpaceMember) -> Result<SpaceMember, Error> {
        self.call("PATCH", &format!("{}/{user}", space_path(id, "members")), Some(body(request)?), http::SLOW_TIMEOUT).map_err(too_old)
    }

    /// Removes a member or withdraws an invitation (an owner's call). The person keeps what they already downloaded.
    pub fn remove_space_member(&self, id: SpaceId, user: Uuid) -> Result<(), Error> {
        self.answer("DELETE", &format!("{}/{user}", space_path(id, "members")), None, http::SLOW_TIMEOUT).map(|_| ()).map_err(too_old)
    }

    /// Leaves a team space: this account is no longer a member (any role may; the last owner may not). The server goes first: a
    /// refusal changes nothing on this computer. After it the space is lost to the agent: the sync ends and the folder stays, with
    /// its files, as a plain folder.
    pub fn leave_space(&self, id: SpaceId) -> Result<(), Error> {
        self.answer("POST", &space_path(id, "leave"), None, http::SLOW_TIMEOUT).map(|_| ()).map_err(too_old)
    }

    /// What a team space stores, against its limit.
    pub fn space_usage(&self, id: SpaceId) -> Result<SpaceUsage, Error> {
        self.call("GET", &space_path(id, "usage"), None, http::SLOW_TIMEOUT).map_err(too_old)
    }

    /// A new name and/or limit for a team space ([`UpdateSpace`]: an owner renames; a limit is an administrator's, with
    /// the confirmation in `request.confirm`). Answers the space as the administrator's list shows it.
    pub fn update_space(&self, id: SpaceId, request: &UpdateSpace) -> Result<AdminSpaceInfo, Error> {
        self.call("PATCH", &format!("{}/{id}", paths::SPACES), Some(body(request)?), http::SLOW_TIMEOUT).map_err(too_old)
    }

    /// A page of the server's team spaces by name, for an administrator (asked like [`Client::admin_users`]: the cursor of
    /// the page before, the page size and a name to search).
    pub fn admin_spaces(&self, query: &UserQuery) -> Result<AdminSpaceList, Error> {
        self.call("GET", &query.path_at(paths::ADMIN_SPACES), None, http::SLOW_TIMEOUT).map_err(too_old)
    }

    /// A new team space with its first owner, an administrator's call (`request.confirm` is theirs).
    pub fn admin_create_space(&self, request: &AdminCreateSpace) -> Result<AdminSpaceInfo, Error> {
        self.call("POST", paths::ADMIN_SPACES, Some(body(request)?), http::SLOW_TIMEOUT).map_err(too_old)
    }

    /// Archives a team space: every member can only read it, and it leaves the lists of all but its owners.
    pub fn admin_archive_space(&self, id: SpaceId, confirm: &SpaceConfirm) -> Result<(), Error> {
        let path = format!("{}/{id}/archive", paths::ADMIN_SPACES);
        self.answer("POST", &path, Some(body(confirm)?), http::SLOW_TIMEOUT).map(|_| ()).map_err(too_old)
    }

    /// Brings an archived team space back.
    pub fn admin_unarchive_space(&self, id: SpaceId, confirm: &SpaceConfirm) -> Result<(), Error> {
        let path = format!("{}/{id}/unarchive", paths::ADMIN_SPACES);
        self.answer("POST", &path, Some(body(confirm)?), http::SLOW_TIMEOUT).map(|_| ()).map_err(too_old)
    }

    /// Makes an account an accepted owner of a team space (the other owners stay owners).
    pub fn admin_set_space_owner(&self, id: SpaceId, request: &AdminSetSpaceOwner) -> Result<(), Error> {
        let path = format!("{}/{id}/owner", paths::ADMIN_SPACES);
        self.answer("PUT", &path, Some(body(request)?), http::SLOW_TIMEOUT).map(|_| ()).map_err(too_old)
    }
}
