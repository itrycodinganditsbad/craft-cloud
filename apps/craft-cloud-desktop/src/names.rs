//! Names for the IDs the pages would otherwise show: the computers of the account (from the devices list the app already
//! asks for) and the accounts it has met (its own, and the admin's list of accounts). An ID nobody knows (a computer that
//! was removed, another account) stays a short ID, with the whole ID as its hover text.

use craft_cloud_app::DeviceInfo;

use crate::audit::short_id;

/// What a page shows for an ID: the text, the hover text with the whole ID, and whether a name was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub text: String,
    pub full: String,
    pub known: bool,
}

/// The names known so far, by ID.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    computers: Vec<(String, String)>,
    accounts: Vec<(String, String)>,
}

fn remember(list: &mut Vec<(String, String)>, id: String, name: &str) {
    if name.is_empty() {
        return;
    }
    match list.iter_mut().find(|(known, _)| *known == id) {
        Some(entry) => entry.1 = name.to_owned(),
        None => list.push((id, name.to_owned())),
    }
}

fn find(list: &[(String, String)], id: &str, kind: &str) -> Named {
    match list.iter().find(|(known, _)| known == id) {
        Some((_, name)) => Named { text: name.clone(), full: format!("{kind} {name}\n{id}"), known: true },
        None => Named { text: short_id(id), full: format!("{kind} {id}"), known: false },
    }
}

impl Names {
    /// Remembers the computers of a devices list. A computer that later leaves the list keeps its name here, so older events
    /// and versions still name it.
    pub fn learn_computers(&mut self, devices: &[DeviceInfo]) {
        for device in devices {
            remember(&mut self.computers, device.id.to_string(), &device.name);
        }
    }

    /// Remembers an account's name (the signed-in one, or one of the admin's list).
    pub fn learn_account(&mut self, id: impl std::fmt::Display, name: &str) {
        remember(&mut self.accounts, id.to_string(), name);
    }

    /// The computer with this ID.
    pub fn computer(&self, id: impl std::fmt::Display) -> Named {
        find(&self.computers, &id.to_string(), "computer")
    }

    /// The account with this ID.
    pub fn account(&self, id: impl std::fmt::Display) -> Named {
        find(&self.accounts, &id.to_string(), "account")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_cloud_app::{DeviceId, Uuid};

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::generate(),
            name: name.into(),
            created_ms: 0,
            last_used_ms: 0,
            revoked: false,
            current: false,
            client_kind: String::new(),
            platform: String::new(),
            app_version: String::new(),
            revoked_ms: None,
            revoke_reason: None,
        }
    }

    #[test]
    fn a_known_computer_is_shown_by_name_with_its_id_on_hover() {
        let beast = device("BEAST");
        let mut names = Names::default();
        names.learn_computers(std::slice::from_ref(&beast));
        let named = names.computer(beast.id);
        assert_eq!(named.text, "BEAST");
        assert!(named.known && named.full.contains(&beast.id.to_string()), "{named:?}");
    }

    #[test]
    fn an_unknown_computer_keeps_a_short_id_and_the_whole_one_on_hover() {
        let id = DeviceId::generate();
        let named = Names::default().computer(id);
        assert!(!named.known);
        assert_eq!(named.text, short_id(id));
        assert!(named.text.len() < id.to_string().len() && named.full.contains(&id.to_string()), "{named:?}");
    }

    #[test]
    fn a_computer_that_left_the_list_keeps_its_name_and_a_rename_wins() {
        let mut old = device("old name");
        let mut names = Names::default();
        names.learn_computers(std::slice::from_ref(&old));
        names.learn_computers(&[]);
        assert_eq!(names.computer(old.id).text, "old name");
        old.name = "new name".into();
        names.learn_computers(std::slice::from_ref(&old));
        assert_eq!(names.computer(old.id).text, "new name");
    }

    #[test]
    fn accounts_are_named_and_an_empty_name_forgets_nothing() {
        let id = Uuid::from_u128(7);
        let mut names = Names::default();
        names.learn_account(id, "fabio");
        names.learn_account(id, "");
        assert_eq!(names.account(id).text, "fabio");
        assert!(!names.account(Uuid::from_u128(8)).known);
    }
}
