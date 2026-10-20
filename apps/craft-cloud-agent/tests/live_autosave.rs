//! Two machines with one live document open, both autosaving the way the kit says (when dirty, or when the file is
//! behind the live document), against running agents and a real server: edits made at once on both, and saves made
//! while the server is away, end with the file holding every edit on both machines, no conflict copy, and then no
//! more saves (no loop).

mod looping;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use craft_cloud_kit::{Agent as KitAgent, ElemId, KitError, LiveDocument, Mappings, Open, RemotePatch, Session, TableSource, Value};
use looping::{Agent, Server, eventually};

/// The one column.
const TEXT: ElemId = ElemId(1);

/// A list of lines, a line per row.
#[derive(Clone, Debug, PartialEq, Default)]
struct Lines {
    rows: Arc<Vec<(ElemId, String)>>,
}

impl Lines {
    fn texts(&self) -> Vec<String> {
        self.rows.iter().map(|(_, t)| t.clone()).collect()
    }

    fn with(&self, text: &str) -> Self {
        let mut rows = (*self.rows).clone();
        rows.push((ElemId::new(), text.to_owned()));
        Self { rows: Arc::new(rows) }
    }

    fn to_file(&self) -> Vec<u8> {
        self.texts().join("\n").into_bytes()
    }

    fn from_file(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes);
        Self { rows: Arc::new(text.lines().filter(|l| !l.is_empty()).map(|l| (ElemId::new(), l.to_owned())).collect()) }
    }
}

struct Rows<'a>(&'a Lines);

impl TableSource for Rows<'_> {
    fn key(&self) -> Option<usize> {
        Some(Arc::as_ptr(&self.0.rows) as usize)
    }

    fn rows(&self) -> Vec<ElemId> {
        self.0.rows.iter().map(|(id, _)| *id).collect()
    }

    fn columns(&self) -> Vec<ElemId> {
        vec![TEXT]
    }

    fn cell(&self, row: ElemId, column: ElemId) -> Option<Value> {
        (column == TEXT).then(|| self.0.rows.iter().find(|(id, _)| *id == row).map(|(_, t)| Value::from(t.as_str()))).flatten()
    }

    fn cells(&self) -> Vec<(ElemId, ElemId, Value)> {
        self.0.rows.iter().map(|(id, t)| (*id, TEXT, Value::from(t.as_str()))).collect()
    }
}

struct LinesApp;

impl LiveDocument for LinesApp {
    type Doc = Lines;
    const APP: &'static str = "lines";
    const SCHEMA: u32 = 1;

    fn mappings(doc: &Lines) -> Mappings<'_> {
        Mappings::new().table("lines", Rows(doc))
    }

    fn apply_remote(old: &Arc<Lines>, patch: &RemotePatch<'_>) -> Result<Arc<Lines>, KitError> {
        let Some(table) = patch.table("lines") else { return Ok(old.clone()) };
        let rows = table
            .rows()
            .into_iter()
            .map(|row| (row, table.cell(row, TEXT).and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()))
            .collect();
        Ok(Arc::new(Lines { rows: Arc::new(rows) }))
    }

    fn normalise(_doc: &mut Lines) {}

    fn refresh_derived(_doc: &mut Lines) {}
}

const FILE: &str = "n.lines";
/// How late every save record reaches each agent's save book.
const RECORD_DELAY_MS: u64 = 1500;

/// What the two windows share with the test.
#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    saves: AtomicUsize,
    /// When the last save was made, in ms since the test started.
    last_save_ms: AtomicU64,
    /// Windows done with their first half of edits, and with all.
    half_done: AtomicUsize,
    edits_done: AtomicUsize,
    /// The second half may start (the server is away).
    second_half: AtomicBool,
}

/// One window: opens the file live, adds `edits` lines (one every 40 ms), and autosaves (at most every 100 ms) when
/// dirty or when the file is behind, in the kit's order (save point, record, then the write), until told to stop.
/// Its model and whether its file was behind at the end.
fn window(client: craft_cloud_app::Client, folder: PathBuf, name: &'static str, edits: usize, shared: Arc<Shared>, t0: Instant) -> (Lines, bool) {
    let path = folder.join(FILE);
    let bytes = std::fs::read(&path).unwrap();
    let agent = KitAgent::from_fn(move || Ok(client.clone()));
    let mut session = match Session::<LinesApp>::open(&agent, FILE, Arc::new(Lines::from_file(&bytes)), Some(&bytes)).unwrap() {
        Open::Live(s) => *s,
        Open::NotLive(why) => panic!("{name}: not live: {why:?}"),
    };
    let (mut made, mut dirty, mut next_edit, mut last_save) = (0, false, Instant::now(), Instant::now() - Duration::from_secs(1));
    let tmp = folder.parent().unwrap().join(format!(".{name}.tmp"));
    while !shared.stop.load(Ordering::SeqCst) {
        session.poll();
        let may = made < edits / 2 || (made < edits && shared.second_half.load(Ordering::SeqCst));
        if may && Instant::now() >= next_edit {
            let old = session.model().clone();
            let new = Arc::new(old.with(&format!("{name} {made}")));
            if session.local_change(&old, &new).is_ok() {
                made += 1;
                dirty = true;
                if made == edits / 2 {
                    shared.half_done.fetch_add(1, Ordering::SeqCst);
                }
                if made == edits {
                    shared.edits_done.fetch_add(1, Ordering::SeqCst);
                }
            }
            next_edit = Instant::now() + Duration::from_millis(40);
        }
        if (dirty || session.file_behind()) && last_save.elapsed() >= Duration::from_millis(100) {
            let point = session.save_point().unwrap();
            let bytes = point.model().to_file();
            session.record(&point, &bytes);
            std::fs::write(&tmp, &bytes).unwrap();
            std::fs::rename(&tmp, &path).unwrap();
            dirty = false;
            last_save = Instant::now();
            shared.saves.fetch_add(1, Ordering::SeqCst);
            shared.last_save_ms.store(t0.elapsed().as_millis() as u64, Ordering::SeqCst);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let behind = session.file_behind();
    (session.model().as_ref().clone(), behind)
}

/// Every file in `folder`, by name.
fn files(folder: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(folder)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()))
        .collect();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_machines_autosaving_end_with_every_edit_in_the_file_and_stop() {
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    // Slow agents: every save record reaches the save book late, after the app wrote its file (as under load).
    let slow = || craft_cloud_agent::daemon::RunOptions { record_delay: Duration::from_millis(RECORD_DELAY_MS), ..looping::quick() };
    let (run_pc, run_laptop) = (pc.run_with(slow()), laptop.run_with(slow()));
    let (on_pc, on_laptop) = (pc.client().await, laptop.client().await);
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join(FILE), "start").unwrap();
    let there = laptop.folder.join(FILE);
    eventually("the file reached the laptop", move || there.exists().then_some(())).await;
    let shared = Arc::new(Shared::default());
    // More load on demand: `CRAFT_CLOUD_TEST_LOAD=8` adds eight threads that keep the CPUs busy throughout.
    let burners: usize = std::env::var("CRAFT_CLOUD_TEST_LOAD").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    for _ in 0..burners {
        let s = shared.clone();
        std::thread::spawn(move || {
            let mut x = 0u64;
            while !s.stop.load(Ordering::Relaxed) {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                std::hint::black_box(x);
            }
        });
    }
    let t0 = Instant::now();
    // The pc opens first (it imports the file), then the laptop.
    let (s, folder, c) = (shared.clone(), pc.folder.clone(), on_pc.clone());
    let first = std::thread::spawn(move || window(c, folder, "pc", 12, s, t0));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (s, folder, c) = (shared.clone(), laptop.folder.clone(), on_laptop.clone());
    let second = std::thread::spawn(move || window(c, folder, "laptop", 12, s, t0));
    // Halfway, the server goes away: both make the rest of their edits and save them; their saves go up once it is
    // back, and the later upload wins without the other's last edits in it. Nothing is edited after that: only the
    // file being behind makes the other window save again.
    let s = shared.clone();
    eventually("both windows made half their edits", move || (s.half_done.load(Ordering::SeqCst) == 2).then_some(())).await;
    server.gate.down();
    shared.second_half.store(true, Ordering::SeqCst);
    let s = shared.clone();
    eventually("both windows made their edits", move || (s.edits_done.load(Ordering::SeqCst) == 2).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    server.gate.up();
    // Settled: both files have every line (the first, and 12 from each), alike, and no save for 3 s.
    let (pc_folder, laptop_folder, s) = (pc.folder.clone(), laptop.folder.clone(), shared.clone());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        // A window that stopped early (its thread panicked) says why, rather than leave the files to never settle.
        assert!(!first.is_finished() && !second.is_finished(), "a window stopped early (see its panic above)");
        let (on_pc, on_laptop) = (files(&pc_folder), files(&laptop_folder));
        let full = |f: &[(String, Vec<u8>)]| f.len() == 1 && Lines::from_file(&f[0].1).rows.len() == 25;
        let quiet = t0.elapsed().as_millis() as u64 > s.last_save_ms.load(Ordering::SeqCst) + 3000;
        if full(&on_pc) && on_pc == on_laptop && quiet {
            break;
        }
        let show = |f: &[(String, Vec<u8>)]| f.iter().map(|(n, d)| format!("{n}: {}", String::from_utf8_lossy(d))).collect::<Vec<_>>();
        assert!(
            Instant::now() < deadline,
            "never settled: pc {:?}, laptop {:?}, {} saves",
            show(&on_pc),
            show(&on_laptop),
            s.saves.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Still quiet a while later: no save loop.
    let saves = shared.saves.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(shared.saves.load(Ordering::SeqCst), saves, "no more saves once the file has everything");
    shared.stop.store(true, Ordering::SeqCst);
    let (model_pc, behind_pc) = tokio::task::spawn_blocking(move || first.join().unwrap()).await.unwrap();
    let (model_laptop, behind_laptop) = tokio::task::spawn_blocking(move || second.join().unwrap()).await.unwrap();
    assert!(!behind_pc && !behind_laptop);
    let mut want = model_pc.texts();
    want.sort();
    let mut got = model_laptop.texts();
    got.sort();
    assert_eq!(want, got);
    assert_eq!(want.len(), 25, "every edit: {want:?}");
    let file = Lines::from_file(&files(&pc.folder)[0].1).texts();
    assert_eq!(file, model_pc.texts(), "the file is the live document");
    // Bounded: every save is an edit's, or one to catch up after another machine's save won.
    assert!(saves <= 24 + 12, "{saves} saves");
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}

/// Saves the session's model in the kit's order (save point, record, write); the bytes.
fn save_now(session: &mut Session<LinesApp>, path: &std::path::Path) -> Vec<u8> {
    let point = session.save_point().unwrap();
    let bytes = point.model().to_file();
    session.record(&point, &bytes);
    let tmp = path.with_extension("tmp-save");
    std::fs::write(&tmp, &bytes).unwrap();
    std::fs::rename(&tmp, path).unwrap();
    bytes
}

/// A save made while the app has no agent (the agent restarts) can't be announced: the agent, back, still waits for
/// it, since an app had the document open moments ago (kept on disk), while the laptop's own Craft save of the same
/// document has gone up meanwhile. The app reconnects within its backoff and records the save: no conflict copy, and
/// the version on the server carries the record. (Before: the agent's first pass made a conflict copy.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_made_while_the_agent_was_away_is_waited_for_and_never_conflicts() {
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let (_on_pc, on_laptop) = (pc.client().await, laptop.client().await);
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join(FILE), "start").unwrap();
    let there = laptop.folder.join(FILE);
    eventually("the file reached the laptop", move || there.exists().then_some(())).await;
    // The pc's app finds its agent afresh each time (it restarts below).
    let pc_data = pc.data.path().to_owned();
    let pc_agent = KitAgent::from_fn(move || craft_cloud_app::Client::connect_in(&pc_data));
    let (pc_folder, laptop_folder) = (pc.folder.clone(), laptop.folder.clone());
    let open_on = |agent: KitAgent, folder: PathBuf| {
        let bytes = std::fs::read(folder.join(FILE)).unwrap();
        match Session::<LinesApp>::open(&agent, FILE, Arc::new(Lines::from_file(&bytes)), Some(&bytes)).unwrap() {
            Open::Live(s) => *s,
            Open::NotLive(why) => panic!("not live: {why:?}"),
        }
    };
    let (a_agent, a_folder) = (pc_agent.clone(), pc_folder.clone());
    let mut a = tokio::task::spawn_blocking(move || open_on(a_agent, a_folder)).await.unwrap();
    let laptop_client = on_laptop.clone();
    let b_folder = laptop_folder.clone();
    let mut b = tokio::task::spawn_blocking(move || open_on(KitAgent::from_fn(move || Ok(laptop_client.clone())), b_folder)).await.unwrap();
    // The pc's agent goes away; the app saves meanwhile, unannounced.
    run_pc.stop().await.0.unwrap();
    let (mut a, pc_saved) = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while a.poll().status.connected {
            assert!(Instant::now() < deadline, "the app saw its agent go");
            std::thread::sleep(Duration::from_millis(20));
        }
        let old = a.model().clone();
        a.local_change(&old, &Arc::new(old.with("pc, while its agent was away"))).unwrap();
        let saved = save_now(&mut a, &pc_folder.join(FILE));
        (a, saved)
    })
    .await
    .unwrap();
    // The laptop saves its own edit, and it goes up with its record.
    let (b_back, laptop_saved) = tokio::task::spawn_blocking(move || {
        let old = b.model().clone();
        b.local_change(&old, &Arc::new(old.with("the laptop's"))).unwrap();
        let saved = save_now(&mut b, &laptop_folder.join(FILE));
        (b, saved)
    })
    .await
    .unwrap();
    let mut b = b_back;
    let m = server.meta.clone();
    let space = server.meta.user_by_name("ana").await.unwrap().unwrap().space;
    let up = move || {
        let m = m.clone();
        async move {
            let tree = m.tree(space, None, 5000).await.unwrap();
            tree.nodes.into_iter().find_map(|n| match n.kind {
                craft_cloud_proto::NodeKind::File(f) if n.name.as_ref().is_some_and(|x| x.as_str() == FILE) => Some(f),
                _ => None,
            })
        }
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(f) = up().await
            && f.save.is_some()
            && f.size == laptop_saved.len() as u64
        {
            break;
        }
        assert!(Instant::now() < deadline, "the laptop's save went up");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The pc's agent is back; its app reconnects within its backoff and records the save it made.
    let run_pc = pc.run();
    let _back = pc.client().await;
    let (pc_dir, laptop_dir) = (pc.folder.clone(), laptop.folder.clone());
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        a = tokio::task::spawn_blocking(move || {
            a.poll();
            a
        })
        .await
        .unwrap();
        b = tokio::task::spawn_blocking(move || {
            b.poll();
            b
        })
        .await
        .unwrap();
        let (on_pc, on_laptop) = (files(&pc_dir), files(&laptop_dir));
        let conflict = on_pc.iter().chain(&on_laptop).any(|(n, _)| n.contains("conflict"));
        assert!(!conflict, "a conflict copy of two Craft saves: pc {on_pc:?}, laptop {on_laptop:?}");
        if let Some(f) = up().await
            && f.save.is_some()
            && f.size == pc_saved.len() as u64
            && on_pc == on_laptop
        {
            break;
        }
        assert!(Instant::now() < deadline, "never settled: pc {on_pc:?}, laptop {on_laptop:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop((a, b));
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}
