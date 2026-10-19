//! Convergence simulation for documents of files: machines of one account, each the agent's own live [`Replica`] over a
//! store that outlasts its crashes, with an app stand-in editing one document (a plain text and a map) through it,
//! against the server's own live [`Session`]s over the real metadata database (SQLite in memory). The connections are
//! queues of frames, delivered in order but at random moments, so every interleaving of machines, catch-ups, fan-out
//! and compaction can come up.
//!
//! At random: apps type into the text and set keys of the map (each edit a token of its own), frames are delivered
//! either way, the server pumps a connection's log, connections drop (with whatever was in flight lost both ways) and
//! come back, machines crash (losing what they hadn't written, never what they told their app they took) and restart
//! from their store, documents are written now and then, apps close the document and open it again, presence goes
//! out, and the server's epoch is reset now and then (as after a restore). The log is compacted at random: a low limit
//! per seed, and requests that time out at once or never. Then every machine comes back, and everything is delivered
//! until nothing moves. Checks:
//!
//! 1. **Convergence**: every machine holds the same document, and so does every app (its own copy, built from the
//!    state it opened with and the changes it was handed).
//! 2. **Nothing kept is lost**: every edit an agent took from its app (on its disk before the app heard so) is in it.
//! 3. **Nothing waits**: no machine has a change the server didn't ack.
//!
//! A seed replays exactly: `SIM_SEED=17 cargo test -p craft-cloud-agent --test documents_sim`; `SIM_SEEDS=10000
//! SIM_FIRST=0` runs many (use `--release`), on `SIM_JOBS` workers. `SIM_TRACE=1` prints every step.

#[path = "docs_common/mod.rs"]
mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::{App, World};
use craft_cloud_agent::live::replica::{DocEvent, Opening};
use craft_cloud_proto::DocId;
use craft_cloud_proto::live::{ClientFrame, ServerFrame};
use craft_cloud_server::state::Limits;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// A frame as the replay log has it: what it is, without IDs (they are made afresh by every run).
fn up_text(frame: &ClientFrame) -> String {
    match frame {
        ClientFrame::Hello { .. } => "hello".into(),
        ClientFrame::Create { .. } => "create".into(),
        ClientFrame::Subscribe { after, .. } => format!("subscribe after {after}"),
        ClientFrame::Unsubscribe { .. } => "unsubscribe".into(),
        ClientFrame::Update { client_seq, bytes, .. } => format!("update {client_seq} ({} bytes)", bytes.len()),
        ClientFrame::Snapshot { upto, .. } => format!("snapshot upto {upto}"),
        ClientFrame::Presence { .. } => "presence".into(),
        ClientFrame::SubscribeLog { after, .. } => format!("subscribe the log after {after}"),
        ClientFrame::CreateDocument { snapshot, .. } => format!("create document ({} bytes)", snapshot.len()),
    }
}

fn down_text(frame: &ServerFrame) -> String {
    match frame {
        ServerFrame::Welcome { protocol, .. } => format!("welcome {protocol}"),
        ServerFrame::Created { .. } => "created".into(),
        ServerFrame::Update { seq, bytes, .. } => format!("update seq {seq} ({} bytes)", bytes.len()),
        ServerFrame::Subscribed { head, .. } => format!("subscribed head {head}"),
        ServerFrame::Ack { client_seq, seq, .. } => format!("ack {client_seq} as {seq}"),
        ServerFrame::Error { code, client_seq, .. } => format!("error {code} {client_seq:?}"),
        ServerFrame::Compact { upto, .. } => format!("compact upto {upto}"),
        ServerFrame::Snapshot { base, .. } => format!("snapshot base {base}"),
        ServerFrame::Presence { .. } => "presence".into(),
        ServerFrame::CreatedDocument { kept, .. } => format!("created, kept {kept}"),
    }
}

/// One machine's app: its own copy of the document, while it has it open.
struct AppState {
    app: Option<App>,
    /// Waiting for the document to be joined (it asked to open it).
    opening: bool,
    peers: u64,
}

struct Sim {
    w: World,
    rng: Rng,
    doc: Option<DocId>,
    node: craft_cloud_proto::NodeId,
    apps: Vec<AppState>,
    /// Every edit an agent took: its token.
    taken: BTreeSet<String>,
    edits: u64,
    log: Vec<String>,
    trace: bool,
    /// The last update or ack each machine was sent (to send again).
    last_down: Vec<Option<ServerFrame>>,
    /// A backup of the document's log: its head, base and snapshot.
    backup: Option<(u64, Option<(u64, craft_cloud_meta::SnapshotRef)>)>,
}

const MACHINES: usize = 3;

impl Sim {
    fn say(&mut self, line: String) {
        if self.trace {
            eprintln!("{line}");
        }
        self.log.push(line);
    }

    /// Hands the machine's events to its app.
    fn events(&mut self, m: usize) {
        let events = self.w.events(m);
        for event in events {
            match event {
                DocEvent::Joined(doc) => {
                    self.doc = Some(doc);
                    if self.apps[m].opening {
                        self.apps[m].opening = false;
                        self.open_app(m, doc);
                    }
                }
                DocEvent::NeedsImport(doc) => {
                    self.say(format!("m{m} imports"));
                    let snapshot = App::new(9_000 + m as u64).with_text("start;").snapshot();
                    if let Ok(frames) = self.w.machines[m].replica().import_document(doc, &snapshot) {
                        self.w.send(m, frames);
                    }
                }
                DocEvent::NotLive(..) => {
                    self.say(format!("m{m} not live for now"));
                    self.apps[m].opening = false;
                }
                DocEvent::Remote(_, bytes) => {
                    if let Some(app) = &self.apps[m].app {
                        // (Loro takes updates in any order; one that waits for its history applies when it comes.)
                        app.doc.import(&bytes).unwrap();
                    }
                }
                DocEvent::Presence(..) => {}
            }
        }
    }

    /// The machine's app opens the document as its agent has it now.
    fn open_app(&mut self, m: usize, doc: DocId) {
        self.apps[m].peers += 1;
        let peer = ((m as u64 + 1) << 32) + self.apps[m].peers;
        let app = self.w.app_of(m, doc, peer);
        self.w.machines[m].replica().set_open(doc, true);
        self.apps[m].app = Some(app);
    }

    fn close_app(&mut self, m: usize) {
        if self.apps[m].app.take().is_some()
            && let Some(doc) = self.doc
            && let Some(replica) = self.w.machines[m].replica.as_mut()
        {
            replica.set_open(doc, false);
        }
    }

    /// The machine's app opens the file.
    fn open(&mut self, m: usize) {
        if self.apps[m].app.is_some() || self.apps[m].opening {
            return;
        }
        let (doc, opening) = self.w.open(m, self.node);
        match opening {
            Opening::Live => {
                self.say(format!("m{m} opens it from its copy"));
                self.open_app(m, doc);
            }
            Opening::Joining => {
                self.say(format!("m{m} joins it"));
                self.apps[m].opening = true;
            }
            Opening::NotLive(..) => self.say(format!("m{m} can't open it now")),
        }
    }

    /// An edit by the machine's app: a token typed into the text, or a key of the map.
    fn edit(&mut self, m: usize) {
        let Some(doc) = self.doc else { return };
        let Some(app) = &self.apps[m].app else { return };
        self.edits += 1;
        let token = format!("m{m}e{};", self.edits);
        let update = if self.rng.chance(70) {
            // Between tokens (every token ends with `;`), so none is cut in two.
            let text = app.doc.get_text("text").to_string();
            let mut places = vec![0];
            places.extend(text.chars().enumerate().filter(|(_, c)| *c == ';').map(|(i, _)| i + 1));
            let at = places[self.rng.below(places.len())];
            app.type_at(at, &token)
        } else {
            app.set(&token, self.edits as i64)
        };
        match self.w.machines[m].replica().local_update(doc, &update) {
            Ok(frames) => {
                self.taken.insert(token.clone());
                self.say(format!("m{m} edits {token} ({} frames)", frames.len()));
                self.w.send(m, frames);
            }
            Err(e) => {
                // Not taken: the app's own copy has it, the agent's doesn't. It opens the document again.
                self.say(format!("m{m}'s edit refused: {e}"));
                self.close_app(m);
            }
        }
        self.events(m);
    }

    /// One random step.
    async fn step(&mut self) {
        let m = self.rng.below(MACHINES);
        match self.rng.below(100) {
            0..=24 => self.edit(m),
            25..=44 => {
                // One frame up.
                let machine = &mut self.w.machines[m];
                if let (Some(conn), Some(frame)) = (machine.conn.as_mut(), machine.up.pop_front()) {
                    let mut text = up_text(&frame);
                    if self.trace
                        && let ClientFrame::Snapshot { bytes, .. } = &frame
                    {
                        text.push_str(&format!(" {:?}", loro::LoroDoc::decode_import_blob_meta(bytes, true).map(|m| m.partial_end_vv)));
                    }
                    let reply = conn.session.handle(frame).await;
                    machine.down.extend(reply.frames);
                    self.say(format!("m{m} up {text}"));
                }
            }
            45..=69 => {
                // One frame down.
                let machine = &mut self.w.machines[m];
                if let (Some(frame), Some(replica)) = (machine.down.pop_front(), machine.replica.as_mut()) {
                    let text = down_text(&frame);
                    self.last_down[m] = matches!(frame, ServerFrame::Update { .. } | ServerFrame::Ack { .. }).then(|| frame.clone());
                    match replica.on_frame(frame) {
                        Ok(frames) => machine.up.extend(frames),
                        Err(e) => {
                            self.say(format!("m{m} link ends: {e}"));
                            self.w.disconnect(m);
                        }
                    }
                    self.say(format!("m{m} down {text}"));
                    self.after(m);
                }
            }
            70..=79 => {
                // The server hears of its space and pumps a page.
                let machine = &mut self.w.machines[m];
                if let Some(conn) = machine.conn.as_mut() {
                    while let Ok(doc) = conn.notes.try_recv() {
                        conn.session.note(doc);
                    }
                    while let Ok(note) = conn.presence.try_recv() {
                        machine.down.extend(conn.session.relay(&note));
                    }
                    if conn.session.has_work() {
                        let frames = conn.session.pump().await.unwrap();
                        let n = frames.len();
                        machine.down.extend(frames);
                        self.say(format!("m{m} pump {n}"));
                    }
                }
            }
            80..=84 => {
                // A tick of the agent.
                if self.w.machines[m].conn.is_some() {
                    let frames = self.w.machines[m].replica().resend();
                    self.w.send(m, frames);
                    self.after(m);
                }
            }
            85..=86 => {
                if self.w.machines[m].conn.is_some() {
                    self.say(format!("m{m} drops its connection"));
                    self.w.disconnect(m);
                    self.after(m);
                } else {
                    self.say(format!("m{m} connects"));
                    self.w.connect(m);
                }
            }
            87 => {
                self.say(format!("m{m} crashes"));
                self.close_app(m);
                self.apps[m].opening = false;
                self.w.crash(m);
            }
            88..=89 => {
                // Time passes: documents are written whole.
                if let Some(replica) = self.w.machines[m].replica.as_mut() {
                    replica.flush_documents().unwrap();
                }
            }
            90..=93 => {
                if self.apps[m].app.is_some() && self.rng.chance(30) {
                    self.say(format!("m{m}'s app closes it"));
                    self.close_app(m);
                } else {
                    self.open(m);
                }
                self.after(m);
            }
            94..=95 => {
                if let Some(doc) = self.doc
                    && self.apps[m].app.is_some()
                {
                    let frames = self.w.machines[m].replica().presence(doc, b"cursor");
                    self.w.send(m, frames);
                }
            }
            96 => {
                if let Some(doc) = self.doc
                    && self.rng.chance(30)
                {
                    // A backup of the database now (the log's head, its base and snapshot).
                    if let Some(found) = self.w.meta.live_doc(self.w.space, doc).await.unwrap() {
                        let snapshot = self.w.meta.live_snapshot(self.w.space, doc).await.unwrap();
                        self.say(format!("a backup at head {}", found.head));
                        self.backup = Some((found.head, snapshot));
                    }
                } else if self.rng.chance(20) {
                    // A restore from the backup (if there is one), and its step: the epoch is reset; every connection
                    // ends at its next ping.
                    if let (Some(doc), Some((head, snapshot))) = (self.doc, self.backup.take()) {
                        self.say(format!("the server is restored to head {head}"));
                        self.w.meta.restore_live_log(self.w.space, doc, Some(head)).await.unwrap();
                        self.w.meta.restore_live_snapshot(self.w.space, doc, snapshot).await.unwrap();
                    }
                    self.say("the server's epoch is reset".into());
                    self.w.meta.reset_epoch(self.w.space).await.unwrap();
                    for i in 0..MACHINES {
                        if self.w.machines[i].conn.is_some() {
                            self.w.disconnect(i);
                            self.after(i);
                        }
                    }
                }
            }
            _ => {
                // An update or ack the server sent comes twice (as from a connection given up on whose last frames
                // still arrive).
                let machine = &mut self.w.machines[m];
                if let (Some(frame), Some(replica)) = (self.last_down[m].clone(), machine.replica.as_mut())
                    && machine.conn.is_some()
                {
                    let text = down_text(&frame);
                    if let Ok(frames) = replica.on_frame(frame) {
                        machine.up.extend(frames);
                    }
                    self.say(format!("m{m} again {text}"));
                    self.after(m);
                }
            }
        }
    }

    /// After the replica did something: writes what it must, says its notes, hands its events to the app.
    fn after(&mut self, m: usize) {
        if let Some(replica) = self.w.machines[m].replica.as_mut() {
            replica.flush().unwrap();
            let notes = replica.take_notes();
            self.w.machines[m].said.extend(notes);
        }
        self.events(m);
    }

    /// What the server's log and snapshot hold of the document, in words (for a failure to say).
    async fn server_version(&mut self, doc: DocId) -> String {
        let (space, meta) = (self.w.space, self.w.meta.clone());
        let rows = meta.live_updates(space, doc, 0, 1000).await.unwrap_or_default();
        let found = meta.live_doc(space, doc).await.ok().flatten();
        let mut ranges = Vec::new();
        for row in &rows {
            let d = loro::LoroDoc::decode_import_blob_meta(&row.bytes, true).unwrap();
            ranges.push((row.seq, format!("{:?}", d.partial_end_vv)));
        }
        format!("{found:?}; rows {ranges:?}")
    }

    /// Everything comes back and is delivered until nothing moves.
    async fn finish(&mut self) {
        for m in 0..MACHINES {
            if self.w.machines[m].conn.is_none() {
                self.w.connect(m);
            }
        }
        for round in 0..200 {
            self.w.settle().await;
            for m in 0..MACHINES {
                self.events(m);
                if self.apps[m].app.is_none() && !self.apps[m].opening {
                    self.open(m);
                }
                self.events(m);
            }
            let all_open = self.apps.iter().all(|a| a.app.is_some());
            let quiet = self.w.machines.iter_mut().all(|machine| machine.up.is_empty() && machine.down.is_empty());
            if all_open && quiet && round > 2 {
                let waiting: u64 =
                    (0..MACHINES).map(|m| self.doc.and_then(|d| self.w.machines[m].replica().document_status(d)).map_or(1, |s| s.1)).sum();
                if waiting == 0 {
                    return;
                }
            }
            for m in 0..MACHINES {
                let frames = self.w.machines[m].replica().resend();
                self.w.send(m, frames);
            }
        }
    }
}

async fn run(seed: u64) -> Vec<String> {
    let mut rng = Rng::new(seed);
    let compact_updates = 2 + rng.below(30) as u64;
    let compact_timeout = if rng.chance(50) { Duration::ZERO } else { Duration::from_secs(3600) };
    let limits =
        Limits { compact_updates, compact_timeout, live_burst: 1_000_000, live_per_sec: 1_000_000, presence_per_sec: 1_000_000, ..Limits::default() };
    let mut w = World::new(limits).await;
    for _ in 0..MACHINES {
        w.machine().await;
    }
    let node = w.file().await;
    for m in 0..MACHINES {
        w.connect(m);
    }
    let trace = std::env::var("SIM_TRACE").is_ok();
    let apps = (0..MACHINES).map(|_| AppState { app: None, opening: false, peers: 0 }).collect();
    let mut sim = Sim {
        w,
        rng,
        doc: None,
        node,
        apps,
        taken: BTreeSet::new(),
        edits: 0,
        log: Vec::new(),
        trace,
        last_down: vec![None; MACHINES],
        backup: None,
    };
    sim.say(format!("seed {seed}: compacting past {compact_updates} updates, asks timing out {compact_timeout:?}"));
    sim.open(0);
    let steps = 300 + sim.rng.below(900);
    for _ in 0..steps {
        sim.step().await;
    }
    sim.finish().await;
    check(&mut sim, seed).await;
    sim.log
}

async fn check(sim: &mut Sim, seed: u64) {
    let doc = sim.doc.unwrap_or_else(|| panic!("seed {seed}: the document was never made"));
    let first = sim.w.app_of(0, doc, 1).doc.get_deep_value();
    for m in 0..MACHINES {
        assert!(sim.w.machines[m].replica().has_document(doc), "seed {seed}: m{m} never joined: {:?}", sim.w.machines[m].said);
        let held = sim.w.app_of(m, doc, 1).doc.get_deep_value();
        if held != first {
            let heard: Vec<String> = sim.w.machines[m].heard.iter().rev().take(40).rev().map(down_text).collect();
            let sent: Vec<String> = sim.w.machines[m].said_up.iter().rev().take(40).rev().map(up_text).collect();
            let said = sim.w.machines[m].said.clone();
            let status = sim.w.machines[m].replica().document_status(doc);
            let waiting = sim.w.machines[m].replica().waiting_bytes();
            let vv = |sim: &mut Sim, i: usize| {
                let snapshot = sim.w.machines[i].replica().snapshot_of(doc).unwrap();
                let d = loro::LoroDoc::new();
                d.import(&snapshot).unwrap();
                d.oplog_vv()
            };
            let (mine, theirs) = (vv(sim, m), vv(sim, 0));
            // What the server holds: every row, and its snapshot.
            let server = sim.server_version(doc).await;
            panic!(
                "seed {seed}: m{m} holds another document; it said {said:?}; heard last {heard:?}; sent last {sent:?}; status {status:?}; waiting {waiting} bytes; vv {mine:?} vs m0 {theirs:?}; the server: {server}"
            );
        }
        let app = sim.apps[m].app.as_ref().unwrap_or_else(|| panic!("seed {seed}: m{m}'s app has nothing open"));
        if app.doc.get_deep_value() != first {
            let shown = format!("{:?}", app.doc.get_deep_value());
            let missing: Vec<&String> = sim.taken.iter().filter(|t| !shown.contains(t.as_str())).collect();
            panic!("seed {seed}: m{m}'s app shows another document: it lacks {missing:?}; m{m} said {:?}", sim.w.machines[m].said);
        }
        let (_, waiting) = sim.w.machines[m].replica().document_status(doc).unwrap();
        assert_eq!(waiting, 0, "seed {seed}: m{m} has {waiting} change(s) the server never acked");
    }
    let view = sim.w.app_of(0, doc, 1);
    let text = view.text();
    let keys: BTreeSet<String> = {
        let mut keys = BTreeSet::new();
        view.doc.get_map("map").for_each(|k, _| {
            keys.insert(k.to_owned());
        });
        keys
    };
    for token in &sim.taken {
        assert!(text.contains(token.as_str()) || keys.contains(token), "seed {seed}: the edit {token} was taken and is lost");
    }
    // A machine that never had it joins now, from the server's last snapshot and the rows above it: the same
    // document (a snapshot that left out a row below its base would show here).
    let fresh = sim.w.machine().await;
    sim.w.connect(fresh);
    sim.w.settle().await;
    let (_, opening) = sim.w.open(fresh, sim.node);
    assert_eq!(opening, Opening::Joining, "seed {seed}");
    sim.w.settle().await;
    let joined = sim.w.events(fresh);
    assert!(joined.contains(&DocEvent::Joined(doc)), "seed {seed}: the fresh machine didn't join: {joined:?}");
    let late = sim.w.app_of(fresh, doc, 1).doc.get_deep_value();
    assert_eq!(late, first, "seed {seed}: a machine joining at the end holds another document; the server: {}", sim.server_version(doc).await);
}

fn for_each_seed(default_seeds: u64, run: impl Fn(u64) + Sync) {
    if let Ok(seed) = std::env::var("SIM_SEED") {
        run(seed.parse().unwrap());
        return;
    }
    let seeds: u64 = std::env::var("SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(default_seeds);
    let first: u64 = std::env::var("SIM_FIRST").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let jobs: usize = std::env::var("SIM_JOBS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(8)))
        .max(1);
    let next = AtomicU64::new(first);
    let failures: std::sync::Mutex<Vec<(u64, String)>> = std::sync::Mutex::default();
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let seed = next.fetch_add(1, Ordering::Relaxed);
                    if seed >= first + seeds {
                        return;
                    }
                    if std::env::var("SIM_SAY").is_ok() {
                        eprintln!("start {seed}");
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(seed)));
                    if let Err(payload) = result {
                        let message = payload.downcast_ref::<String>().cloned().or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()));
                        failures.lock().unwrap().push((seed, message.unwrap_or_default()));
                    }
                }
            });
        }
    });
    let mut failures = failures.into_inner().unwrap();
    failures.sort();
    if let Some((seed, message)) = failures.first() {
        let others: Vec<String> = failures.iter().skip(1).map(|(s, _)| s.to_string()).collect();
        panic!("seed {seed}: {message}{}", if others.is_empty() { String::new() } else { format!("\n(seeds {} failed too)", others.join(", ")) });
    }
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

#[test]
fn documents_converge_without_losing_an_edit() {
    for_each_seed(30, |seed| {
        block_on(run(seed));
    });
}

#[test]
fn a_seed_replays_exactly() {
    for seed in 0..10 {
        let (a, b) = (block_on(run(seed)), block_on(run(seed)));
        assert_eq!(a, b, "seed {seed} ran two ways");
    }
}
