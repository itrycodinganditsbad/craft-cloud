//! How fast live documents are. **Latency**: a keystroke in an app on one machine is applied in the open document on
//! another in under 250 ms, through the crate, two running agents and the server, on loopback with 2 ms of simulated
//! delay each way (95 keystrokes in 100, and the worst under a second: see [`P95_UNDER`]; each keystroke's time is
//! reported in order when it fails). **Size**: a 5 MB text document with 100,000 updates in its log opens on a machine that never had it
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
    assert_fast(&time_keystrokes(false).await);
}

/// The same while another program (a virus scanner, the search indexer, a backup tool) keeps opening the files the
/// agents write now and then, `status.json` and the live documents' files, without sharing them for deletion: the
/// agents' writes of them wait, or are left for later, and no keystroke waits for them.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_keystroke_isnt_held_up_by_a_scanner_holding_the_files_written_now_and_then() {
    assert_fast(&time_keystrokes(true).await);
}

/// Holds each agent's `status.json` and live document files (`<data>/live/<space>/*.doc` and `*.live`) open, sharing
/// them for reading only, for 400 ms of every 450 ms until `stop` is set.
#[cfg(windows)]
fn scanner(data: Vec<std::path::PathBuf>, stop: Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<()> {
    use std::os::windows::fs::OpenOptionsExt;
    std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let mut files: Vec<std::path::PathBuf> = data.iter().map(|d| d.join("status.json")).collect();
            for space in data.iter().flat_map(|d| std::fs::read_dir(d.join("live")).into_iter().flatten().flatten()) {
                for entry in std::fs::read_dir(space.path()).into_iter().flatten().flatten() {
                    if entry.path().extension().is_some_and(|x| x == "doc" || x == "live") {
                        files.push(entry.path());
                    }
                }
            }
            let held: Vec<std::fs::File> = files.iter().filter_map(|f| std::fs::OpenOptions::new().read(true).share_mode(1).open(f).ok()).collect();
            std::thread::sleep(Duration::from_millis(400));
            drop(held);
            std::thread::sleep(Duration::from_millis(50));
        }
    })
}

/// Two agents, a file opened live on both, and [`KEYSTROKES`] keystrokes typed on the pc and timed to the laptop's
/// app; with `scan`, a [`scanner`] holds the agents' files from just before the open on (Windows only).
async fn time_keystrokes(scan: bool) -> Vec<Keystroke> {
    let server = Server::delayed(Duration::from_millis(2)).await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let (on_pc, on_laptop) = (pc.client().await, laptop.client().await);
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join("fast.txt"), "").unwrap();
    let file = laptop.folder.join("fast.txt");
    eventually("the file reached the laptop", move || file.exists().then_some(())).await;
    let stop_scanning = Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(windows)]
    let scanning = scan.then(|| scanner(vec![pc.data.path().to_owned(), laptop.data.path().to_owned()], stop_scanning.clone()));
    #[cfg(not(windows))]
    let _ = scan;
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
    // Both online: the laptop's document followed, and the pc's caught up with its own import (the server sends it
    // back once it is kept). Until then the pc's keystrokes wait in its journal, and the first ones would time the
    // rest of that catch-up, not a keystroke.
    for (who, side) in [("pc", pc_side.clone()), ("laptop", laptop_side.clone())] {
        // As it was opened, then as each status since says.
        let online = std::sync::atomic::AtomicBool::new(side.lock().unwrap().1.opened_state().online);
        eventually(&format!("the {who}'s document is online"), move || {
            let side = side.lock().unwrap();
            for event in std::iter::from_fn(|| side.1.try_recv()) {
                if let Event::Status { online: now, .. } = event {
                    online.store(now, std::sync::atomic::Ordering::Relaxed);
                }
            }
            online.load(std::sync::atomic::Ordering::Relaxed).then_some(())
        })
        .await;
    }
    let (p, l) = (pc_side.clone(), laptop_side.clone());
    let typed = off(move || {
        let mut typed = Vec::new();
        let start = Instant::now();
        for i in 0..KEYSTROKES {
            let (at, mut events) = {
                let (mine, doc) = &*p.lock().unwrap();
                let before = mine.oplog_vv();
                let text = mine.get_text("text");
                text.insert(text.len_unicode(), &format!("{}", i % 10)).unwrap();
                mine.commit();
                let update = mine.export(ExportMode::updates(&before)).unwrap();
                // What the pc's document said since the last keystroke (an offline spell, say).
                let events: Vec<String> = std::iter::from_fn(|| doc.try_recv()).filter(worth_saying).map(|e| format!("pc {e:?}")).collect();
                let at = Instant::now();
                doc.send(&update).unwrap();
                (at, events)
            };
            let want = i + 1;
            let (theirs, doc) = &*l.lock().unwrap();
            let took = loop {
                match doc.recv_timeout(Duration::from_secs(5)) {
                    Some(Event::Update(bytes)) => {
                        theirs.import(&bytes).unwrap();
                        if theirs.get_text("text").len_unicode() >= want {
                            break at.elapsed();
                        }
                    }
                    Some(other) => {
                        if worth_saying(&other) {
                            events.push(format!("laptop {other:?}"));
                        }
                    }
                    None => panic!("keystroke {i} never arrived"),
                }
            };
            typed.push(Keystroke { sent: at - start, took, events });
            // A typist's pace: a keystroke batch every 30 ms.
            std::thread::sleep(Duration::from_millis(30));
        }
        typed
    })
    .await;
    stop_scanning.store(true, std::sync::atomic::Ordering::Relaxed);
    #[cfg(windows)]
    if let Some(scanning) = scanning {
        scanning.join().unwrap();
    }
    // The agents stop before anything is asserted: a failure doesn't remove their folders under them (which only adds
    // "couldn't be written to disk" lines to the output).
    drop((docs_pc, docs_laptop, pc_side, laptop_side));
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
    typed
}

/// Holds the keystrokes' times to the bounds below, showing each keystroke in order.
fn assert_fast(typed: &[Keystroke]) {
    let latency = Latency::of(typed);
    // (Shown with the failure, or with `--nocapture`.)
    let in_order: String = typed.iter().enumerate().map(|(i, k)| format!("\n  {i:2} {k}")).collect();
    eprintln!("keystroke to remote apply: {latency}; in order:{in_order}");
    assert!(latency.median < MEDIAN_UNDER, "the median keystroke took {:?}, over {MEDIAN_UNDER:?} (each keystroke above)", latency.median);
    assert!(latency.p95 < P95_UNDER, "the 95th percentile took {:?}, over {P95_UNDER:?} (each keystroke above)", latency.p95);
    assert!(latency.worst < WORST_UNDER, "keystroke {} took {:?}, over {WORST_UNDER:?} (each keystroke above)", latency.worst_at, latency.worst);
}

/// Whether a document's event is worth reporting beside a keystroke's time: anything but acks and the count of
/// changes waiting going up and down while it is online.
fn worth_saying(event: &Event) -> bool {
    !matches!(event, Event::Acked(_) | Event::Status { online: true, deleted: false, stopped: false, .. })
}

/// Keystrokes typed in the latency test.
const KEYSTROKES: usize = 40;
/// The latency test's bounds. A keystroke reaches the other machine in under 250 ms: 95 in 100 do, every time, and
/// the typical one well inside that. A debug build on a shared CI runner (two to four cores, with the rest of the
/// workspace's tests just run) can lose one or two keystrokes to the scheduler or the disk, so the worst one only has
/// to stay under a second; a delay many keystrokes share (on each keystroke, or on every few) still fails the median
/// or the 95th percentile. (Windows' timers tick every 15.6 ms, so each of the gate's 2 ms delays is one tick there:
/// its median is about 50 ms.)
const MEDIAN_UNDER: Duration = Duration::from_millis(100);
const P95_UNDER: Duration = Duration::from_millis(250);
const WORST_UNDER: Duration = Duration::from_secs(1);

/// One keystroke of the latency test: when it was sent (from the first), how long it took to reach the laptop, and
/// what else the two documents said meanwhile.
struct Keystroke {
    sent: Duration,
    took: Duration,
    events: Vec<String>,
}

impl std::fmt::Display for Keystroke {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sent at {:>9.1?}, took {:>9.1?}", self.sent, self.took)?;
        for event in &self.events {
            write!(f, "; {event}")?;
        }
        Ok(())
    }
}

/// The median, 95th percentile (nearest rank) and worst of the keystrokes' times, and which keystroke the worst was.
struct Latency {
    median: Duration,
    p95: Duration,
    worst: Duration,
    worst_at: usize,
    count: usize,
}

impl Latency {
    fn of(typed: &[Keystroke]) -> Self {
        let mut sorted: Vec<Duration> = typed.iter().map(|k| k.took).collect();
        sorted.sort();
        let rank = |p: usize| sorted[(sorted.len() * p).div_ceil(100).saturating_sub(1)];
        let (worst_at, worst) = typed.iter().map(|k| k.took).enumerate().max_by_key(|(_, t)| *t).unwrap();
        Self { median: rank(50), p95: rank(95), worst, worst_at, count: typed.len() }
    }
}

impl std::fmt::Display for Latency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { median, p95, worst, worst_at, count } = self;
        write!(f, "median {median:?}, 95th percentile {p95:?}, worst {worst:?} (keystroke {worst_at}) over {count} keystrokes")
    }
}

/// The bounds fail what they are meant to fail: a 400 ms delay on every keystroke, or on every tenth, fails; one
/// keystroke held up once doesn't.
#[test]
fn the_latency_bounds_fail_a_shared_delay_but_not_one_hiccup() {
    let fast = Duration::from_millis(10);
    let slow = Duration::from_millis(400);
    let run = |slow_at: &dyn Fn(usize) -> bool| -> Vec<Keystroke> {
        (0..KEYSTROKES)
            .map(|i| Keystroke { sent: Duration::from_millis(40) * i as u32, took: if slow_at(i) { slow } else { fast }, events: Vec::new() })
            .collect()
    };
    let passes = |l: &Latency| l.median < MEDIAN_UNDER && l.p95 < P95_UNDER && l.worst < WORST_UNDER;
    assert!(passes(&Latency::of(&run(&|i| i == 17))));
    let hiccup = Latency::of(&run(&|i| i == 17));
    assert_eq!((hiccup.worst, hiccup.worst_at), (slow, 17));
    assert!(!passes(&Latency::of(&run(&|_| true))));
    assert!(!passes(&Latency::of(&run(&|i| i % 10 == 0))));
    assert!(!passes(&Latency::of(&run(&|i| i % 2 == 0))));
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
        let import = w.upload(doc, &import).await;
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
        let stored = w.upload(doc, &snapshot).await;
        assert!(w.meta.compact_live_doc(w.space, doc, 99_000, &stored).await.unwrap());
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
