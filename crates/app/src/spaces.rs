//! Team spaces through the agent's local API (`/v1/spaces`): the spaces of the account with what this computer does with
//! each, accepting and declining an invitation (passed on to the server), and turning the sync of a team space on or
//! off here. A call that asks the server ([`Client::accept_space`], [`Client::decline_space`]) waits for it: call it off
//! the UI thread. An agent from before team spaces answers [`codes::AGENT_TOO_OLD`] to all of them.

use craft_cloud_api::SpaceId;
use craft_cloud_api::api::SpaceListItem;
use craft_cloud_api::local::{ApproveDeletesAnswer, ApproveDeletesRequest, SpaceInfo, SpaceList, SpaceSyncRequest, paths};

use crate::{Client, Error, codes, http};

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

    /// Approves the deletions the mass-delete guard holds back in a team space, as `craft-cloud-agent sync
    /// --approve-deletes --space` does. With `set`, only that set (the identifier the list was shown with) and only if it is
    /// the one waiting; without, the set the previous call of this kind listed (the first call lists and approves nothing).
    /// The running agent carries the approved deletions out in its next pass. `approved` in the answer says whether an
    /// approval was handed over.
    pub fn approve_space_deletes(&self, id: SpaceId, set: Option<&str>) -> Result<ApproveDeletesAnswer, Error> {
        let body = serde_json::to_vec(&ApproveDeletesRequest { set: set.map(str::to_owned) }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", &space_path(id, "approve-deletes"), Some(body), http::COMMAND_TIMEOUT).map_err(too_old)
    }
}
