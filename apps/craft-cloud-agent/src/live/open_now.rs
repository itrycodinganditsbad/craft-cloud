//! Who has a recent file open right now ([`RecentEntry::open_now`]): the presence the hub hears for the documents it follows
//! (an app on this computer has them open), kept for a short while per document and account, and put on the entries of a
//! list when the list is handed out. Nothing here asks the server for anything and nothing is written to disk: a document the
//! agent does not follow has no presence to hear, and a quiet account fades by itself.
//!
//! The role of the sender is not in the data (presence is the app's bytes, stamped with the account by the server), so this
//! says who is there, not whether they edit or only look.

use std::collections::HashMap;
use std::time::Duration;

use craft_cloud_proto::live::node_doc;
use craft_cloud_proto::local::{DocMember, RecentEntry, RecentPerson};
use craft_cloud_proto::{DocId, SpaceId};
use tokio::time::Instant;
use uuid::Uuid;

/// How long an account counts as there after its last presence (the kit sends again every three seconds and drops a peer
/// after ten).
pub const OPEN_NOW_TTL: Duration = Duration::from_secs(10);
/// Most documents kept at once; past that the table starts over (it is only a convenience).
const MAX_DOCS: usize = 4096;
/// Most accounts kept per document.
const MAX_PER_DOC: usize = 64;

/// The last presence heard per document and account.
#[derive(Debug, Default)]
pub struct Seen {
    docs: HashMap<DocId, HashMap<Uuid, Instant>>,
    /// When the earliest account may fade (an account heard again fades later, so this can be early, never late). None while
    /// nobody is counted.
    next: Option<Instant>,
}

impl Seen {
    /// Presence from `user` in `doc` at `now`. `true` when that account was not counted as there before (the lists change).
    pub fn note(&mut self, now: Instant, doc: DocId, user: Uuid) -> bool {
        // Only once the earliest deadline has passed is there anything to drop.
        if self.next.is_some_and(|deadline| now >= deadline) {
            self.sweep(now);
        }
        if self.docs.len() >= MAX_DOCS && !self.docs.contains_key(&doc) {
            self.docs.clear();
            self.next = None;
        }
        let users = self.docs.entry(doc).or_default();
        if users.len() >= MAX_PER_DOC && !users.contains_key(&user) {
            return false;
        }
        let new = users.insert(user, now).is_none();
        self.next.get_or_insert(now + OPEN_NOW_TTL);
        new
    }

    /// Drops the accounts that have faded by `now`, and finds when the next one will. `true` when anybody was dropped (the
    /// lists change).
    pub fn sweep(&mut self, now: Instant) -> bool {
        let before: usize = self.docs.values().map(HashMap::len).sum();
        self.docs.retain(|_, users| {
            users.retain(|_, at| now.saturating_duration_since(*at) < OPEN_NOW_TTL);
            !users.is_empty()
        });
        let after: usize = self.docs.values().map(HashMap::len).sum();
        self.next = self.docs.values().flat_map(HashMap::values).min().map(|at| *at + OPEN_NOW_TTL);
        after < before
    }

    /// When somebody may fade next: the hub looks at its lists again then (none while nobody is counted).
    pub fn next_expiry(&self) -> Option<Instant> {
        self.next
    }

    /// The accounts there in `doc` at `now`, apart from `me` and anybody `members` marks as the account itself, with the
    /// names and colours `members` gives. An account `members` does not know is left out (its name is not known). By name.
    pub fn people(&self, now: Instant, doc: DocId, me: Option<Uuid>, members: &[DocMember]) -> Vec<RecentPerson> {
        let Some(users) = self.docs.get(&doc) else { return Vec::new() };
        let mut people: Vec<RecentPerson> = users
            .iter()
            .filter(|(user, at)| Some(**user) != me && now.saturating_duration_since(**at) < OPEN_NOW_TTL)
            .filter_map(|(user, _)| members.iter().find(|m| m.user == *user && !m.is_self))
            .map(|m| RecentPerson { name: m.name.clone(), colour: m.colour })
            .collect();
        people.sort_by(|a, b| a.name.cmp(&b.name));
        people
    }

    /// `entries` of the space `space` with `open_now` filled in from what is heard (an entry without a node has no document).
    pub fn decorate(&self, now: Instant, space: SpaceId, me: Option<Uuid>, members: &[DocMember], mut entries: Vec<RecentEntry>) -> Vec<RecentEntry> {
        for entry in &mut entries {
            entry.open_now = match entry.node {
                Some(node) => self.people(now, node_doc(space, node), me, members),
                None => Vec::new(),
            };
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_cloud_proto::NodeId;

    fn member(n: u8, name: &str, is_self: bool) -> DocMember {
        DocMember { user: Uuid::from_bytes([n; 16]), name: name.into(), colour: n, is_self }
    }

    fn entry(node: Option<NodeId>) -> RecentEntry {
        RecentEntry { node, path: "a.txt".into(), title: "a".into(), opened_ms: 1, saved_ms: None, machine: "pc".into(), open_now: Vec::new() }
    }

    #[test]
    fn people_join_leave_and_never_include_the_account_itself() {
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        let doc = node_doc(space, node);
        let members = [member(1, "ana", true), member(2, "ben", false), member(3, "cy", false)];
        let me = Some(members[0].user);
        let t0 = Instant::now();
        let mut seen = Seen::default();
        assert!(seen.decorate(t0, space, me, &members, vec![entry(Some(node))])[0].open_now.is_empty(), "nobody yet");
        assert!(seen.note(t0, doc, members[1].user), "ben joins");
        assert!(!seen.note(t0, doc, members[1].user), "again is not a change");
        seen.note(t0, doc, members[0].user);
        let shown = seen.decorate(t0, space, me, &members, vec![entry(Some(node)), entry(None)]);
        assert_eq!(shown[0].open_now, vec![RecentPerson { name: "ben".into(), colour: 2 }], "ana herself is hidden");
        assert!(shown[1].open_now.is_empty(), "an entry without a node has no document");
        // Ben goes quiet and fades; cy is there later.
        let later = t0 + OPEN_NOW_TTL + Duration::from_secs(1);
        assert!(seen.people(later, doc, me, &members).is_empty(), "ben left");
        assert!(seen.note(later, doc, members[2].user));
        assert!(seen.note(later, doc, members[1].user), "ben is back, and counts as a change");
        let names: Vec<_> = seen.people(later, doc, me, &members).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["ben", "cy"]);
    }

    #[test]
    fn the_next_expiry_follows_the_oldest_account_and_a_sweep_drops_what_faded() {
        let (doc, ben, cy) = (DocId::generate(), Uuid::from_bytes([2; 16]), Uuid::from_bytes([3; 16]));
        let t0 = Instant::now();
        let mut seen = Seen::default();
        assert_eq!(seen.next_expiry(), None, "nobody, nothing to wait for");
        seen.note(t0, doc, ben);
        assert_eq!(seen.next_expiry(), Some(t0 + OPEN_NOW_TTL));
        let t5 = t0 + Duration::from_secs(5);
        seen.note(t5, doc, cy);
        assert!(!seen.sweep(t5), "nobody faded yet");
        assert_eq!(seen.next_expiry(), Some(t0 + OPEN_NOW_TTL), "ben is the oldest");
        assert!(seen.sweep(t0 + OPEN_NOW_TTL), "ben faded");
        assert_eq!(seen.next_expiry(), Some(t5 + OPEN_NOW_TTL), "cy is next");
        assert!(seen.sweep(t5 + OPEN_NOW_TTL));
        assert_eq!(seen.next_expiry(), None);
    }

    #[test]
    fn an_account_the_member_list_does_not_know_is_left_out_and_the_table_is_bounded() {
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        let doc = node_doc(space, node);
        let t0 = Instant::now();
        let mut seen = Seen::default();
        seen.note(t0, doc, Uuid::from_bytes([9; 16]));
        assert!(seen.people(t0, doc, None, &[member(1, "ana", false)]).is_empty());
        for n in 0..(MAX_PER_DOC as u32 + 10) {
            seen.note(t0, doc, Uuid::from_u128(u128::from(n) + 100));
        }
        assert!(seen.docs[&doc].len() <= MAX_PER_DOC);
        for _ in 0..(MAX_DOCS + 5) {
            seen.note(t0, DocId::generate(), Uuid::from_bytes([1; 16]));
        }
        assert!(seen.docs.len() <= MAX_DOCS);
    }

    #[test]
    fn debug_output_names_nobody() {
        let shown = format!("{:?}", RecentEntry { open_now: vec![RecentPerson { name: "ben".into(), colour: 2 }], ..entry(None) });
        assert!(!shown.contains("ben"), "{shown}");
    }
}
