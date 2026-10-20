//! The kit simulation: app sessions on three machines of one fake hub (two apps on the third), each making random
//! edits of every shape (tree, rich text, table, timeline, opaque; stories and tables nested in layers, a table whose
//! cells are maps with stories), undoing and redoing, saving, while machines go
//! offline, drop their apps' connections and restart, and the live relay goes down while files still sync. Healed,
//! every session ends with the same model, every model is one the app could make (normalised), and the file catches up
//! with the autosave rule within a save per window.
//!
//! `SIM_SEEDS=5000 SIM_FIRST=0 cargo test --release -p craft-cloud-kit --test kit_sim` runs seeds 0 to 4999 on
//! `SIM_JOBS` threads (default 4); `SIM_SEED=17` runs one. A seed's history is random but not replayed exactly: the
//! fake agent's threads and the sessions' reconnects run on the clock.

mod common;
mod model;

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::{change, open};
use craft_cloud_app::fake::{Fake, Hub};
use craft_cloud_kit::{KitError, Session};
use model::{Doc, Rng, SHAPES, TestApp, edit, valid};

const PATH: &str = "sim/story.kitdemo";
const STEPS: usize = 80;
/// Which machine each session runs on: the third machine has two apps open on the document.
const ON: [usize; 4] = [0, 1, 2, 2];

fn run_seed(seed: u64) {
    let hub = Hub::new();
    let fakes: Vec<Fake> = (0..3).map(|i| hub.machine(&format!("m{i}")).unwrap()).collect();
    let mut rng = Rng(seed);
    let start = model::random_doc(&mut rng, 12);
    let mut sessions: Vec<Session<TestApp>> = Vec::new();
    for (k, m) in ON.iter().enumerate() {
        sessions.push(open::<TestApp>(&fakes[*m], PATH, if k == 0 { start.clone() } else { Doc::empty() }));
    }
    let mut stopped = [false; 3];
    for step in 0..STEPS {
        let i = rng.below(sessions.len());
        let m = ON[i];
        match rng.below(24) {
            0..=11 => {
                let shape = SHAPES[rng.below(SHAPES.len())];
                let mut r = rng.clone();
                match change(&mut sessions[i], |d| edit(d, shape, &mut r)) {
                    Ok(()) | Err(KitError::Stalled(_)) => {}
                    Err(e) => panic!("seed {seed} step {step}: session {i}'s change failed: {e}"),
                }
                rng = r;
            }
            12 | 13 => {
                sessions[i].undo().unwrap_or_else(|e| panic!("seed {seed} step {step}: undo: {e}"));
            }
            14 => {
                sessions[i].redo().unwrap_or_else(|e| panic!("seed {seed} step {step}: redo: {e}"));
            }
            15 => {
                if let Ok(point) = sessions[i].save_point() {
                    sessions[i].record(&point, format!("seed {seed} step {step}").as_bytes());
                }
            }
            16 => fakes[m].set_online(false),
            17 | 18 => fakes[m].set_online(true),
            19 => fakes[m].drop_connections(),
            20 => {
                fakes[m].restart();
                stopped[m] = true;
            }
            21 => {
                fakes[m].start();
                stopped[m] = false;
            }
            // The live relay goes down (files still sync) or comes back.
            22 => hub.set_live(rng.below(2) == 0),
            _ => std::thread::sleep(Duration::from_millis(rng.below(3) as u64)),
        }
        for s in &mut sessions {
            s.poll();
        }
    }
    // Healed: every machine runs and is online, the live relay is up, and every session tries at once.
    hub.set_live(true);
    for f in &fakes {
        f.start();
        f.set_online(true);
    }
    for s in &mut sessions {
        s.reconnect_now();
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        for s in &mut sessions {
            s.poll();
        }
        let quiet = sessions.iter().all(|s| s.status().connected && s.status().queued == 0) && fakes.iter().all(|f| f.waiting(PATH) == 0);
        if quiet {
            let first = sessions[0].model().clone();
            if sessions.iter().all(|s| **s.model() == *first) {
                break;
            }
        }
        if Instant::now() > deadline {
            let views: Vec<String> = sessions.iter().map(|s| format!("{:?}\n  {:?}", s.status(), s.model())).collect();
            panic!("seed {seed}: the sessions never agreed:\n{}", views.join("\n"));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // The file catches up on its own: one window saves, then each saves while its file is behind (as an app's
    // autosave does), and soon none is; the file then has the version every session has.
    if let Ok(point) = sessions[0].save_point() {
        sessions[0].record(&point, b"healed");
    }
    let heads = |s: &Session<TestApp>| {
        let doc = loro::LoroDoc::new();
        doc.import(&s.snapshot().unwrap()).unwrap();
        let mut v: Vec<(u64, u32)> = doc.oplog_frontiers().iter().map(|id| (id.peer, u32::try_from(id.counter).unwrap())).collect();
        v.sort_unstable();
        v
    };
    let (mut saves, mut quiet) = (0usize, 0);
    // (A save the file being behind asks for may wait out `CATCH_UP_EVERY` after the window's last one.)
    while quiet < 30 || sessions.iter().any(|s| hub.file(PATH) != Some(heads(s))) {
        let mut saved = false;
        for s in &mut sessions {
            s.poll();
            if s.file_behind()
                && let Ok(point) = s.save_point()
            {
                s.record(&point, format!("caught up {saves}").as_bytes());
                saves += 1;
                saved = true;
            }
        }
        quiet = if saved { 0 } else { quiet + 1 };
        assert!(Instant::now() < deadline + Duration::from_secs(30), "seed {seed}: the file never caught up ({saves} saves)");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(saves <= sessions.len(), "seed {seed}: {saves} saves to catch up");
    for (i, s) in sessions.iter().enumerate() {
        assert_eq!(hub.file(PATH), Some(heads(s)), "seed {seed}: session {i}'s edits aren't all in the file");
    }
    for (i, s) in sessions.iter().enumerate() {
        if let Err(why) = valid(s.model()) {
            panic!("seed {seed}: session {i}'s model isn't one the app could make: {why}");
        }
        assert!(s.status().read_only.is_none());
        // The model the session built change by change (texts patched from Loro's deltas) is the one its document
        // reads back whole.
        let mut replica = craft_cloud_kit::raw::Replica::default();
        replica.import(&[s.snapshot().unwrap()], &["layers"]).unwrap();
        let mut whole = (*replica.read::<TestApp>(&std::sync::Arc::new(Doc::empty())).unwrap()).clone();
        <TestApp as craft_cloud_kit::LiveDocument>::normalise(&mut whole);
        <TestApp as craft_cloud_kit::LiveDocument>::refresh_derived(&mut whole);
        assert!(whole == **s.model(), "seed {seed}: session {i}'s model isn't what its document reads back:\n{whole:?}\n{:?}", s.model());
    }
}

/// The seeds the environment asks for, on threads; the lowest failing seed is reported.
#[test]
fn kit_sessions_converge() {
    if let Ok(seed) = std::env::var("SIM_SEED") {
        run_seed(seed.parse().unwrap());
        return;
    }
    let seeds: u64 = std::env::var("SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(16);
    let first: u64 = std::env::var("SIM_FIRST").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let jobs: usize = std::env::var("SIM_JOBS").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    let next = AtomicU64::new(first);
    let failed: Mutex<Option<(u64, String)>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let seed = next.fetch_add(1, Ordering::Relaxed);
                    if seed >= first + seeds || failed.lock().unwrap().as_ref().is_some_and(|(s, _)| *s < seed) {
                        return;
                    }
                    if let Err(e) = std::panic::catch_unwind(|| run_seed(seed)) {
                        let message =
                            e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned())).unwrap_or_default();
                        let mut f = failed.lock().unwrap();
                        if f.as_ref().is_none_or(|(s, _)| seed < *s) {
                            *f = Some((seed, message));
                        }
                    }
                }
            });
        }
    });
    if let Some((seed, message)) = failed.into_inner().unwrap() {
        panic!("seed {seed} failed (SIM_SEED={seed} replays it, as near as the clock allows): {message}");
    }
}
