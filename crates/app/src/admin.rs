//! The admin calls of the agent's local API (`/v1/admin/…`), which the agent passes on to the server's admin API for the
//! Craft Cloud app's admin pages. Every one asks the server (an Argon2 check of the admin's password among it), so call
//! them **off the UI thread**.
//!
//! The server decides everything: a member's call is `Refused { code: "forbidden" }`, a changing call without the
//! admin's current password, or with a wrong one, is `invalid` or `wrong_password` (HTTP 403, never 401), and a change
//! that would leave the server without an enabled admin is `last_admin`. A `rate_limited` or `two_factor_locked` refusal
//! carries the seconds to wait ([`Error::retry_after_secs`]). The password is sent in the body of each change
//! ([`AdminConfirm::password`] and the same field of the other bodies), goes to the server and is kept by neither the
//! agent nor this crate; the types' `Debug` output hides it.

use craft_cloud_api::local::paths;

use crate::{
    AdminCode, AdminConfirm, AdminCreateUser, AdminCreatedUser, AdminDevices, AdminPasswordReset, AdminServerInfo, AdminUpdateUser, AdminUserInfo,
    AdminUserList, Client, DeviceId, Error, Uuid, http,
};

/// Which accounts [`Client::admin_users`] lists: a page by username, optionally those whose name contains `query`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserQuery {
    /// The `next` of the page before (opaque); none for the first page.
    pub cursor: Option<String>,
    /// Accounts on the page, 1 to 200; none: the server's default (50).
    pub limit: Option<u32>,
    /// Only usernames containing this text (case doesn't matter, at most 64 bytes); none or empty: all.
    pub query: Option<String>,
}

impl UserQuery {
    /// The path with its query string.
    fn path(&self) -> String {
        let mut pairs = Vec::new();
        if let Some(cursor) = self.cursor.as_deref().filter(|c| !c.is_empty()) {
            pairs.push(format!("cursor={}", encode(cursor)));
        }
        if let Some(limit) = self.limit {
            pairs.push(format!("limit={limit}"));
        }
        if let Some(query) = self.query.as_deref().filter(|q| !q.is_empty()) {
            pairs.push(format!("query={}", encode(query)));
        }
        if pairs.is_empty() { paths::ADMIN_USERS.to_owned() } else { format!("{}?{}", paths::ADMIN_USERS, pairs.join("&")) }
    }
}

/// `text` percent-encoded for a query string (everything but unreserved characters).
pub(crate) fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn body<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    serde_json::to_vec(value).map_err(|e| Error::Protocol(e.to_string()))
}

fn user_path(id: Uuid) -> String {
    format!("{}/{id}", paths::ADMIN_USERS)
}

impl Client {
    /// The server's counters and kinds. Only for an admin: any other account is `Refused { code: "forbidden" }`, which
    /// is how the app tells whether to show its admin pages.
    pub fn admin_server(&self) -> Result<AdminServerInfo, Error> {
        self.call("GET", paths::ADMIN_SERVER, None, http::SLOW_TIMEOUT)
    }

    /// A page of accounts by username ([`UserQuery`]); `next` of the answer is the cursor of the following page.
    pub fn admin_users(&self, query: &UserQuery) -> Result<AdminUserList, Error> {
        self.call("GET", &query.path(), None, http::SLOW_TIMEOUT)
    }

    /// A new account without a password, with an invite (its code is in the answer, once). `request.password` is the
    /// acting admin's current password.
    pub fn admin_create_user(&self, request: &AdminCreateUser) -> Result<AdminCreatedUser, Error> {
        self.call("POST", paths::ADMIN_USERS, Some(body(request)?), http::SLOW_TIMEOUT)
    }

    /// A new role and/or quota for an account (`request.password` is the acting admin's current password).
    pub fn admin_update_user(&self, id: Uuid, request: &AdminUpdateUser) -> Result<AdminUserInfo, Error> {
        self.call("PATCH", &user_path(id), Some(body(request)?), http::SLOW_TIMEOUT)
    }

    /// Disables an account: its devices are signed out, and it can't sign in.
    pub fn admin_disable_user(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
        self.confirmed("POST", &format!("{}/disable", user_path(id)), confirm)
    }

    /// Enables a disabled account again (its signed-out devices stay signed out).
    pub fn admin_enable_user(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
        self.confirmed("POST", &format!("{}/enable", user_path(id)), confirm)
    }

    /// Marks an account for removal; nothing it stored is deleted.
    pub fn admin_remove_user(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
        self.confirmed("DELETE", &user_path(id), confirm)
    }

    /// A new invite for an account that has no password yet; the code is in the answer, once.
    pub fn admin_issue_invite(&self, id: Uuid, confirm: &AdminConfirm) -> Result<AdminCode, Error> {
        self.call("POST", &format!("{}/invite", user_path(id)), Some(body(confirm)?), http::SLOW_TIMEOUT)
    }

    /// Ends the account's open invite or reset code.
    pub fn admin_revoke_invite(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
        self.confirmed("DELETE", &format!("{}/invite", user_path(id)), confirm)
    }

    /// A password reset code for an account (good for an hour); it is in the answer, once. `request.reset_two_factor` says
    /// whether redeeming it also turns the account's two-factor sign-in off.
    pub fn admin_issue_reset(&self, id: Uuid, request: &AdminPasswordReset) -> Result<AdminCode, Error> {
        self.call("POST", &format!("{}/password-reset", user_path(id)), Some(body(request)?), http::SLOW_TIMEOUT)
    }

    /// Takes an account's two-factor sign-in away and signs every device of it out (the server refuses it for the
    /// admin's own account).
    pub fn admin_reset_two_factor(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
        self.confirmed("POST", &format!("{}/reset-two-factor", user_path(id)), confirm)
    }

    /// Every device of an account, revoked ones with when and why.
    pub fn admin_devices(&self, id: Uuid) -> Result<AdminDevices, Error> {
        self.call("GET", &format!("{}/devices", user_path(id)), None, http::SLOW_TIMEOUT)
    }

    /// Signs one device of an account out.
    pub fn admin_revoke_device(&self, id: Uuid, device: DeviceId, confirm: &AdminConfirm) -> Result<(), Error> {
        self.confirmed("DELETE", &format!("{}/devices/{device}", user_path(id)), confirm)
    }

    /// A change whose only body is the confirmation, answered without one.
    fn confirmed(&self, method: &str, path: &str, confirm: &AdminConfirm) -> Result<(), Error> {
        self.answer(method, path, Some(body(confirm)?), http::SLOW_TIMEOUT).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_query_is_written_as_a_query_string() {
        assert_eq!(UserQuery::default().path(), "/v1/admin/users");
        let all = UserQuery { cursor: Some("YW5h".into()), limit: Some(25), query: Some("a b&c=é".into()) };
        assert_eq!(all.path(), "/v1/admin/users?cursor=YW5h&limit=25&query=a%20b%26c%3D%C3%A9");
        let empty = UserQuery { cursor: Some(String::new()), limit: None, query: Some(String::new()) };
        assert_eq!(empty.path(), "/v1/admin/users", "empty means none");
    }
}
