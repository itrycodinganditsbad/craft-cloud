//! A live document in a team space, against the fake agent: two accounts on one hub. Who is there ([`Session::people`]),
//! a viewer's session, the refusal that is never retried, and the blob store bound to the document's space.

mod common;

use std::time::{Duration, Instant};

use common::{App, Notes, change, open, until};
use craft_cloud_app::fake::Hub;
use craft_cloud_kit::{AgentBlobs, Badge, BlobStore, KitError, MemoryBlobs, PRESENCE_TTL, Session};

fn names(s: &Session<App>) -> Vec<String> {
    s.people().iter().map(|p| p.name.clone()).collect()
}

/// People come from the members the agent lists and the presence it stamps with the sender's account: yourself first,
/// then by name; a colour is the account's, the same in every window; a member who is removed or goes quiet leaves.
#[test]
fn people_join_leave_and_keep_their_colours() {
    let hub = Hub::new();
    hub.team();
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, "t.txt", Notes::new("t", &[]));
    let mut b = open::<App>(&ben_pc, "t.txt", Notes::default());

    // Alone, a session sees only itself, in its colour, as itself.
    until(&mut [&mut a, &mut b], "the members came", |s| s.iter().all(|x| !x.people().is_empty()));
    let alone = a.people();
    assert_eq!(alone.len(), 1);
    assert!(alone[0].is_self && alone[0].name == "ana" && alone[0].peers.is_empty());
    assert!(craft_cloud_api::PALETTE.contains(&alone[0].colour));

    // Ben's presence makes him one of the people, with his peer; the app's bytes come as they were sent.
    b.set_presence(Some(b"ben at 1".to_vec())).unwrap();
    until(&mut [&mut a, &mut b], "ana sees ben", |s| s.first().is_some_and(|a| names(a) == ["ana", "ben"]));
    let seen_by_a = a.people();
    assert!(seen_by_a[0].is_self && !seen_by_a[1].is_self);
    assert_eq!(seen_by_a[1].peers, vec![b.peer()]);
    assert!(a.peers().values().any(|p| p.bytes == b"ben at 1"));

    // The same colours in both windows.
    a.set_presence(Some(b"ana at 2".to_vec())).unwrap();
    until(&mut [&mut a, &mut b], "ben sees ana", |s| s.get(1).is_some_and(|b| names(b) == ["ben", "ana"]));
    let seen_by_b = b.people();
    assert!(seen_by_b[0].is_self && seen_by_b[0].name == "ben");
    assert_eq!(seen_by_b[0].colour, seen_by_a[1].colour, "ben's colour is ben's in both windows");
    assert_eq!(seen_by_b[1].colour, alone[0].colour, "ana's colour is ana's in both windows");
    assert_eq!(seen_by_b[1].peers, vec![a.peer()]);

    // Joining: a third account shows up, in name order after the first.
    let cara_pc = hub.machine_as("cara-pc", "cara").unwrap();
    let mut c = open::<App>(&cara_pc, "t.txt", Notes::default());
    c.set_presence(Some(b"cara at 3".to_vec())).unwrap();
    until(&mut [&mut a, &mut b, &mut c], "ana sees cara", |s| s.first().is_some_and(|a| names(a) == ["ana", "ben", "cara"]));

    // Yourself on another machine is you, with that window's peer.
    let ana_laptop = hub.machine_as("ana-laptop", "ana").unwrap();
    let mut a2 = open::<App>(&ana_laptop, "t.txt", Notes::default());
    a2.set_presence(Some(b"ana at 9".to_vec())).unwrap();
    until(&mut [&mut a, &mut b, &mut c, &mut a2], "ana sees her other window", |s| {
        s.first().is_some_and(|a| a.people().first().is_some_and(|me| me.is_self && me.peers == vec![s.get(3).map_or(0, |x| x.peer())]))
    });
    assert_eq!(names(&a), ["ana", "ben", "cara"], "one person for two windows");

    // Leaving: removed from the space, ben is gone from the list at once.
    hub.remove_member("ben");
    until(&mut [&mut a, &mut b, &mut c, &mut a2], "ben left", |s| s.first().is_some_and(|a| names(a) == ["ana", "cara"]));

    // Going quiet: without presence for the time to live, a person is no longer here.
    c.set_presence(None).unwrap();
    a2.set_presence(None).unwrap();
    let started = Instant::now();
    until(&mut [&mut a, &mut b, &mut c, &mut a2], "cara and her other window went quiet", |s| {
        s.first().is_some_and(|a| names(a) == ["ana"] && a.people().first().is_some_and(|me| me.peers.is_empty()))
    });
    assert!(started.elapsed() + Duration::from_secs(4) >= PRESENCE_TTL, "not before the presence expired");
    assert!(a.people().first().is_some_and(|me| me.is_self && me.peers.is_empty()));
}

/// A personal document has its owner alone in the list; a document opened by an app that never asks is not different.
#[test]
fn a_personal_document_lists_its_account_alone() {
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let laptop = hub.machine("laptop").unwrap();
    let mut a = open::<App>(&pc, "p.txt", Notes::new("p", &[]));
    let mut b = open::<App>(&laptop, "p.txt", Notes::default());
    b.set_presence(Some(b"cursor".to_vec())).unwrap();
    until(&mut [&mut a, &mut b], "the other machine is here", |s| s.first().is_some_and(|a| !a.peers().is_empty() && !a.people().is_empty()));
    let people = a.people();
    assert_eq!(people.len(), 1, "the account once, whatever the machines");
    assert!(people[0].is_self);
    assert_eq!(people[0].peers, vec![b.peer()]);
    assert_eq!(a.space(), None, "the fake names no space for a personal document");
}

/// A viewer's session: opened read-only by the agent, it refuses local changes and undo through the read-only path, sends
/// nothing, and shows the read-only badge; the editor's document is untouched.
#[test]
fn a_viewer_cannot_change_the_document() {
    let hub = Hub::new();
    hub.team();
    hub.set_read_only("ben", true);
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, "v.txt", Notes::new("v", &["one"]));
    let mut b = open::<App>(&ben_pc, "v.txt", Notes::default());
    assert!(!a.status().viewer && a.status().badge() != Badge::ReadOnly);
    assert!(b.status().viewer, "the agent said read_only at the open");
    assert_eq!(b.status().badge(), Badge::ReadOnly);
    assert_eq!(b.undo_kind(), craft_cloud_kit::UndoKind::None);
    assert!(b.status().read_only.is_none(), "it is a role, not a newer app's document");
    assert_eq!(b.model().texts(), vec!["one"]);

    let refused = change(&mut b, |m| m.with_item(1, "ben's"));
    assert_eq!(refused, Err(KitError::ViewOnly));
    assert_eq!(b.undo(), Err(KitError::ViewOnly));
    assert!(!b.can_undo() && !b.can_redo());
    assert!(matches!(b.save_point(), Err(KitError::NoRecord(_))));
    assert_eq!(b.model().texts(), vec!["one"], "the model stands");
    assert_eq!(b.poll().status.queued, 0, "nothing is kept to send");

    // He still gets the editor's changes, and shows he is there.
    change(&mut a, |m| m.with_item(1, "two")).unwrap();
    until(&mut [&mut a, &mut b], "the viewer sees the edit", |s| s.get(1).is_some_and(|b| b.model().texts() == ["one", "two"]));
    b.set_presence(Some(b"ben looks".to_vec())).unwrap();
    until(&mut [&mut a, &mut b], "ana sees the viewer", |s| s.first().is_some_and(|a| names(a) == ["ana", "ben"]));
}

/// An agent that refuses a change with `read_only` (the role was lowered while the document was open): the session
/// becomes a viewer, the change is not sent again, and later changes are refused locally.
#[test]
fn a_refusal_read_only_is_never_retried() {
    let hub = Hub::new();
    hub.team();
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, "r.txt", Notes::new("r", &[]));
    let mut b = open::<App>(&ben_pc, "r.txt", Notes::default());
    // The next update is refused as the agent does for a lowered role (a transient code would be sent again).
    ana_pc.refuse_next("read_only", 1);
    change(&mut a, |m| m.with_item(0, "refused")).unwrap();
    until(&mut [&mut a, &mut b], "the session knows it is a viewer", |s| s.first().is_some_and(|a| a.status().viewer));
    assert!(a.status().error.clone().unwrap_or_default().contains("no longer change"), "{:?}", a.status().error);
    assert_eq!(a.status().badge(), Badge::ReadOnly);
    // Long enough for any resend (the first is after 200 ms): nothing reaches the hub or the other account.
    let until_quiet = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until_quiet {
        a.poll();
        b.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(hub.updates("r.txt"), 0, "the refused change was not sent again");
    assert!(b.model().items.is_empty());
    assert!(!a.status().stopped, "a role is not a stop");
    assert_eq!(change(&mut a, |m| m.with_item(1, "after")), Err(KitError::ViewOnly));
    assert_eq!(a.model().texts(), vec!["refused"], "the model keeps what was made before the refusal");
}

/// `session.blobs()` is bound to the document's space: in a team space the other member gets the blob; the unbound
/// constructor keeps it in the personal space, where they don't.
#[test]
fn blobs_are_bound_to_the_documents_space() {
    let hub = Hub::new();
    hub.team();
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let a = open::<App>(&ana_pc, "b.txt", Notes::new("b", &[]));
    let b = open::<App>(&ben_pc, "b.txt", Notes::default());
    let (ana_blobs, ben_blobs) = (a.blobs().unwrap(), b.blobs().unwrap());
    assert_eq!(ana_blobs.space_bound(), Some(true));
    let shared = ana_blobs.put(b"a picture for the team").unwrap();
    assert_eq!(&*ben_blobs.get(&shared).unwrap(), b"a picture for the team", "every member gets it");

    // The old constructor keeps the blob in ana's personal space.
    let unbound = AgentBlobs(ana_pc.client().blobs());
    assert_eq!(unbound.space_bound(), Some(false));
    let private = unbound.put(b"in ana's own space").unwrap();
    let e = ben_blobs.get(&private).unwrap_err();
    assert!(e.to_string().contains("blob_not_found"), "{e}");
    assert_eq!(&*unbound.get(&private).unwrap(), b"in ana's own space");
}

/// A team document whose blob store is not bound to it gets a warning, once; the bound store, a store with no space and
/// a personal document get none.
#[test]
fn an_unbound_blob_store_in_a_team_space_is_warned_about() {
    let hub = Hub::new();
    hub.team();
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let _ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, "w.txt", Notes::new("w", &[]));
    assert!(a.space().is_some(), "the agent names the team space");
    assert!(a.check_blobs(&a.blobs().unwrap()), "a bound store is fine");
    assert!(a.check_blobs(&MemoryBlobs::default()), "a store with no space is not the agent's");
    assert!(a.warnings().is_empty());
    let unbound = AgentBlobs(ana_pc.client().blobs());
    assert!(!a.check_blobs(&unbound));
    assert!(!a.check_blobs(&unbound));
    assert_eq!(a.warnings().len(), 1, "once per session");
    assert!(a.warnings()[0].contains("Session::blobs()"), "{}", a.warnings()[0]);

    // A personal space: the plain constructor is right, and nothing is said.
    let plain = Hub::new();
    let pc = plain.machine("pc").unwrap();
    let mut p = open::<App>(&pc, "w.txt", Notes::new("w", &[]));
    assert!(p.check_blobs(&AgentBlobs(pc.client().blobs())));
    assert!(p.warnings().is_empty());
}
