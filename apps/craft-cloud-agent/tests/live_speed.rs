//! How fast live documents are. **Latency**: a keystroke in an app on one machine is applied in the open document on
//! another in under 250 ms, through the crate, two running agents and the server, on loopback with 2 ms of simulated
//! delay each way. **Size**: a 5 MB text document with 100,000 updates in its log opens on a machine that never had it
//! in under a second, from the server's snapshot and the log above it. The size test runs only in a release build
//! (`cargo test --release`), or with `LIVE_SIZE=1`, as the long simulations do.

mod looping;

#[path = "docs_common/mod.rs"]
mod docs_common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use craft_cloud_app::live::{Event, Opened};
use looping::{Agent, Server, eventually};
use loro::{ExportMode, LoroDoc};

/// Blocking work off the test's runtime.
async fn off<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_keystroke_reaches_the_other_machine_in_under_250_ms() {
    let server = Server::delayed(Duration::from_millis(2)).await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let (on_pc, on_laptop) = (pc.client().await, laptop.client().await);
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join("fast.txt"), "").unwrap();
    let file = laptop.folder.join("fast.txt");
    eventually("the file reached the laptop", move || file.exists().then_some(())).await;
    let c = on_pc.clone();
    let (docs_pc, doc_pc) = off(move || {
        let docs = c.documents().unwrap();
        let doc = match docs.open("fast.txt", "wordcraft").unwrap() {
            Opened::NeedsImport(importer) => importer.import(&craft_cloud_agent::doc::plain_text_snapshot("").unwrap()).unwrap(),
            Opened::Live(doc) => doc,
            Opened::NotLive(reason) => panic!("{reason:?}"),
        };
        (docs, doc)
    })
    .await;
    let c = on_laptop.clone();
    let (docs_laptop, doc_laptop) = off(move || {
        let docs = c.documents().unwrap();
        let Opened::Live(doc) = docs.open("fast.txt", "wordcraft").unwrap() else { panic!("not live on the laptop") };
        (docs, doc)
    })
    .await;
    let mine = LoroDoc::new();
    mine.import(doc_pc.state()).unwrap();
    let theirs = LoroDoc::new();
    theirs.import(doc_laptop.state()).unwrap();
    let (pc_side, laptop_side) = (Arc::new(Mutex::new((mine, doc_pc))), Arc::new(Mutex::new((theirs, doc_laptop))));
    // Both online (the laptop's document followed).
    let l = laptop_side.clone();
    eventually("the laptop's document is online", move || {
        let side = l.lock().unwrap();
        (side.1.opened_state().online || std::iter::from_fn(|| side.1.try_recv()).any(|e| matches!(e, Event::Status { online: true, .. })))
            .then_some(())
    })
    .await;
    let (p, l) = (pc_side.clone(), laptop_side.clone());
    let times = off(move || {
        let mut times = Vec::new();
        for i in 0..40 {
            let typed = {
                let (mine, doc) = &*p.lock().unwrap();
                let before = mine.oplog_vv();
                let text = mine.get_text("text");
                text.insert(text.len_unicode(), &format!("{}", i % 10)).unwrap();
                mine.commit();
                let update = mine.export(ExportMode::updates(&before)).unwrap();
                let at = Instant::now();
                doc.send(&update).unwrap();
                at
            };
            let want = i + 1;
            let (theirs, doc) = &*l.lock().unwrap();
            loop {
                match doc.recv_timeout(Duration::from_secs(5)) {
                    Some(Event::Update(bytes)) => {
                        theirs.import(&bytes).unwrap();
                        if theirs.get_text("text").len_unicode() >= want {
                            times.push(typed.elapsed());
                            break;
                        }
                    }
                    Some(_) => {}
                    None => panic!("keystroke {i} never arrived"),
                }
            }
            // A typist's pace: a keystroke batch every 30 ms.
            std::thread::sleep(Duration::from_millis(30));
        }
        times
    })
    .await;
    let mut sorted = times.clone();
    sorted.sort();
    let (median, worst) = (sorted[sorted.len() / 2], sorted[sorted.len() - 1]);
    eprintln!("keystroke to remote apply: median {median:?}, worst {worst:?} over {} keystrokes", sorted.len());
    assert!(worst < Duration::from_millis(250), "a keystroke took {worst:?}: {sorted:?}");
    drop((docs_pc, docs_laptop, pc_side, laptop_side));
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}

/// A 5 MB text document with 100,000 updates in its log (compacted, with a tail above the snapshot) opens on a fresh
/// machine in under a second: the catch-up from the snapshot and the tail, and the state an app builds from.
#[test]
fn a_big_document_opens_from_its_snapshot_and_tail_in_under_a_second() {
    if cfg!(debug_assertions) && std::env::var_os("LIVE_SIZE").is_none() {
        eprintln!("skipped: a release build (or LIVE_SIZE=1) runs it");
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        use craft_cloud_agent::live::replica::{DocEvent, Opening};
        use craft_cloud_proto::live::{ClientFrame, ServerFrame};
        let mut w =
            docs_common::World::new(craft_cloud_server::state::Limits { compact_updates: u64::MAX, compact_bytes: u64::MAX, ..Default::default() })
                .await;
        let fresh = w.machine().await;
        let node = w.file().await;
        let doc = craft_cloud_proto::live::node_doc(w.space, node);
        // The document as its machines made it: 100,000 changes of about 50 characters, 5 MB of text.
        let made = LoroDoc::new();
        made.set_peer_id(1).unwrap();
        let import = made.export(ExportMode::Snapshot).unwrap();
        let token = [97u8; 32];
        w.meta.create_device(w.user, "writer", &token, 0).await.unwrap();
        let auth = w.meta.device_by_token(&token).await.unwrap().unwrap();
        let mut writer = craft_cloud_server::live::Session::new(w.state.clone(), &auth);
        writer.handle(ClientFrame::Hello { min: 1, max: 2 }).await;
        let made_reply = writer.handle(ClientFrame::CreateDocument { doc, node, app: docs_common::app(), snapshot: import }).await;
        assert_eq!(made_reply.frames, vec![ServerFrame::CreatedDocument { doc, kept: true }]);
        let line = "the quick brown fox jumps over the lazy dog, again\n";
        let mut tail_from = None;
        for i in 0..100_000u64 {
            let before = made.oplog_vv();
            let text = made.get_text("text");
            text.insert(text.len_unicode(), line).unwrap();
            made.commit();
            let update = made.export(ExportMode::updates(&before)).unwrap();
            w.meta.append_live_update(w.space, doc, auth.device, i + 1, &update, 0).await.unwrap();
            if i + 1 == 99_000 {
                tail_from = Some(made.export(ExportMode::Snapshot).unwrap());
            }
        }
        // Compacted at 99,000 (as an agent would answer), the last 1,000 above it.
        let snapshot = tail_from.unwrap();
        let (stored, pieces) = craft_cloud_meta::SnapshotRef::split(&snapshot);
        for (id, piece) in stored.pieces.iter().zip(pieces) {
            w.state.store.put(craft_cloud_store::BlobKey::chunk(id), piece).await.unwrap();
        }
        assert!(w.meta.compact_live_doc(w.space, doc, 99_000, &stored, 0).await.unwrap());
        let text_len = made.get_text("text").len_utf8();
        assert!(text_len >= 5_000_000, "{text_len} bytes of text");
        // A machine that never had it opens it.
        w.connect(fresh);
        w.settle().await;
        let started = Instant::now();
        let (_, opening) = w.open(fresh, node);
        assert_eq!(opening, Opening::Joining);
        w.settle().await;
        assert!(w.events(fresh).contains(&DocEvent::Joined(doc)));
        let joined_in = started.elapsed();
        let app = w.app_of(fresh, doc, 5);
        eprintln!("joined in {joined_in:?}, the app's copy built in {:?}", started.elapsed() - joined_in);
        let took = started.elapsed();
        assert_eq!(app.doc.get_text("text").len_utf8(), text_len);
        eprintln!("a {text_len}-byte document with 100,000 updates opened in {took:?}");
        assert!(took < Duration::from_secs(1), "it took {took:?}");
    });
}
