//! Documents of files through the agent's replica and the server's own live sessions: joining by import (one import
//! kept when two machines open a never-live file at once), edits going both ways and offline, compaction and catch-up
//! from a snapshot (one the agent can't read too), presence, the cap on followed documents, restores and resets with
//! snapshots in play, and an older server.

#[path = "docs_common/mod.rs"]
mod common;

use common::{App, World};
use craft_cloud_agent::live::replica::{DocEvent, Opening};
use craft_cloud_proto::live::{ClientFrame, MAX_SUBSCRIPTIONS, ServerFrame};
use craft_cloud_proto::local::NotLiveReason;
use craft_cloud_server::state::Limits;

fn small_logs() -> Limits {
    Limits { compact_updates: 5, ..Limits::default() }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// Opens `node` on `m` and settles; answers an import with `content` if asked. Returns the document.
async fn joined(w: &mut World, m: usize, node: craft_cloud_proto::NodeId, content: &str) -> craft_cloud_proto::DocId {
    let (doc, opening) = w.open(m, node);
    assert_eq!(opening, Opening::Joining);
    w.settle().await;
    let events = w.events(m);
    if events.contains(&DocEvent::NeedsImport(doc)) {
        w.import(m, doc, content);
        w.settle().await;
        assert!(w.events(m).contains(&DocEvent::Joined(doc)), "kept: live");
    } else {
        assert!(events.contains(&DocEvent::Joined(doc)), "{events:?}");
    }
    doc
}

#[test]
fn a_file_opened_first_is_imported_and_the_next_machine_joins_it() {
    rt().block_on(async {
        let mut w = World::new(Limits::default()).await;
        let (a, b) = (w.machine().await, w.machine().await);
        w.connect(a);
        w.connect(b);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "hello").await;
        assert_eq!(w.text(a, doc), "hello");
        // The next machine joins it: it isn't asked for an import, and sees the first one's.
        let (doc_b, opening) = w.open(b, node);
        assert_eq!((doc_b, opening), (doc, Opening::Joining));
        w.settle().await;
        let events = w.events(b);
        assert!(events.contains(&DocEvent::Joined(doc)) && !events.contains(&DocEvent::NeedsImport(doc)), "{events:?}");
        assert_eq!(w.text(b, doc), "hello");
        // Edits go both ways, as the apps' own changes.
        let app_a = w.app_of(a, doc, 101);
        let app_b = w.app_of(b, doc, 102);
        let u = app_a.append(" world");
        w.edit(a, doc, &u);
        w.settle().await;
        let remote: Vec<Vec<u8>> =
            w.events(b).into_iter().filter_map(|e| if let DocEvent::Remote(d, bytes) = e { (d == doc).then_some(bytes) } else { None }).collect();
        assert!(!remote.is_empty(), "the change reached the other machine's apps");
        for bytes in &remote {
            app_b.apply(bytes);
        }
        assert_eq!(app_b.text(), "hello world");
        let v = app_b.type_at(0, ">> ");
        w.edit(b, doc, &v);
        w.settle().await;
        assert_eq!(w.text(a, doc), ">> hello world");
        assert_eq!(w.text(b, doc), ">> hello world");
        // A second open on a machine where it is live: from its copy, at once.
        assert_eq!(w.open(a, node).1, Opening::Live);
    });
}

/// Review: two machines open a never-live file at the same moment. One import wins, both end on the same document, and
/// the loser's app never saw its own import as live (it hears `joined` only once the loser has caught up with the
/// winner's).
#[test]
fn two_machines_importing_at_once_end_on_one_document() {
    rt().block_on(async {
        let mut w = World::new(Limits::default()).await;
        let (a, b) = (w.machine().await, w.machine().await);
        w.connect(a);
        w.connect(b);
        w.settle().await;
        let node = w.file().await;
        let (doc, _) = w.open(a, node);
        w.open(b, node);
        w.settle().await;
        assert_eq!(w.events(a), vec![DocEvent::NeedsImport(doc)]);
        assert_eq!(w.events(b), vec![DocEvent::NeedsImport(doc)]);
        // Both imports in flight at once.
        w.import(a, doc, "from a");
        w.import(b, doc, "from b");
        assert!(!w.machines[a].replica().has_document(doc) && !w.machines[b].replica().has_document(doc), "not live before the server says");
        w.settle().await;
        let (ea, eb) = (w.events(a), w.events(b));
        assert!(ea.contains(&DocEvent::Joined(doc)) && eb.contains(&DocEvent::Joined(doc)), "{ea:?} {eb:?}");
        let (ta, tb) = (w.text(a, doc), w.text(b, doc));
        assert_eq!(ta, tb, "one document");
        assert!(ta == "from a" || ta == "from b", "{ta}");
        // The loser's text is nowhere: its import was dropped, not merged.
        assert!(!(ta.contains("from a") && ta.contains("from b")));
        // And a third machine sees the same.
        let c = w.machine().await;
        w.connect(c);
        w.settle().await;
        joined(&mut w, c, node, "never asked").await;
        assert_eq!(w.text(c, doc), ta);
    });
}

#[test]
fn edits_made_offline_on_both_sides_merge_on_reconnect() {
    rt().block_on(async {
        let mut w = World::new(Limits::default()).await;
        let (a, b) = (w.machine().await, w.machine().await);
        w.connect(a);
        w.connect(b);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "base").await;
        joined(&mut w, b, node, "").await;
        w.disconnect(a);
        w.disconnect(b);
        // Offline, both open it again from their copies and edit.
        assert_eq!(w.open(a, node).1, Opening::Live);
        assert_eq!(w.open(b, node).1, Opening::Live);
        let (app_a, app_b) = (w.app_of(a, doc, 201), w.app_of(b, doc, 202));
        for i in 0..5 {
            let u = app_a.append(&format!(" a{i}"));
            w.edit(a, doc, &u);
            let v = app_b.type_at(0, &format!("b{i} "));
            w.edit(b, doc, &v);
        }
        let (online, waiting) = w.machines[a].replica().document_status(doc).unwrap();
        assert_eq!((online, waiting), (false, 5), "offline, five changes waiting");
        // A crash before the document's file is written: the changes are in its journal.
        w.crash(a);
        w.connect(a);
        w.connect(b);
        w.settle().await;
        let (ta, tb) = (w.text(a, doc), w.text(b, doc));
        assert_eq!(ta, tb);
        for i in 0..5 {
            assert!(ta.contains(&format!(" a{i}")) && ta.contains(&format!("b{i} ")), "{ta}");
        }
        assert_eq!(w.machines[a].replica().document_status(doc).unwrap(), (true, 0));
    });
}

/// A file that has never been live opens as a plain file offline; one that has been opens from the copy.
#[test]
fn offline_a_never_live_file_is_a_plain_file() {
    rt().block_on(async {
        let mut w = World::new(Limits::default()).await;
        let a = w.machine().await;
        let node = w.file().await;
        let (_, opening) = w.open(a, node);
        assert!(matches!(opening, Opening::NotLive(NotLiveReason::Offline, _)), "{opening:?}");
        w.connect(a);
        w.settle().await;
        let doc = joined(&mut w, a, node, "x").await;
        w.disconnect(a);
        assert_eq!(w.open(a, node).1, Opening::Live);
        w.stop(a);
        assert_eq!(w.open(a, node).1, Opening::Live, "after a restart too");
        assert_eq!(w.text(a, doc), "x");
        // Joining when the link goes: the waiting app is told it is a plain file for now.
        let other = w.file().await;
        w.connect(a);
        w.settle().await;
        let (doc2, _) = w.open(a, other);
        w.disconnect(a);
        assert!(w.events(a).iter().any(|e| matches!(e, DocEvent::NotLive(d, NotLiveReason::Offline, _) if *d == doc2)));
        assert!(!w.machines[a].replica().has_document(doc2), "never live: nothing kept");
        assert!(!w.machines[a].store.documents().contains_key(&doc2.to_string()), "and nothing on disk");
    });
}

/// Ruling: an update the agent refuses leaves the document as it was; a change it can't write to disk is taken back.
#[test]
fn a_refused_or_unwritten_change_leaves_the_document_as_it_was() {
    rt().block_on(async {
        let mut w = World::new(Limits::default()).await;
        let a = w.machine().await;
        w.connect(a);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "keep").await;
        let before = w.machines[a].replica().document_state(doc).unwrap();
        for junk in [vec![1, 2, 3, 4, 5, 6, 7, 8], vec![0; 100], Vec::new()] {
            assert!(w.machines[a].replica().local_update(doc, &junk).is_err());
        }
        assert_eq!(w.machines[a].replica().document_state(doc).unwrap(), before);
        assert!(w.machines[a].replica().local_update(doc, &vec![0; craft_cloud_proto::live::MAX_UPDATE_BYTES + 1]).is_err());
        let app = w.app_of(a, doc, 301);
        let u = app.append("!");
        w.machines[a].store.fail_journal(true);
        assert!(matches!(w.machines[a].replica().local_update(doc, &u), Err(craft_cloud_agent::live::replica::LocalError::Io(_))));
        assert_eq!(w.text(a, doc), "keep", "taken back");
        w.machines[a].store.fail_journal(false);
        w.edit(a, doc, &u);
        w.settle().await;
        assert_eq!(w.text(a, doc), "keep!");
    });
}

/// A change in fragments that crosses the document's cap on the server is stored whole, never half; the next one, a
/// big paste already in the sender's copy when the compaction is asked, is refused as full and kept. The sender, the
/// only device, far past the version asked, answers once it is refused (it would never be asked again: nothing more
/// is stored), the log gets below its cap, the paste goes, and a machine joining at the end has both, whole.
#[test]
fn a_change_over_the_cap_is_stored_whole_and_the_document_gets_below_it_again() {
    rt().block_on(async {
        // (Fragments of 2 KiB: the change of 12 KB goes in six, as a change over 1 MiB does.)
        craft_cloud_agent::live::replica::set_frame_limit_for_tests(2048);
        let meta = craft_cloud_meta::Meta::open("sqlite::memory:").await.unwrap().with_live_doc_cap(8_000);
        let limits = Limits { compact_updates: 3, ..Limits::default() };
        let mut w = World::with_meta(limits, meta).await;
        let (a, b) = (w.machine().await, w.machine().await);
        w.connect(a);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "start").await;
        let app_a = w.app_of(a, doc, 501);
        let big = format!("[{}]", "b".repeat(12_000));
        let paste = format!("({})", "p".repeat(70_000));
        w.edit(a, doc, &app_a.append(&big));
        w.edit(a, doc, &app_a.append(&paste));
        w.settle().await;
        assert!(w.machines[a].said.iter().any(|s| s.contains("full on the server")), "refused as full: {:?}", w.machines[a].said);
        for _ in 0..40 {
            w.settle().await;
        }
        assert_eq!(w.machines[a].replica().document_status(doc).map(|s| s.1), Some(0), "nothing waits: {:?}", w.machines[a].said);
        let found = w.meta.live_doc(w.space, doc).await.unwrap().unwrap();
        assert!(found.base > 0, "compacted: {found:?}");
        let rows = w.meta.live_updates(w.space, doc, found.base, 1000).await.unwrap();
        let fragments = rows.iter().filter(|r| craft_cloud_proto::fragments::is_fragment(&r.bytes)).count();
        assert!(fragments == 0 || fragments >= 35, "whole sets in the log: {fragments} fragment rows");
        w.connect(b);
        w.settle().await;
        joined(&mut w, b, node, "").await;
        assert_eq!(w.text(b, doc), format!("start{big}{paste}"), "the other machine has both, whole");
    });
}

/// A long log is compacted by an agent, at the server's asking; a machine opening the document later starts from
/// the snapshot and the log above it, and ends on the same document.
#[test]
fn a_long_log_is_compacted_and_a_new_machine_starts_from_the_snapshot() {
    rt().block_on(async {
        let mut w = World::new(small_logs()).await;
        let (a, b) = (w.machine().await, w.machine().await);
        w.connect(a);
        w.connect(b);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "start").await;
        joined(&mut w, b, node, "").await;
        let (app_a, app_b) = (w.app_of(a, doc, 401), w.app_of(b, doc, 402));
        for i in 0..30 {
            let u = app_a.append(&format!(" {i}"));
            w.edit(a, doc, &u);
            if i % 3 == 0 {
                let v = app_b.set(&format!("k{i}"), i);
                w.edit(b, doc, &v);
            }
            w.settle().await;
        }
        let found = w.meta.live_doc(w.space, doc).await.unwrap().unwrap();
        assert!(found.base > 0, "compacted: {found:?}");
        assert!(found.head - found.base <= 6, "and kept short: {found:?}");
        let c = w.machine().await;
        w.connect(c);
        w.settle().await;
        joined(&mut w, c, node, "never asked").await;
        assert!(w.machines[c].heard.iter().any(|f| matches!(f, ServerFrame::Snapshot { base, .. } if *base > 0)), "caught up from a snapshot");
        assert_eq!(w.text(c, doc), w.text(a, doc));
        assert_eq!(w.app_of(c, doc, 9).doc.get_deep_value(), w.app_of(a, doc, 9).doc.get_deep_value());
        assert_eq!(w.app_of(b, doc, 9).doc.get_deep_value(), w.app_of(a, doc, 9).doc.get_deep_value());
        // Its next `at` names the update at the snapshot's base (sent after it): a subscribe from there is checked.
        let frames: Vec<ClientFrame> = {
            let r = w.machines[c].replica();
            r.disconnected();
            r.connected();
            let welcome = ServerFrame::Welcome { protocol: 2, epoch: w.meta.live_epoch(w.space).await.unwrap() };
            r.on_frame(welcome).unwrap()
        };
        assert!(frames.iter().all(|f| !matches!(f, ClientFrame::Subscribe { at: None, after, .. } if *after > 0)), "{frames:?}");
    });
}

/// Review: a snapshot that doesn't read on another machine (a device answered `compact` with junk). The machine that
/// can't read it says so and reads the import and the whole log instead; nothing is lost and it converges.
#[test]
fn a_snapshot_another_machine_cant_read_is_skipped_for_the_whole_log() {
    rt().block_on(async {
        let mut w = World::new(Limits { compact_updates: 3, ..Limits::default() }).await;
        let a = w.machine().await;
        w.connect(a);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "import").await;
        let app = w.app_of(a, doc, 501);
        // A rogue device of the account: it makes the log long and answers the compaction with junk.
        let token = [99u8; 32];
        w.meta.create_device(w.user, "rogue", &token, 0).await.unwrap();
        let auth = w.meta.device_by_token(&token).await.unwrap().unwrap();
        let mut rogue = craft_cloud_server::live::Session::new(w.state.clone(), &auth);
        rogue.handle(ClientFrame::Hello { min: 1, max: 2 }).await;
        rogue.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        while rogue.has_work() {
            rogue.pump().await.unwrap();
        }
        let mut asked = None;
        for i in 1..=5u64 {
            let u = app.append(&format!(" r{i}"));
            let reply = rogue.handle(ClientFrame::Update { doc, client_seq: i, bytes: u }).await;
            asked = asked.or(reply.frames.iter().find_map(|f| if let ServerFrame::Compact { upto, .. } = f { Some(*upto) } else { None }));
        }
        let upto = asked.expect("the rogue was asked");
        let junk = w.upload(doc, &[0xde, 0xad, 0xbe, 0xef]).await;
        rogue.handle(ClientFrame::Snapshot { doc, upto, manifest: junk }).await;
        assert_eq!(w.meta.live_doc(w.space, doc).await.unwrap().unwrap().base, upto, "the junk is the snapshot now");
        w.settle().await;
        // A fresh machine: the snapshot doesn't read; it reads the import and every row.
        let b = w.machine().await;
        w.connect(b);
        w.settle().await;
        joined(&mut w, b, node, "never asked").await;
        assert!(w.machines[b].said.iter().any(|s| s.contains("doesn't read here")), "{:?}", w.machines[b].said);
        assert!(w.machines[b].heard.iter().any(|f| matches!(f, ServerFrame::Snapshot { base: 0, .. })), "the import came");
        assert_eq!(w.text(b, doc), w.text(a, doc));
        assert!(w.text(b, doc).contains(" r5"));
        // And it goes on taking edits.
        let next = w.app_of(a, doc, 502).append(" more");
        w.edit(a, doc, &next);
        w.settle().await;
        assert_eq!(w.text(b, doc), w.text(a, doc));
    });
}

/// Review: a snapshot that reads but lacks some of what the rows below its base held (a device made it wrong): a machine
/// catching up from it finds updates waiting for that history, and reads the import and the whole log instead.
#[test]
fn a_snapshot_that_lacks_updates_is_caught_by_the_whole_log() {
    rt().block_on(async {
        let mut w = World::new(Limits { compact_updates: 3, ..Limits::default() }).await;
        let a = w.machine().await;
        w.connect(a);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "import").await;
        let app = w.app_of(a, doc, 1501);
        let token = [98u8; 32];
        w.meta.create_device(w.user, "rogue", &token, 0).await.unwrap();
        let auth = w.meta.device_by_token(&token).await.unwrap().unwrap();
        let mut rogue = craft_cloud_server::live::Session::new(w.state.clone(), &auth);
        rogue.handle(ClientFrame::Hello { min: 1, max: 2 }).await;
        rogue.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        while rogue.has_work() {
            rogue.pump().await.unwrap();
        }
        let mut asked = None;
        for i in 1..=5u64 {
            let u = app.append(&format!(" r{i}"));
            let reply = rogue.handle(ClientFrame::Update { doc, client_seq: i, bytes: u }).await;
            asked = asked.or(reply.frames.iter().find_map(|f| if let ServerFrame::Compact { upto, .. } = f { Some(*upto) } else { None }));
        }
        let upto = asked.expect("the rogue was asked");
        // A snapshot of the import only: it reads, but holds none of the updates up to `upto`.
        let wrong = w.upload(doc, &App::new(1).with_text("import").snapshot()).await;
        rogue.handle(ClientFrame::Snapshot { doc, upto, manifest: wrong }).await;
        let more = app.append(" after");
        rogue.handle(ClientFrame::Update { doc, client_seq: 6, bytes: more }).await;
        let b = w.machine().await;
        w.connect(b);
        w.settle().await;
        joined(&mut w, b, node, "never asked").await;
        assert!(w.machines[b].said.iter().any(|s| s.contains("reads the whole log")), "{:?}", w.machines[b].said);
        assert_eq!(w.text(b, doc), "import r1 r2 r3 r4 r5 after");
    });
}

/// A compaction answered by a device that is behind: it answers only once it holds every update up to `upto`.
#[test]
fn a_device_behind_answers_compaction_only_once_it_holds_the_updates() {
    rt().block_on(async {
        let mut w = World::new(Limits { compact_updates: 3, ..Limits::default() }).await;
        let (a, b) = (w.machine().await, w.machine().await);
        w.connect(a);
        w.connect(b);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "x").await;
        joined(&mut w, b, node, "").await;
        let app = w.app_of(a, doc, 601);
        for i in 0..3 {
            let u = app.append(&i.to_string());
            w.edit(a, doc, &u);
        }
        w.settle().await;
        // B is asked for 4 while it holds 3 (the server's frame comes before the update it names).
        let r = w.machines[b].replica();
        assert!(r.on_frame(ServerFrame::Compact { doc, upto: 4 }).unwrap().is_empty(), "not yet");
        let u = app.append("3");
        w.edit(a, doc, &u);
        w.settle().await;
        let answer = w.machines[b].said_up.iter().find_map(|f| match f {
            ClientFrame::Snapshot { upto, manifest, .. } => Some((*upto, *manifest)),
            _ => None,
        });
        let (upto, manifest) = answer.expect("answered once the update came");
        assert_eq!(upto, 4);
        let bytes = w.download(&manifest).await;
        // A snapshot of exactly the updates 1-4: with the log above it, a fresh copy is the document.
        let fresh = App::new(9);
        fresh.apply(&bytes);
        assert_eq!(fresh.text(), "x0123");
        let later = app.append("4");
        fresh.apply(&later);
        assert_eq!(fresh.text(), "x01234");
    });
}

/// A snapshot made while this machine has a change of its own not yet in the log holds none of it (exactly the
/// updates up to `upto`).
#[test]
fn a_compaction_snapshot_holds_nothing_past_upto() {
    rt().block_on(async {
        let mut w = World::new(Limits::default()).await;
        let a = w.machine().await;
        w.connect(a);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "base").await;
        let app = w.app_of(a, doc, 901);
        let u = app.append(" acked");
        w.edit(a, doc, &u);
        w.settle().await;
        // Offline now: a change of its own, unsent.
        w.disconnect(a);
        let v = app.append(" unsent");
        w.edit(a, doc, &v);
        w.connect(a);
        // Asked to compact at 1 (the acked change) before its own goes up.
        let r = w.machines[a].replica();
        let mut frames = Vec::new();
        frames.extend(r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: w.meta.live_epoch(w.space).await.unwrap() }).unwrap());
        frames.extend(r.on_frame(ServerFrame::Update { doc, seq: 1, bytes: u.clone(), device: craft_cloud_proto::DeviceId::generate() }).unwrap());
        frames.extend(r.on_frame(ServerFrame::Subscribed { doc, head: 1 }).unwrap());
        frames.extend(r.on_frame(ServerFrame::Compact { doc, upto: 1 }).unwrap());
        let manifest = frames
            .iter()
            .find_map(|f| if let ClientFrame::Snapshot { upto: 1, manifest, .. } = f { Some(*manifest) } else { None })
            .expect("answered");
        let bytes = r.take_uploads().into_iter().find(|u| u.id() == manifest).expect("uploaded before it is named").bytes();
        let fresh = App::new(9);
        fresh.apply(&bytes);
        assert_eq!(fresh.text(), "base acked", "nothing past upto");
    });
}

#[test]
fn presence_goes_between_machines_and_is_never_stored() {
    rt().block_on(async {
        let mut w = World::new(Limits::default()).await;
        let (a, b) = (w.machine().await, w.machine().await);
        w.connect(a);
        w.connect(b);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "x").await;
        joined(&mut w, b, node, "").await;
        let frames = w.machines[a].replica().presence(doc, b"cursor 3");
        w.send(a, frames);
        w.settle().await;
        assert!(w.events(b).contains(&DocEvent::Presence(doc, b"cursor 3".to_vec())));
        assert!(!w.events(a).iter().any(|e| matches!(e, DocEvent::Presence(..))), "not back to its sender");
        assert_eq!(w.meta.live_doc(w.space, doc).await.unwrap().unwrap().head, 0);
    });
}

/// Past the device's cap on followed documents, the least recently used one no app has open is unfollowed; it stays
/// on disk and is followed again when opened.
#[test]
fn past_the_cap_the_least_recently_used_document_is_unfollowed() {
    rt().block_on(async {
        let mut w = World::new(Limits { live_burst: 100_000, ..Limits::default() }).await;
        let a = w.machine().await;
        w.connect(a);
        w.settle().await;
        let mut docs = Vec::new();
        let mut nodes = Vec::new();
        for i in 0..=MAX_SUBSCRIPTIONS {
            let node = w.file().await;
            let doc = joined(&mut w, a, node, &format!("d{i}")).await;
            // Closed at once: no app has it open.
            w.machines[a].replica().set_open(doc, false);
            docs.push(doc);
            nodes.push(node);
        }
        // (Each new file is first not found: that is how it is joined.)
        let refused = w.machines[a].heard.iter().filter(|f| matches!(f, ServerFrame::Error { code, .. } if code != "not_found")).count();
        assert_eq!(refused, 0, "no subscribe was refused for the device's cap");
        // The first one went: reopening it follows it again, from its copy.
        assert_eq!(w.open(a, nodes[0]).1, Opening::Live);
        w.settle().await;
        assert_eq!(w.text(a, docs[0]), "d0");
        let (online, _) = w.machines[a].replica().document_status(docs[0]).unwrap();
        assert!(online, "followed again");
        // (Unloaded too: past the cap on documents in memory as well.)
        let followed = w.machines[a].replica().document_status(docs[1]).is_some_and(|(online, _)| online);
        assert!(!followed, "the next least recent went instead");
        assert!(w.machines[a].replica().loaded_documents() <= craft_cloud_agent::live::replica::MAX_LOADED_DOCUMENTS);
    });
}

/// Review: the server reset, or restored behind or ahead of a machine, with snapshots in play: the agent catches up
/// from the snapshot or pushes what the log lacks, and nothing any machine holds is lost.
#[test]
fn resets_and_restores_with_a_snapshot_lose_nothing() {
    rt().block_on(async {
        let mut w = World::new(small_logs()).await;
        let (a, b) = (w.machine().await, w.machine().await);
        w.connect(a);
        w.connect(b);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "v").await;
        joined(&mut w, b, node, "").await;
        let (app_a, app_b) = (w.app_of(a, doc, 701), w.app_of(b, doc, 702));
        let mut typed = Vec::new();
        let edit = async |w: &mut World, m: usize, app: &App, s: String| {
            let u = app.append(&s);
            w.edit(m, doc, &u);
            w.settle().await;
            s
        };
        for i in 0..12 {
            typed.push(edit(&mut w, a, &app_a, format!(" a{i}")).await);
        }
        // A backup now: the log, its base and snapshot as they are.
        let backup = w.meta.live_doc(w.space, doc).await.unwrap().unwrap();
        let backup_snapshot = w.meta.live_snapshot(w.space, doc).await.unwrap();
        assert!(backup.base > 0);
        for i in 0..12 {
            typed.push(edit(&mut w, b, &app_b, format!(" b{i}")).await);
        }
        assert!(w.meta.live_doc(w.space, doc).await.unwrap().unwrap().base > backup.head, "compacted past the backup");
        // Restored from the backup (both machines are ahead of it now), and its epoch reset as a restore does.
        w.disconnect(a);
        w.disconnect(b);
        w.meta.restore_live_log(w.space, doc, Some(backup.head)).await.unwrap();
        w.meta.restore_live_snapshot(w.space, doc, backup_snapshot).await.unwrap();
        w.meta.reset_epoch(w.space).await.unwrap();
        w.connect(a);
        w.connect(b);
        w.settle().await;
        let c = w.machine().await;
        w.connect(c);
        w.settle().await;
        joined(&mut w, c, node, "never asked").await;
        let text = w.text(c, doc);
        for s in &typed {
            assert!(text.contains(s.as_str()), "{s} lost after the restore: {text}");
        }
        assert_eq!(w.text(a, doc), text);
        assert_eq!(w.text(b, doc), text);
        // The admin's reset: the log and the snapshot (the import too) go; the machines push it all back. A machine
        // without a copy that opens it before they have is told to open the file (never handed an empty document).
        for m in [a, b, c] {
            w.disconnect(m);
        }
        w.meta.reset_live_doc(w.space, doc).await.unwrap();
        let d = w.machine().await;
        w.connect(d);
        w.settle().await;
        let (_, opening) = w.open(d, node);
        assert_eq!(opening, Opening::Joining);
        w.settle().await;
        let events = w.events(d);
        assert!(events.iter().any(|e| matches!(e, DocEvent::NotLive(_, NotLiveReason::Resetting, _))), "{events:?}");
        for m in [a, b, c] {
            w.connect(m);
        }
        w.settle().await;
        joined(&mut w, d, node, "never asked").await;
        assert_eq!(w.text(d, doc), text, "after the reset");
        // A restore from before the document was made at all: it is made again from a machine's copy.
        for m in [a, b, c, d] {
            w.disconnect(m);
        }
        w.meta.restore_live_log(w.space, doc, None).await.unwrap();
        assert!(w.meta.live_doc(w.space, doc).await.unwrap().is_none());
        w.meta.reset_epoch(w.space).await.unwrap();
        for m in [a, b, c, d] {
            w.connect(m);
        }
        w.settle().await;
        assert!(w.meta.live_doc(w.space, doc).await.unwrap().is_some(), "made again");
        let e = w.machine().await;
        w.connect(e);
        w.settle().await;
        joined(&mut w, e, node, "never asked").await;
        assert_eq!(w.text(e, doc), text, "after the document itself was lost");
    });
}

/// An agent against an older server (live protocol 1): recents go on, documents don't go live, and one that is live
/// here already keeps its changes on this machine.
#[test]
fn against_an_older_server_documents_wait_and_say_so() {
    rt().block_on(async {
        let mut w = World::new(Limits::default()).await;
        let a = w.machine().await;
        w.connect(a);
        w.settle().await;
        let node = w.file().await;
        let doc = joined(&mut w, a, node, "x").await;
        w.disconnect(a);
        w.machines[a].old_server = true;
        w.connect(a);
        w.settle().await;
        assert_eq!(w.machines[a].replica().documents_supported(), Some(false));
        let fresh = w.file().await;
        let (_, opening) = w.open(a, fresh);
        assert!(matches!(&opening, Opening::NotLive(NotLiveReason::Offline, m) if m.contains("newer")), "{opening:?}");
        assert_eq!(w.open(a, node).1, Opening::Live, "a document live here opens from its copy");
        let u = w.app_of(a, doc, 801).append("y");
        w.edit(a, doc, &u);
        w.settle().await;
        assert_eq!(w.machines[a].replica().document_status(doc), Some((false, 1)), "it waits");
        assert!(w.machines[a].said.iter().any(|s| s.contains("newer Craft Cloud server")), "{:?}", w.machines[a].said);
        // Recents on: an app's list works as ever.
        let app = craft_cloud_proto::live::AppName::new("gridcraft").unwrap();
        let entry = craft_cloud_proto::local::RecentEntry {
            node: None,
            path: "a.txt".into(),
            title: "a".into(),
            opened_ms: 1,
            saved_ms: None,
            machine: "pc".into(),
        };
        let frames = w.machines[a].replica().add(&app, [1; 16], entry).unwrap();
        w.send(a, frames);
        w.settle().await;
        assert_eq!(w.machines[a].replica().waiting_for(&app), 0, "the add went up");
        // Back to a newer server: the change goes up.
        w.disconnect(a);
        w.machines[a].old_server = false;
        w.connect(a);
        w.settle().await;
        assert_eq!(w.machines[a].replica().document_status(doc), Some((true, 0)));
    });
}
