//! Convergence simulation for documents of files in a team space: three accounts, each with a machine whose agent is the
//! live [`Replica`] over a store that outlasts its crashes, against the server's own live [`Session`]s in a team space over
//! the real metadata database (SQLite in memory). `ana` owns the space, `ben` is an editor, `carl` a viewer. The connections
//! are queues of frames, delivered in order but at random moments (live protocol 3: `welcome_space`, `presence_from`).
//!
//! At random: the editors type into the document and set keys of its map (each edit a token of its own), the viewer tries to
//! (refused where an agent refuses: nothing it types is kept or sent), frames are delivered either way, connections drop and
//! machines crash and restart from their store, and the owner changes `ben`'s role (to viewer and back: the server ends his
//! connection with `access_changed`, he connects again and gets the role he has now) or removes him and invites him again
//! (his agent stops meanwhile; what he had not got acked stays in his journal). Then everyone comes back as they should be
//! (`ben` an editor again) and everything is delivered until nothing moves. Checks:
//!
//! 1. **Convergence**: every machine, the viewer's too, holds the same document, and so does every app.
//! 2. **Nothing kept is lost**: every edit an agent took from its app is in it, also one that waited in a journal through
//!    a lowered role or a removal.
//! 3. **Nothing waits**: no machine has a change the server didn't ack.
//! 4. **A viewer writes nothing**: nothing typed while the agent's role was viewer is in the document, and no write frame of
//!    an agent in that state ever reached the server.
//!
//! A seed replays exactly: `SIM_SEED=17 cargo test -p craft-cloud-agent --test documents_team_sim`; `SIM_SEEDS=2000` runs many.

#[path = "docs_common/mod.rs"]
mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::{App, World};
use craft_cloud_agent::live::replica::{DocEvent, Opening};
use craft_cloud_meta::SpaceActor;
use craft_cloud_proto::DocId;
use craft_cloud_proto::api::SpaceRole;
use craft_cloud_proto::api::codes;
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

fn is_write(frame: &ClientFrame) -> bool {
    matches!(frame, ClientFrame::Create { .. } | ClientFrame::Update { .. } | ClientFrame::Snapshot { .. } | ClientFrame::CreateDocument { .. })
}

/// One machine's app: its own copy of the document, while it has it open.
struct AppState {
    app: Option<App>,
    opening: bool,
    peers: u64,
}

const ANA: usize = 0;
const BEN: usize = 1;
const CARL: usize = 2;
const MACHINES: usize = 3;
const NAMES: [&str; MACHINES] = ["ana", "ben", "carl"];

struct Sim {
    w: World,
    rng: Rng,
    doc: Option<DocId>,
    node: craft_cloud_proto::NodeId,
    apps: Vec<AppState>,
    /// Every edit an agent took: its token.
    taken: BTreeSet<String>,
    /// Every edit an agent refused (a viewer's): its token.
    refused: BTreeSet<String>,
    edits: u64,
    trace: bool,
    log: Vec<String>,
    /// Ben is removed from the space (his agent is stopped) until the owner invites him again.
    ben_removed: bool,
    /// `read_only` errors the server sent an agent (must stay zero: an agent that knows it may not write sends nothing, and
    /// one whose role changed meanwhile is told `access_changed` first).
    read_only_errors: u64,
}

impl Sim {
    fn say(&mut self, line: String) {
        if self.trace {
            eprintln!("{line}");
        }
        self.log.push(line);
    }

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
                    self.say(format!("{} imports", NAMES[m]));
                    let snapshot = App::new(9_000 + m as u64).with_text("start;").snapshot();
                    if let Ok(frames) = self.w.machines[m].replica().import_document(doc, &snapshot) {
                        self.w.send(m, frames);
                    }
                }
                DocEvent::NotLive(..) => {
                    self.say(format!("{} is not live for now", NAMES[m]));
                    self.apps[m].opening = false;
                }
                DocEvent::Remote(_, bytes) => {
                    if let Some(app) = &self.apps[m].app {
                        app.doc.import(&bytes).unwrap();
                    }
                }
                DocEvent::Presence(..) | DocEvent::PresenceFrom(..) => {}
            }
        }
    }

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

    fn open(&mut self, m: usize) {
        if self.apps[m].app.is_some() || self.apps[m].opening || (m == BEN && self.ben_removed) {
            return;
        }
        let (doc, opening) = self.w.open(m, self.node);
        match opening {
            Opening::Live => {
                self.say(format!("{} opens it from its copy", NAMES[m]));
                self.open_app(m, doc);
            }
            Opening::Joining => {
                self.say(format!("{} joins it", NAMES[m]));
                self.apps[m].opening = true;
            }
            Opening::NotLive(..) => self.say(format!("{} can't open it now", NAMES[m])),
        }
    }

    /// An edit by the machine's app. The agent's guard (the hub's): a read-only space refuses it, and nothing is kept.
    fn edit(&mut self, m: usize) {
        let Some(doc) = self.doc else { return };
        let Some(app) = &self.apps[m].app else { return };
        self.edits += 1;
        let token = format!("{}e{};", NAMES[m], self.edits);
        if self.w.machines[m].replica().read_only() {
            self.say(format!("{}'s edit {token} is refused: read-only", NAMES[m]));
            self.refused.insert(token);
            return;
        }
        let update = if self.rng.chance(70) {
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
                self.say(format!("{} edits {token} ({} frames)", NAMES[m], frames.len()));
                self.w.send(m, frames);
            }
            Err(e) => {
                self.say(format!("{}'s edit refused: {e}", NAMES[m]));
                self.close_app(m);
            }
        }
        self.events(m);
    }

    /// The owner changes ben's role.
    async fn flip_ben(&mut self) {
        if self.ben_removed {
            return;
        }
        let (space, _) = self.w.team.unwrap();
        let ben = self.w.machines[BEN].auth.user;
        let now = self.w.meta.space_access(ben, space).await.unwrap();
        let role = if now == Some(SpaceRole::Editor) { SpaceRole::Viewer } else { SpaceRole::Editor };
        self.say(format!("ana makes ben a {}", role.as_str()));
        self.w.meta.change_member_role(&SpaceActor::Cli, space, ben, role, 1).await.unwrap();
        // His agent's list of spaces may say so already (or not yet: then the server's `access_changed` tells it).
        if self.rng.chance(50)
            && let Some(replica) = self.w.machines[BEN].replica.as_mut()
        {
            replica.set_read_only(role == SpaceRole::Viewer);
        }
    }

    /// The owner removes ben (his agent stops: its hub is dropped), or invites him again (it starts from its store).
    async fn ben_leaves_or_returns(&mut self) {
        let (space, _) = self.w.team.unwrap();
        let ben = self.w.machines[BEN].auth.user;
        if !self.ben_removed {
            self.say("ana removes ben; his agent stops".into());
            self.w.meta.remove_member(&SpaceActor::Cli, space, ben, 2).await.unwrap();
            self.ben_removed = true;
            self.close_app(BEN);
            self.apps[BEN].opening = false;
            // Its hub is dropped: what it wrote is on disk, its connection is gone.
            self.w.stop(BEN);
        } else {
            let role = if self.rng.chance(50) { SpaceRole::Editor } else { SpaceRole::Viewer };
            self.say(format!("ana invites ben again as a {}", role.as_str()));
            self.w.meta.invite_member(&SpaceActor::Cli, space, ben, role, 3).await.unwrap();
            self.ben_removed = false;
        }
    }

    async fn step(&mut self) {
        let m = self.rng.below(MACHINES);
        let gone = m == BEN && self.ben_removed;
        match self.rng.below(100) {
            0..=24 if !gone => self.edit(m),
            25..=44 if !gone => {
                // One frame up (the snapshot it may name goes up before it, as the hub sends it).
                let api = self.w.blobs_of(m);
                let machine = &mut self.w.machines[m];
                if let Some(replica) = machine.replica.as_mut()
                    && machine.conn.is_some()
                {
                    for up in replica.take_uploads() {
                        craft_cloud_agent::live::blobs::upload(&api, &up).await.unwrap();
                    }
                }
                if let (Some(conn), Some(frame)) = (machine.conn.as_mut(), machine.up.pop_front()) {
                    // The agent's guard: nothing that writes goes to a space the account may not change.
                    let held_back = machine.replica.as_ref().is_some_and(|r| r.read_only() && is_write(&frame));
                    if !held_back {
                        let reply = conn.session.handle(frame).await;
                        machine.down.extend(reply.frames);
                    }
                }
            }
            45..=69 if !gone => {
                let machine = &mut self.w.machines[m];
                if let (Some(frame), Some(replica)) = (machine.down.pop_front(), machine.replica.as_mut()) {
                    if matches!(&frame, ServerFrame::Error { code, .. } if code == codes::READ_ONLY) {
                        self.read_only_errors += 1;
                    }
                    match replica.on_frame(frame) {
                        Ok(frames) => {
                            let frames = replica.gate(frames);
                            machine.up.extend(frames);
                            for (doc, manifest) in replica.take_fetches() {
                                let got = craft_cloud_agent::live::blobs::download(&self.w.blobs_of(m), &manifest).await.map_err(|e| e.to_string());
                                let machine = &mut self.w.machines[m];
                                if let Some(replica) = machine.replica.as_mut() {
                                    let frames = replica.on_snapshot(doc, manifest, got);
                                    let frames = replica.gate(frames);
                                    machine.up.extend(frames);
                                }
                            }
                        }
                        Err(e) => {
                            self.say(format!("{}'s link ends: {e}", NAMES[m]));
                            self.w.disconnect(m);
                        }
                    }
                    self.after(m);
                }
            }
            70..=79 if !gone => {
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
                        machine.down.extend(frames);
                    }
                    // The ping: the server re-reads the account's access.
                    if self.rng.chance(30)
                        && let Some(ended) = conn.session.check().await
                    {
                        machine.down.push_back(ended);
                    }
                }
            }
            80..=84 if !gone => {
                if self.w.machines[m].conn.is_some() {
                    let frames = self.w.machines[m].replica().resend();
                    self.w.send(m, frames);
                    self.after(m);
                }
            }
            85..=87 if !gone => {
                if self.w.machines[m].conn.is_some() {
                    self.say(format!("{} drops its connection", NAMES[m]));
                    self.w.disconnect(m);
                    self.after(m);
                } else {
                    self.say(format!("{} connects", NAMES[m]));
                    self.w.connect(m);
                }
            }
            88 if !gone => {
                self.say(format!("{} crashes", NAMES[m]));
                self.close_app(m);
                self.apps[m].opening = false;
                self.w.crash(m);
                self.lock(m, m == CARL);
            }
            89 if !gone => {
                if let Some(replica) = self.w.machines[m].replica.as_mut() {
                    replica.flush_documents().unwrap();
                }
            }
            90..=93 => {
                if self.apps[m].app.is_some() && self.rng.chance(30) {
                    self.close_app(m);
                } else {
                    self.open(m);
                }
                self.after(m);
            }
            94..=95 if !gone => {
                if let Some(doc) = self.doc
                    && self.apps[m].app.is_some()
                {
                    let frames = self.w.machines[m].replica().presence(doc, b"cursor");
                    self.w.send(m, frames);
                }
            }
            96..=97 => self.flip_ben().await,
            98 if self.rng.chance(50) => self.ben_leaves_or_returns().await,
            _ => {}
        }
    }

    fn after(&mut self, m: usize) {
        if let Some(replica) = self.w.machines[m].replica.as_mut() {
            replica.flush().unwrap();
            let notes = replica.take_notes();
            self.w.machines[m].said.extend(notes);
        }
        self.events(m);
    }

    /// What the agent's list of spaces says of the account's role: the viewer's agent is told before its first write.
    fn lock(&mut self, m: usize, read_only: bool) {
        if let Some(replica) = self.w.machines[m].replica.as_mut() {
            replica.set_read_only(read_only);
        }
    }

    /// Everyone comes back as they should be, and everything is delivered until nothing moves.
    async fn finish(&mut self) {
        let (space, _) = self.w.team.unwrap();
        let ben = self.w.machines[BEN].auth.user;
        if self.ben_removed {
            self.w.meta.invite_member(&SpaceActor::Cli, space, ben, SpaceRole::Editor, 4).await.unwrap();
            self.ben_removed = false;
        } else if self.w.meta.space_access(ben, space).await.unwrap() != Some(SpaceRole::Editor) {
            self.w.meta.change_member_role(&SpaceActor::Cli, space, ben, SpaceRole::Editor, 5).await.unwrap();
        }
        for m in 0..MACHINES {
            // A connection that was opened as another role ends (the server's word), and is made again.
            if self.w.machines[m].conn.is_some() {
                self.w.disconnect(m);
            }
            if self.w.machines[m].replica.is_none() {
                self.w.crash(m);
            }
            // The agents' lists of spaces are right by now.
            self.lock(m, m == CARL);
            self.w.connect(m);
        }
        for round in 0..300 {
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
    w.team().await;
    assert_eq!(w.owner_machine().await, ANA);
    assert_eq!(w.member("ben", SpaceRole::Editor).await, BEN);
    assert_eq!(w.member("carl", SpaceRole::Viewer).await, CARL);
    let node = w.team_file().await;
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
        refused: BTreeSet::new(),
        edits: 0,
        trace,
        log: Vec::new(),
        ben_removed: false,
        read_only_errors: 0,
    };
    sim.lock(CARL, true);
    sim.say(format!("seed {seed}: compacting past {compact_updates} updates, asks timing out {compact_timeout:?}"));
    sim.open(ANA);
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
    let first = sim.w.app_of(ANA, doc, 1).doc.get_deep_value();
    #[allow(clippy::needless_range_loop)]
    for m in 0..MACHINES {
        assert!(sim.w.machines[m].replica().has_document(doc), "seed {seed}: {} never joined: {:?}", NAMES[m], sim.w.machines[m].said);
        let held = sim.w.app_of(m, doc, 1).doc.get_deep_value();
        assert_eq!(held, first, "seed {seed}: {} holds another document; it said {:?}", NAMES[m], sim.w.machines[m].said);
        let app = sim.apps[m].app.as_ref().unwrap_or_else(|| panic!("seed {seed}: {}'s app has nothing open", NAMES[m]));
        assert_eq!(app.doc.get_deep_value(), first, "seed {seed}: {}'s app shows another document", NAMES[m]);
        let (_, waiting) = sim.w.machines[m].replica().document_status(doc).unwrap();
        assert_eq!(waiting, 0, "seed {seed}: {} has {waiting} change(s) the server never acked", NAMES[m]);
    }
    let view = sim.w.app_of(ANA, doc, 1);
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
    for token in &sim.refused {
        assert!(
            !text.contains(token.as_str()) && !keys.contains(token),
            "seed {seed}: the edit {token} was refused to a viewer and is in the document"
        );
    }
    assert_eq!(sim.read_only_errors, 0, "seed {seed}: the server refused a write an agent should not have sent");
    // Nothing the viewer typed is on the server: its tokens are all refused ones.
    assert!(!text.contains("carle"), "seed {seed}: the viewer's edit is in the document");
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
fn team_documents_converge_with_two_accounts_and_a_viewer() {
    for_each_seed(30, |seed| {
        block_on(run(seed));
    });
}

#[test]
fn a_team_seed_replays_exactly() {
    for seed in 0..6 {
        let (a, b) = (block_on(run(seed)), block_on(run(seed)));
        assert_eq!(a, b, "seed {seed} ran two ways");
    }
}
