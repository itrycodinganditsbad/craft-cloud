//! A live document in a team space, against the fake agent: two accounts on one hub. Who is there ([`Session::people`]),
//! a viewer's session, the refusal that is never retried, the role that is pushed, the changes kept through an app's end,
//! a space that stops syncing, and the blob store bound to the document's space.
//!
//! As in the agent, a team document is opened by an absolute path inside the team space's folder.

mod common;

use std::time::{Duration, Instant};

use common::{App, Notes, change, open, until};
use craft_cloud_app::fake::Hub;
use craft_cloud_kit::{AgentBlobs, Badge, BlobStore, KitError, MemoryBlobs, PRESENCE_TTL, Session};

fn names(s: &Session<App>) -> Vec<String> {
    s.people().iter().map(|p| p.name.clone()).collect()
}

/// A file of the team space's folder, as the agent knows it.
fn team_path(hub: &Hub, name: &str) -> String {
    format!("{}/{name}", hub.team_folder())
}

/// Polls the sessions for `span` (nothing is waited for: something that must not happen is looked for).
fn settle(sessions: &mut [&mut Session<App>], span: Duration) {
    let end = Instant::now() + span;
    while Instant::now() < end {
        for s in sessions.iter_mut() {
            s.poll();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// People come from the members the agent lists and the presence it stamps with the sender's account: yourself first,
/// then by name; a colour is the account's, the same in every window; a member who is removed or goes quiet leaves.
#[test]
fn people_join_leave_and_keep_their_colours() {
    let hub = Hub::new();
    hub.team();
    let path = team_path(&hub, "t.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, &path, Notes::new("t", &[]));
    let mut b = open::<App>(&ben_pc, &path, Notes::default());

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
    let mut c = open::<App>(&cara_pc, &path, Notes::default());
    c.set_presence(Some(b"cara at 3".to_vec())).unwrap();
    until(&mut [&mut a, &mut b, &mut c], "ana sees cara", |s| s.first().is_some_and(|a| names(a) == ["ana", "ben", "cara"]));

    // Yourself on another machine is you, with that window's peer.
    let ana_laptop = hub.machine_as("ana-laptop", "ana").unwrap();
    let mut a2 = open::<App>(&ana_laptop, &path, Notes::default());
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
}

/// A personal document has its owner alone in the list, in the personal space the agent names; the personal spaces of two
/// accounts are apart.
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
    assert!(a.space().is_some(), "the agent always names the space");

    // Another account's `p.txt` is its own document in its own space.
    let other = hub.machine_as("other-pc", "other").unwrap();
    let mut o = open::<App>(&other, "p.txt", Notes::new("o", &["not yours"]));
    assert_ne!(o.space(), a.space());
    settle(&mut [&mut a, &mut b, &mut o], Duration::from_millis(300));
    assert!(a.model().items.is_empty(), "the other account's document is not this one");
    until(&mut [&mut a, &mut b, &mut o], "the other account's members came", |s| s.get(2).is_some_and(|o| names(o) == ["other"]));
}

/// A viewer's session: opened read-only by the agent, it refuses local changes and undo through the read-only path, sends
/// nothing, and shows the read-only badge; the editor's document is untouched.
#[test]
fn a_viewer_cannot_change_the_document() {
    let hub = Hub::new();
    hub.team();
    hub.set_read_only("ben", true);
    let path = team_path(&hub, "v.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, &path, Notes::new("v", &["one"]));
    let mut b = open::<App>(&ben_pc, &path, Notes::default());
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
    let path = team_path(&hub, "r.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, &path, Notes::new("r", &[]));
    let mut b = open::<App>(&ben_pc, &path, Notes::default());
    // The next update is refused as the agent does for a lowered role (a transient code would be sent again).
    ana_pc.refuse_next("read_only", 1);
    change(&mut a, |m| m.with_item(0, "refused")).unwrap();
    until(&mut [&mut a, &mut b], "the session knows it is a viewer", |s| s.first().is_some_and(|a| a.status().viewer));
    assert!(a.status().error.clone().unwrap_or_default().contains("no longer change"), "{:?}", a.status().error);
    assert_eq!(a.status().badge(), Badge::ReadOnly);
    // Long enough for any resend (the first is after 200 ms): nothing reaches the hub or the other account.
    settle(&mut [&mut a, &mut b], Duration::from_millis(1500));
    assert_eq!(ana_pc.update_frames(), 1, "the refused change was sent once");
    assert_eq!(hub.updates("r.txt"), 0);
    assert!(b.model().items.is_empty());
    assert!(!a.status().stopped, "a role is not a stop");
    assert_eq!(change(&mut a, |m| m.with_item(1, "after")), Err(KitError::ViewOnly));
    assert_eq!(a.model().texts(), vec!["refused"], "the model keeps what was made before the refusal");
}

/// A role lowered while the document is open and raised again: the agent tells the document (`role`), and what was made
/// meanwhile, kept and not sent, goes up at once. The app's connection is never closed for it.
#[test]
fn changes_made_as_a_viewer_go_up_when_the_role_is_raised() {
    let hub = Hub::new();
    hub.team();
    let path = team_path(&hub, "k.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, &path, Notes::new("k", &[]));
    let mut b = open::<App>(&ben_pc, &path, Notes::default());

    // Lowered: the open document hears it before it has tried anything.
    hub.set_read_only("ana", true);
    until(&mut [&mut a, &mut b], "ana is told she is a viewer", |s| s.first().is_some_and(|a| a.status().viewer));
    assert_eq!(change(&mut a, |m| m.with_item(0, "kept")), Err(KitError::ViewOnly));

    // An edit that was already on its way when the role fell is refused and kept (the app had not heard yet).
    hub.set_read_only("ana", false);
    until(&mut [&mut a, &mut b], "ana may edit again", |s| s.first().is_some_and(|a| !a.status().viewer));
    ana_pc.refuse_next("read_only", 1);
    change(&mut a, |m| m.with_item(0, "kept")).unwrap();
    until(&mut [&mut a, &mut b], "the refusal makes her a viewer", |s| s.first().is_some_and(|a| a.status().viewer));
    hub.set_read_only("ana", true);
    assert_eq!(a.poll().status.queued, 1, "the change is kept, unsent");
    assert_eq!(hub.updates("k.txt"), 0);

    // Raised: told at once (no new open), and the kept change goes up and reaches ben.
    hub.set_read_only("ana", false);
    until(&mut [&mut a, &mut b], "the kept change reached ben", |s| {
        s.first().is_some_and(|a| !a.status().viewer && a.status().queued == 0) && s.get(1).is_some_and(|b| b.model().texts() == ["kept"])
    });
    assert!(change(&mut a, |m| m.with_item(1, "more")).is_ok(), "editing works again");
}

/// The kept changes of a viewer survive the app ending: the session hands them to the agent as it is dropped, the agent
/// keeps them (never sending them while the account may not write), and once the role is raised they go up, and a new open
/// of the document has them.
#[test]
fn changes_kept_as_a_viewer_survive_the_app_ending() {
    let hub = Hub::new();
    hub.team();
    let path = team_path(&hub, "q.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, &path, Notes::new("q", &[]));
    let mut b = open::<App>(&ben_pc, &path, Notes::default());
    ana_pc.refuse_next("read_only", 1);
    change(&mut a, |m| m.with_item(0, "kept")).unwrap();
    until(&mut [&mut a, &mut b], "ana is a viewer", |s| s.first().is_some_and(|a| a.status().viewer));
    hub.set_read_only("ana", true);

    // The app ends. The agent holds the change, and does not send it: she is still a viewer.
    drop(a);
    let deadline = Instant::now() + Duration::from_secs(10);
    while hub.held("q.txt") == 0 {
        assert!(Instant::now() < deadline, "the agent never got the change");
        std::thread::sleep(Duration::from_millis(10));
    }
    settle(&mut [&mut b], Duration::from_millis(300));
    assert_eq!(hub.updates("q.txt"), 0);
    assert!(b.model().items.is_empty());

    // The role is raised with no app open: the change goes up, and ben has it.
    hub.set_read_only("ana", false);
    until(&mut [&mut b], "the held change reached ben", |s| s.first().is_some_and(|b| b.model().texts() == ["kept"]));

    // Opened again, her document has it, and she can edit.
    let mut again = open::<App>(&ana_pc, &path, Notes::default());
    assert_eq!(again.model().texts(), vec!["kept"]);
    assert!(change(&mut again, |m| m.with_item(1, "more")).is_ok());
}

/// A team space that stops syncing: the document is told, nothing is sent into the dead hub, and the badge says so. The
/// kit does not retry a `not_found` either (the agent answers one when a change reaches a hub that stopped).
#[test]
fn a_document_of_a_space_that_stops_is_not_sent_into_a_dead_hub() {
    let hub = Hub::new();
    hub.team();
    let path = team_path(&hub, "s.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let mut a = open::<App>(&ana_pc, &path, Notes::new("s", &[]));
    hub.stop_team();
    until(&mut [&mut a], "the document is told", |s| s.first().is_some_and(|a| a.status().stopped));
    assert_eq!(a.status().badge(), Badge::Stopped);
    change(&mut a, |m| m.with_item(0, "typed")).unwrap();
    settle(&mut [&mut a], Duration::from_millis(1200));
    assert_eq!(ana_pc.update_frames(), 0, "nothing is sent to a hub that stopped");
    assert_eq!(a.poll().status.queued, 1, "the change stays in the window");
}

/// An update the agent answers with `not_found` (its hub was gone before the document heard): not sent again on that link. The
/// document is opened again after a pause, and the change goes up once, on the new link.
#[test]
fn a_refusal_not_found_opens_the_document_again_and_the_change_goes_up_once() {
    let hub = Hub::new();
    hub.team();
    let path = team_path(&hub, "n.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, &path, Notes::new("n", &[]));
    let mut b = open::<App>(&ben_pc, &path, Notes::default());
    ana_pc.refuse_next("not_found", 1);
    change(&mut a, |m| m.with_item(0, "lost?")).unwrap();
    until(&mut [&mut a, &mut b], "the session knows it is not followed", |s| s.first().is_some_and(|a| a.status().stopped));
    let error = a.status().error.clone().unwrap_or_default();
    assert!(error.contains("no longer follows"), "{error}");
    assert_eq!(a.status().badge(), Badge::Stopped);
    until(&mut [&mut a, &mut b], "the document is followed again and the change reaches ben", |s| {
        s.first().is_some_and(|a| !a.status().stopped && a.status().queued == 0) && s.get(1).is_some_and(|b| b.model().texts() == ["lost?"])
    });
    settle(&mut [&mut a, &mut b], Duration::from_millis(1200));
    assert_eq!(ana_pc.update_frames(), 2, "sent on the old link, and once more on the new one");
    assert_eq!(hub.updates("n.txt"), 1);
}

/// A team space that stops syncing and syncs again: the open document is told, nothing is sent while it is stopped, and once
/// the space is back the session opens the document again by itself, so the changes made meanwhile go up.
#[test]
fn a_space_that_stops_and_syncs_again_is_followed_again_and_the_queue_goes_up() {
    let hub = Hub::new();
    hub.team();
    let path = team_path(&hub, "again.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, &path, Notes::new("again", &[]));
    let mut b = open::<App>(&ben_pc, &path, Notes::default());
    hub.stop_team();
    until(&mut [&mut a, &mut b], "both are told", |s| s.iter().all(|x| x.status().stopped));
    assert!(a.status().error.clone().unwrap_or_default().contains("no longer follows"), "{:?}", a.status().error);
    change(&mut a, |m| m.with_item(0, "typed while stopped")).unwrap();
    settle(&mut [&mut a, &mut b], Duration::from_millis(700));
    assert_eq!(ana_pc.update_frames(), 0, "nothing is sent while it is stopped");
    assert_eq!(a.poll().status.queued, 1);
    assert_eq!(a.status().badge(), Badge::Stopped);

    hub.start_team();
    until(&mut [&mut a, &mut b], "the queue went up and reached ben", |s| {
        s.iter().all(|x| !x.status().stopped)
            && s.first().is_some_and(|a| a.status().queued == 0)
            && s.get(1).is_some_and(|b| b.model().texts() == ["typed while stopped"])
    });
    assert!(change(&mut a, |m| m.with_item(1, "and after")).is_ok(), "editing goes on");
}

/// `reconnect_now` after a stop opens the document at once, without waiting out the pause.
#[test]
fn reconnect_now_after_a_stop_opens_the_document_at_once() {
    let hub = Hub::new();
    hub.team();
    let path = team_path(&hub, "now.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let mut a = open::<App>(&ana_pc, &path, Notes::new("now", &[]));
    hub.stop_team();
    until(&mut [&mut a], "it is told", |s| s.first().is_some_and(|a| a.status().stopped));
    change(&mut a, |m| m.with_item(0, "waiting")).unwrap();
    // Let the pauses between tries grow.
    settle(&mut [&mut a], Duration::from_millis(3500));
    hub.start_team();
    a.reconnect_now();
    let started = Instant::now();
    until(&mut [&mut a], "it is followed again", |s| s.first().is_some_and(|a| !a.status().stopped && a.status().queued == 0));
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
}

/// `session.blobs()` is bound to the document's space: in a team space the other member gets the blob; the unbound
/// constructor keeps it in the personal space, where they don't. A viewer's put is `ViewOnly`.
#[test]
fn blobs_are_bound_to_the_documents_space() {
    let hub = Hub::new();
    hub.team();
    let path = team_path(&hub, "b.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let a = open::<App>(&ana_pc, &path, Notes::new("b", &[]));
    let b = open::<App>(&ben_pc, &path, Notes::default());
    let (ana_blobs, ben_blobs) = (a.blobs().unwrap(), b.blobs().unwrap());
    assert_eq!(ana_blobs.space_bound(), Some(true));
    assert_eq!(ana_blobs.bound_path(), Some(path.as_str()));
    let shared = ana_blobs.put(b"a picture for the team").unwrap();
    assert_eq!(&*ben_blobs.get(&shared).unwrap(), b"a picture for the team", "every member gets it");

    // The old constructor keeps the blob in ana's personal space.
    let unbound = AgentBlobs(ana_pc.client().blobs());
    assert_eq!(unbound.space_bound(), Some(false));
    let private = unbound.put(b"in ana's own space").unwrap();
    let e = ben_blobs.get(&private).unwrap_err();
    assert!(e.to_string().contains("blob_not_found"), "{e}");
    assert_eq!(&*unbound.get(&private).unwrap(), b"in ana's own space");

    // A relative path is the personal space's, as the agent routes it; a path that climbs out of the team folder is refused.
    let relative = AgentBlobs(ana_pc.client().blobs().for_path("b.txt"));
    let rel = relative.put(b"personal by its path").unwrap();
    assert!(ben_blobs.get(&rel).is_err());
    let climbing = AgentBlobs(ana_pc.client().blobs().for_path(&format!("{}/../elsewhere/b.txt", hub.team_folder())));
    let e = climbing.put(b"nowhere").unwrap_err();
    assert!(e.to_string().contains("invalid"), "{e}");

    // An absolute path in the personal folder is the personal space's, as for the agent.
    let in_personal = AgentBlobs(ana_pc.client().blobs().for_path(&format!("{}/b.txt", hub.personal_folder())));
    let mine = in_personal.put(b"mine, by an absolute path").unwrap();
    assert_eq!(&*unbound.get(&mine).unwrap(), b"mine, by an absolute path");
    assert!(ben_blobs.get(&mine).is_err());

    // A viewer's put into the team space is the same refusal as a change to the document.
    hub.set_read_only("ben", true);
    assert_eq!(ben_blobs.put(b"a viewer's picture"), Err(KitError::ViewOnly));
    assert_eq!(&*ben_blobs.get(&shared).unwrap(), b"a picture for the team", "he still reads");
}

/// A team document whose blob store is not bound to it (or is bound to another document) gets a warning, once; the store
/// bound to it, a store with no space and a personal document get none.
#[test]
fn an_unbound_blob_store_in_a_team_space_is_warned_about() {
    let hub = Hub::new();
    hub.team();
    let path = team_path(&hub, "w.txt");
    let ana_pc = hub.machine_as("ana-pc", "ana").unwrap();
    let _ben_pc = hub.machine_as("ben-pc", "ben").unwrap();
    let mut a = open::<App>(&ana_pc, &path, Notes::new("w", &[]));
    assert!(a.space().is_some(), "the agent names the team space");
    assert!(a.check_blobs(&a.blobs().unwrap()), "a bound store is fine");
    assert!(a.check_blobs(&MemoryBlobs::default()), "a store with no space is not the agent's");
    assert!(a.warnings().is_empty());
    let unbound = AgentBlobs(ana_pc.client().blobs());
    assert!(!a.check_blobs(&unbound));
    assert!(!a.check_blobs(&unbound));
    assert_eq!(a.warnings().len(), 1, "once per session");
    assert!(a.warnings()[0].contains("Session::blobs()"), "{}", a.warnings()[0]);

    // A store built for another document is not this document's.
    let other = AgentBlobs(ana_pc.client().blobs().for_path(&team_path(&hub, "another.txt")));
    assert!(!a.check_blobs(&other));
    assert_eq!(a.warnings().len(), 2, "a second reason is a second warning");
    assert!(a.warnings()[1].contains("another document"), "{}", a.warnings()[1]);

    // The same file under another spelling is this document's store.
    let spelled = AgentBlobs(ana_pc.client().blobs().for_path(&format!("{}/./sub/../w.txt", hub.team_folder())));
    assert!(a.check_blobs(&spelled));
    assert_eq!(a.warnings().len(), 2, "no new warning");

    // A personal space: the plain constructor is right, and nothing is said.
    let plain = Hub::new();
    let pc = plain.machine("pc").unwrap();
    let mut p = open::<App>(&pc, "w.txt", Notes::new("w", &[]));
    assert!(p.check_blobs(&AgentBlobs(pc.client().blobs())));
    // (A store made for another file of the account's personal space is not a problem there.)
    assert!(p.check_blobs(&AgentBlobs(pc.client().blobs().for_path("another.txt"))));
    assert!(p.warnings().is_empty());
}
