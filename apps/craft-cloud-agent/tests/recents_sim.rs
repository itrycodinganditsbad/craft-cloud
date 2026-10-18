//! Convergence simulation for recents: three machines of one account, each the agent's own live [`Replica`] over a
//! store that outlasts its crashes, against the server's own live [`Session`]s over the real metadata database
//! (SQLite in memory). The connections are queues of frames, delivered in order but at random moments, so every
//! interleaving of machines, catch-ups and fan-out can come up.
//!
//! At random: apps add entries (two apps, files with a node and files without, clocks a little apart per machine, the
//! same file again), frames are delivered either way, the server pumps a connection's log, connections drop (with
//! whatever was in flight lost both ways) and come back, machines go offline, crash (losing what they had read but not
//! yet written) and restart from their store, and refused updates are sent again. Then every machine comes back,
//! follows every app, and everything is delivered until nothing moves. Checks:
//!
//! 1. **Convergence**: every machine shows the same list for every app.
//! 2. **Nothing kept is lost, nothing else appears**: that list is exactly what the list rules make of every add an
//!    agent accepted (wrote to disk before answering the app), whatever reached the server when: every add the cap
//!    (or a newer add of the same file) didn't push out is in it.
//! 3. **Nothing waits**: no machine has an update the server didn't ack.
//!
//! A seed replays exactly: `SIM_SEED=17 cargo test -p craft-cloud-agent --test recents_sim`; `SIM_SEEDS=10000
//! SIM_FIRST=0` runs many (use `--release`), on `SIM_JOBS` workers. `SIM_TRACE=1` prints every step.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use craft_cloud_agent::live::recents::{AddKey, Recents, view_of};
use craft_cloud_agent::live::replica::{MAX_WAITING_BYTES, Peers, Replica};
use craft_cloud_agent::live::store::MemStore;
use craft_cloud_meta::{DeviceAuth, Meta, Role, UserId};
use craft_cloud_proto::live::{AppName, ClientFrame, DocKind, ServerFrame, recents_doc};
use craft_cloud_proto::local::RecentEntry;
use craft_cloud_proto::{DocId, NodeId, SpaceId};
use craft_cloud_server::AppState;
use craft_cloud_server::live::Session;
use craft_cloud_store::Store;
use craft_cloud_store::memory::MemoryBackend;
use tokio::sync::broadcast;

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
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// An open connection: the server's session for it, what the server hears of its space, and the frames on their way.
struct Conn {
    session: Session,
    notes: broadcast::Receiver<DocId>,
    up: VecDeque<ClientFrame>,
    down: VecDeque<ServerFrame>,
}

struct Machine {
    name: String,
    store: MemStore,
    replica: Option<Replica<MemStore>>,
    conn: Option<Conn>,
    /// A connection the machine gave up on while the server still reads what it had sent (one device, two sockets).
    old: Option<Conn>,
    auth: DeviceAuth,
    /// This machine's clock runs this far from the others'.
    skew: i64,
    /// What its agent told the user.
    said: Vec<String>,
    /// The most bytes of updates it held waiting for their history at any moment.
    most_waiting: usize,
}

/// Another device of the account that sends hostile or odd updates straight to the server: junk bytes, Loro updates
/// with entries that break the rules, a document of a newer version, and honest updates out of their order.
struct Rogue {
    auth: DeviceAuth,
    session: Option<Session>,
    /// Its own Loro documents, one chain per app (entries that depend on its earlier ones).
    chains: Vec<loro::LoroDoc>,
    next_client_seq: u64,
    /// Updates held back to be sent later (after one that depends on them).
    deferred: Vec<(usize, Vec<u8>)>,
    /// Everything it sent, to send again after a restore (as an agent sends its document again).
    sent: Vec<(usize, Vec<u8>)>,
    /// Updates sent on the current session.
    on_session: usize,
}

/// Updates the rogue sends on one session before it opens another (under the frame budget of 400).
const ROGUE_PER_SESSION: usize = 300;

/// Loro peer IDs for one run of a machine, from the seed (so a seed replays exactly): distinct per document and per run.
fn peers(base: u64) -> Peers {
    let mut n = 0u64;
    Box::new(move || {
        n += 1;
        (base ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 1
    })
}

/// A frame as the replay log has it: what it is, without IDs (device IDs are made afresh by every run).
fn up_text(frame: &ClientFrame) -> String {
    match frame {
        ClientFrame::Hello { .. } => "hello".into(),
        ClientFrame::Create { kind, .. } => format!("create {}", kind.as_text()),
        ClientFrame::Subscribe { after, .. } => format!("subscribe after {after}"),
        ClientFrame::Unsubscribe { .. } => "unsubscribe".into(),
        ClientFrame::Update { client_seq, bytes, .. } => format!("update {client_seq} ({} bytes)", bytes.len()),
    }
}

fn down_text(frame: &ServerFrame) -> String {
    match frame {
        ServerFrame::Welcome { protocol, .. } => format!("welcome {protocol}"),
        ServerFrame::Created { .. } => "created".into(),
        ServerFrame::Update { seq, bytes, .. } => format!("update seq {seq} ({} bytes)", bytes.len()),
        ServerFrame::Subscribed { head, .. } => format!("subscribed head {head}"),
        ServerFrame::Ack { client_seq, seq, .. } => format!("ack {client_seq} as {seq}"),
        ServerFrame::Error { code, message, client_seq, .. } => format!("error {code} {client_seq:?}: {message}"),
    }
}

/// The files apps open: some synced (a node, always at the same path), some not yet (a path only).
fn file(i: usize, nodes: &[NodeId]) -> (Option<NodeId>, String) {
    match nodes.get(i) {
        Some(node) => (Some(*node), format!("Docs/file{i}.docx")),
        None => (None, format!("New/unsynced{i}.txt")),
    }
}

const STEPS: usize = 260;
/// How many files there are to open: a few dozen, or (a quarter of the seeds) more than the cap of 100, spread over
/// the machines before they first connect, so their lists only go over the cap once merged and every machine prunes
/// on its own while others add.
const FILES: usize = 40;
const FILES_FULL: usize = 130;

struct Sim {
    rng: Rng,
    state: Arc<AppState>,
    space: SpaceId,
    machines: Vec<Machine>,
    apps: Vec<AppName>,
    nodes: Vec<NodeId>,
    files: usize,
    /// Every add an agent accepted, per app (and the rogue's valid entries).
    accepted: BTreeMap<AppName, Vec<(AddKey, RecentEntry)>>,
    clock: i64,
    trace: bool,
    /// What happened, for the replay check.
    log: Vec<String>,
    rogue: Rogue,
    /// The documents' heads when the last backup was taken.
    backup: Option<Vec<(DocId, u64)>>,
    /// The rogue may send a document of a newer version.
    newer_ok: bool,
    /// Keys of honest entries the rogue wrote over (their entry is whatever the merged document says).
    touched: BTreeMap<AppName, std::collections::BTreeSet<AddKey>>,
    /// The logs went back (a restore) and the epoch isn't reset yet.
    lagging: bool,
    /// The rogue flooded held updates (some stay held for good: they wait for a commit never sent).
    flooded: bool,
    /// The Loro peers of the rogue's chains whose first commit is never sent: only their updates may wait for good.
    orphans: std::collections::BTreeSet<u64>,
    /// The documents' log cap (small in the seeds where documents fill up).
    cap: u64,
    /// The app whose document the rogue held past the cap on waiting updates (it is quiet from then on).
    overflowed: Option<usize>,
    /// The remedy for that was given.
    remedied: bool,
    /// Resets of full documents the admin still makes in the settle.
    resets_left: u32,
    /// The app whose document holds a retired machine's long history.
    retired: Option<usize>,
    /// No rogue in this seed.
    no_rogue: bool,
}

/// Which of the harsher events a run has (all by default; `SIM_ONLY=restore,hostile,…` keeps only some, to see
/// which one a failure needs).
#[derive(Clone, Copy)]
struct Features {
    restore: bool,
    hostile: bool,
    overlap: bool,
    forced: bool,
    /// The rogue writes honest machines' keys: valid entries, and decodable values of the wrong shape.
    shape: bool,
    /// The rogue floods updates that wait for a commit it never sends, past the agent's cap on held updates.
    flood: bool,
    /// Restores where the epoch is reset only a while after the logs went back, with connections open meanwhile.
    lag: bool,
    /// Documents filled past a small cap (`quota_exceeded`), and the admin's reset.
    quota: bool,
    /// One update that never applies, then over ten thousand more.
    stuck: bool,
}

impl Features {
    fn from_env() -> Self {
        match std::env::var("SIM_ONLY") {
            Ok(only) => {
                let has = |name: &str| only.split(',').any(|o| o.trim() == name);
                Self {
                    restore: has("restore"),
                    hostile: has("hostile"),
                    overlap: has("overlap"),
                    forced: has("forced"),
                    shape: has("shape"),
                    flood: has("flood"),
                    lag: has("lag"),
                    quota: has("quota"),
                    stuck: has("stuck"),
                }
            }
            Err(_) => {
                Self { restore: true, hostile: true, overlap: true, forced: true, shape: true, flood: true, lag: true, quota: true, stuck: true }
            }
        }
    }
}

impl Sim {
    fn note(&mut self, line: String) {
        if self.trace {
            eprintln!("{line}");
        }
        self.log.push(line);
    }

    fn alive(&self, m: usize) -> bool {
        self.machines.get(m).is_some_and(|m| m.replica.is_some())
    }

    fn drop_conn(&mut self, m: usize) {
        let machine = &mut self.machines[m];
        if machine.conn.take().is_some()
            && let Some(replica) = machine.replica.as_mut()
        {
            replica.disconnected();
        }
    }

    fn connect(&mut self, m: usize) {
        let state = self.state.clone();
        let machine = &mut self.machines[m];
        let Some(replica) = machine.replica.as_mut() else { return };
        if machine.conn.is_some() {
            return;
        }
        let notes = state.live.listen(machine.auth.space);
        let session = Session::new(state, &machine.auth);
        let up = replica.connected().into();
        machine.conn = Some(Conn { session, notes, up, down: VecDeque::new() });
    }

    fn add(&mut self, m: usize, many: bool) {
        let app = self.rng.below(self.apps.len());
        // A few files are opened over and over, so the same file comes up on several machines at once.
        let i = if many { self.rng.below(self.files) } else { self.rng.below(6) };
        self.add_file(m, app, i);
    }

    fn add_file(&mut self, m: usize, app: usize, i: usize) {
        if !self.alive(m) {
            return;
        }
        let app = self.apps[app].clone();
        let (node, path) = file(i, &self.nodes);
        self.clock += 1 + self.rng.below(3) as i64;
        let machine = &mut self.machines[m];
        let entry = RecentEntry {
            node,
            path: path.clone(),
            title: path.clone(),
            opened_ms: self.clock + machine.skew,
            saved_ms: None,
            machine: machine.name.clone(),
        };
        let mut key = [0u8; 16];
        key[..8].copy_from_slice(&self.rng.next().to_le_bytes());
        key[8..].copy_from_slice(&self.rng.next().to_le_bytes());
        let Some(replica) = machine.replica.as_mut() else { return };
        match replica.add(&app, key, entry.clone()) {
            Ok(frames) => {
                if let Some(conn) = machine.conn.as_mut() {
                    conn.up.extend(frames);
                }
                self.accepted.entry(app.clone()).or_default().push((key, entry));
                self.note(format!("m{m} adds {path} to {app}"));
            }
            // A document a newer agent wrote: this agent doesn't add to it (and the app is told so).
            Err(e) => self.note(format!("m{m} can't add {path} to {app}: {e}")),
        }
    }

    /// Delivers the next frame from machine `m` to the server.
    async fn deliver_up(&mut self, m: usize) {
        let Some(conn) = self.machines[m].conn.as_mut() else { return };
        let Some(frame) = conn.up.pop_front() else { return };
        let text = format!("m{m} -> server {}", up_text(&frame));
        let reply = conn.session.handle(frame).await;
        conn.down.extend(reply.frames);
        let close = reply.close;
        self.note(text);
        if close {
            self.drop_conn(m);
        }
    }

    /// The server sends machine `m` what its log holds that the machine lacks (one page).
    async fn pump(&mut self, m: usize) {
        let Some(conn) = self.machines[m].conn.as_mut() else { return };
        loop {
            match conn.notes.try_recv() {
                Ok(doc) => conn.session.note(doc),
                Err(broadcast::error::TryRecvError::Lagged(_)) => conn.session.lagged(),
                Err(_) => break,
            }
        }
        if conn.session.has_work() {
            let frames = conn.session.pump().await.unwrap();
            conn.down.extend(frames);
        }
    }

    /// Delivers the next frame from the server to machine `m`; `flush`: the machine writes what it learnt.
    fn deliver_down(&mut self, m: usize, flush: bool) {
        let machine = &mut self.machines[m];
        let (Some(conn), Some(replica)) = (machine.conn.as_mut(), machine.replica.as_mut()) else { return };
        let Some(frame) = conn.down.pop_front() else { return };
        let text = format!("server -> m{m} {}", down_text(&frame));
        let result = replica.on_frame(frame);
        if flush {
            replica.flush().unwrap();
        }
        machine.most_waiting = machine.most_waiting.max(replica.waiting_bytes());
        for said in replica.take_notes() {
            if self.trace {
                eprintln!("m{m} says: {said}");
            }
            machine.said.push(said);
        }
        match result {
            Ok(frames) => conn.up.extend(frames),
            Err(e) => {
                self.note(format!("{text}: {e}; m{m} drops the connection"));
                self.drop_conn(m);
                return;
            }
        }
        self.note(text);
    }

    fn crash(&mut self, m: usize) {
        self.drop_conn(m);
        self.machines[m].replica = None;
        self.note(format!("m{m} crashes"));
    }

    fn restart(&mut self, m: usize) {
        let base = self.rng.next();
        let machine = &mut self.machines[m];
        if machine.replica.is_none() {
            machine.replica = Some(Replica::open(machine.store.clone(), self.space, peers(base)).unwrap());
            self.note(format!("m{m} restarts"));
            if self.trace {
                eprintln!("m{m} peers from {base}");
            }
        }
    }

    fn busy(&self) -> bool {
        self.machines.iter().any(|m| {
            m.conn.as_ref().is_some_and(|c| !c.up.is_empty() || !c.down.is_empty() || c.session.has_work() || !c.notes.is_empty())
                || m.old.as_ref().is_some_and(|c| !c.up.is_empty())
        }) || !self.rogue.deferred.is_empty()
    }

    /// Machine `m` opens a second connection while the server still reads the first one's frames.
    fn overlap(&mut self, m: usize) {
        let machine = &mut self.machines[m];
        if machine.old.is_some() || machine.conn.is_none() {
            return;
        }
        machine.old = machine.conn.take();
        if let Some(replica) = machine.replica.as_mut() {
            replica.disconnected();
        }
        self.connect(m);
        self.note(format!("m{m} connects again while its old connection lingers"));
    }

    /// The server reads the next frame the old connection had sent (its answers are lost: the machine left it).
    async fn deliver_old(&mut self, m: usize) {
        let Some(old) = self.machines[m].old.as_mut() else { return };
        let Some(frame) = old.up.pop_front() else {
            self.machines[m].old = None;
            return;
        };
        let text = format!("m{m} (old) -> server {}", up_text(&frame));
        let reply = old.session.handle(frame).await;
        if reply.close {
            self.machines[m].old = None;
        }
        self.note(text);
    }

    /// The server refuses one of machine `m`'s unacked updates for a moment (`rate_limited`) or as too large.
    fn force_error(&mut self, m: usize) {
        let too_large = self.rng.chance(30);
        let machine = &mut self.machines[m];
        let (Some(conn), Some(replica)) = (machine.conn.as_mut(), machine.replica.as_ref()) else { return };
        let unacked = replica.unacked();
        let Some((doc, client_seq)) = unacked.get(self.rng.below(unacked.len().max(1))).copied() else { return };
        let code = if too_large { "too_large" } else { "rate_limited" };
        conn.down.push_back(ServerFrame::error(code, "forced by the simulation", Some(doc), Some(client_seq)));
        self.note(format!("server refuses m{m}'s update {client_seq}: {code}"));
    }

    /// The database as it is now goes into a backup.
    async fn take_backup(&mut self) {
        self.backup = Some(self.state.meta.live_heads(self.space).await.unwrap());
        self.note("backup".into());
    }

    /// The server is restored from the last backup: every log cut back to it, a new epoch, every connection gone (the
    /// server restarted).
    async fn restore(&mut self) {
        let Some(backup) = self.backup.clone() else { return };
        for (doc, _) in self.state.meta.live_heads(self.space).await.unwrap() {
            let head = backup.iter().find(|(d, _)| *d == doc).map(|(_, h)| *h);
            self.state.meta.restore_live_log(self.space, doc, head).await.unwrap();
        }
        self.state.meta.reset_epoch(self.space).await.unwrap();
        for m in 0..self.machines.len() {
            self.drop_conn(m);
            self.machines[m].old = None;
        }
        // The rogue device, like any agent, sends what it has again.
        self.rogue.session = None;
        let again = self.rogue.sent.clone();
        self.rogue.sent.clear();
        for (app, bytes) in again {
            self.rogue_send(app, bytes).await;
        }
        self.note("the server is restored from the backup".into());
    }

    /// The rogue device sends `bytes` to `app`'s document.
    async fn rogue_send(&mut self, app: usize, bytes: Vec<u8>) {
        self.rogue_send_once(app, bytes.clone()).await;
        self.rogue.sent.push((app, bytes));
    }

    /// Sends `bytes` without keeping them to send again after a restore or a reset.
    async fn rogue_send_once(&mut self, app: usize, bytes: Vec<u8>) {
        // Quiet (revoked) after an overflow.
        if self.rogue_quiet() {
            return;
        }
        // A connection's frame budget is 400: the rogue opens a new one before it runs out (a flood is over a
        // thousand updates), so every update it sends is stored, as the checks assume.
        if self.rogue.on_session >= ROGUE_PER_SESSION {
            self.rogue.session = None;
        }
        let doc = recents_doc(self.space, &self.apps[app]);
        let client_seq = self.rogue.next_client_seq;
        self.rogue.next_client_seq += 1;
        // A second try on a new session when the document is gone (a restore whose logs went back dropped it while
        // the rogue's session stayed open): the new session creates it again, as an agent does.
        for attempt in 0..2 {
            if self.rogue.session.is_none() {
                self.rogue.on_session = 0;
                let mut session = Session::new(self.state.clone(), &self.rogue.auth);
                session.handle(ClientFrame::Hello { min: 1, max: 1 }).await;
                for app in &self.apps {
                    let doc = recents_doc(self.space, app);
                    session.handle(ClientFrame::Create { doc, kind: DocKind::Recents { app: app.clone() } }).await;
                    session.handle(ClientFrame::Subscribe { doc, after: u64::MAX, at: None }).await;
                }
                self.rogue.session = Some(session);
            }
            self.rogue.on_session += 1;
            let Some(session) = self.rogue.session.as_mut() else { break };
            let reply = session.handle(ClientFrame::Update { doc, client_seq, bytes: bytes.clone() }).await;
            let gone = matches!(reply.frames.as_slice(), [ServerFrame::Error { code, .. }] if code == "not_found");
            if gone && attempt == 0 {
                self.rogue.session = None;
                continue;
            }
            // Junk is stored too (the server never parses an update): anything but an ack, or a full document (it is
            // sent again after the admin's reset), is the harness's mistake.
            let full = matches!(reply.frames.as_slice(), [ServerFrame::Error { code, .. }] if code == "quota_exceeded");
            assert!(full || matches!(reply.frames.as_slice(), [ServerFrame::Ack { .. }]), "the rogue's update was refused: {:?}", reply.frames);
            break;
        }
    }

    /// One hostile act of the rogue device.
    async fn rogue_act(&mut self) {
        if self.rogue_quiet() {
            return;
        }
        let app = self.rng.below(self.apps.len());
        match self.rng.below(4) {
            // Junk that isn't a Loro update at all.
            0 => {
                let junk: Vec<u8> = (0..1 + self.rng.below(40)).map(|_| self.rng.next() as u8).collect();
                self.rogue_send(app, junk).await;
                self.note("rogue sends junk".into());
            }
            // A document of a newer version, on a chain of its own (in one seed of eight: once it wins, adds stop).
            1 if self.newer_ok => {
                let newer = loro::LoroDoc::new();
                let _ = newer.set_peer_id(self.rng.next() >> 1);
                newer.get_map("schema").insert("version", 99i64).unwrap();
                newer.commit();
                self.rogue_send(app, newer.export(loro::ExportMode::Snapshot).unwrap()).await;
                self.note("rogue sends a newer document".into());
            }
            1 => {}
            // Two commits of its chain, the second depending on the first: one with an entry that breaks the rules, one
            // valid; sent in order or the second first (the first held back for later).
            _ => {
                self.clock += 1;
                let bad = match self.rng.below(3) {
                    0 => "not json".to_owned(),
                    1 => serde_json::to_string(&rogue_entry("../escape.txt", self.clock)).unwrap(),
                    _ => serde_json::to_string(&RecentEntry { title: "t".repeat(300), ..rogue_entry("Docs/long.txt", self.clock) }).unwrap(),
                };
                let good = rogue_entry(&format!("Rogue/r{}.txt", self.rng.below(5)), self.clock + 1);
                let (bad_key, good_key) = (self.key(), self.key());
                let bad_first = self.rng.chance(50);
                let chain = &self.rogue.chains[app];
                let mut commits = Vec::new();
                for (key, json) in if bad_first {
                    vec![(bad_key, bad), (good_key, serde_json::to_string(&good).unwrap())]
                } else {
                    vec![(good_key, serde_json::to_string(&good).unwrap()), (bad_key, bad)]
                } {
                    let before = chain.oplog_vv();
                    chain.get_map("entries").insert(&hex(&key), json).unwrap();
                    chain.commit();
                    commits.push(chain.export(loro::ExportMode::updates(&before)).unwrap());
                }
                self.accepted.entry(self.apps[app].clone()).or_default().push((good_key, good));
                let second_first = self.rng.chance(50);
                let (first, second) = (commits.remove(0), commits.remove(0));
                if second_first {
                    self.rogue_send(app, second).await;
                    self.rogue.deferred.push((app, first));
                    self.note("rogue sends a commit before the one it depends on".into());
                } else {
                    self.rogue_send(app, first).await;
                    self.rogue_send(app, second).await;
                    self.note("rogue sends two commits, one with a bad entry".into());
                }
            }
        }
    }

    /// The rogue writes over an honest entry's key: with another valid entry, or a decodable value of the wrong shape
    /// (not a string, or over 16 KiB), with a good entry of its own in the same commit.
    async fn rogue_shape(&mut self) {
        if self.rogue_quiet() {
            return;
        }
        let app = self.rng.below(self.apps.len());
        let name = self.apps[app].clone();
        let honest: Vec<AddKey> =
            self.accepted.get(&name).map(|a| a.iter().filter(|(_, e)| e.machine != "rogue").map(|(k, _)| *k).collect()).unwrap_or_default();
        let Some(target) = honest.get(self.rng.below(honest.len().max(1))).copied() else { return };
        self.clock += 1;
        let good = rogue_entry(&format!("Rogue/s{}.txt", self.rng.below(5)), self.clock + 1);
        let good_key = self.key();
        let kind = self.rng.below(3);
        let chain = &self.rogue.chains[app];
        let before = chain.oplog_vv();
        let entries = chain.get_map("entries");
        match kind {
            0 => entries.insert(&hex(&target), 42i64).unwrap(),
            1 => entries.insert(&hex(&target), "x".repeat(17 * 1024)).unwrap(),
            _ => entries.insert(&hex(&target), serde_json::to_string(&rogue_entry("Rogue/over.txt", self.clock)).unwrap()).unwrap(),
        }
        entries.insert(&hex(&good_key), serde_json::to_string(&good).unwrap()).unwrap();
        chain.commit();
        let bytes = chain.export(loro::ExportMode::updates(&before)).unwrap();
        self.touched.entry(name.clone()).or_default().insert(target);
        self.accepted.entry(name).or_default().push((good_key, good));
        self.rogue_send(app, bytes).await;
        self.note(format!("rogue writes over an honest key ({})", ["a number", "17 KiB", "another entry"][kind]));
    }

    /// The rogue sends more updates than an agent holds, all waiting for a commit it never sends.
    async fn rogue_flood(&mut self) {
        let app = self.rng.below(self.apps.len());
        let chain = loro::LoroDoc::new();
        let peer = self.rng.next() >> 1;
        let _ = chain.set_peer_id(peer);
        self.orphans.insert(peer);
        chain.get_map("entries").insert("never", "sent").unwrap();
        chain.commit();
        for i in 0..1100 {
            let before = chain.oplog_vv();
            chain.get_map("entries").insert(&format!("flood{i}"), "x").unwrap();
            chain.commit();
            let bytes = chain.export(loro::ExportMode::updates(&before)).unwrap();
            self.rogue_send(app, bytes).await;
        }
        self.flooded = true;
        self.note("rogue floods updates that never apply".into());
    }

    /// A long stuck run: one update that never applies (its first commit is never sent), then over ten thousand more
    /// updates after it (junk: cheap, and every one a number in the log).
    async fn rogue_stuck(&mut self) {
        let app = self.rng.below(self.apps.len());
        let chain = loro::LoroDoc::new();
        let peer = self.rng.next() >> 1;
        let _ = chain.set_peer_id(peer);
        self.orphans.insert(peer);
        chain.get_map("entries").insert("never", "sent").unwrap();
        chain.commit();
        let before = chain.oplog_vv();
        chain.get_map("entries").insert("orphan", "x").unwrap();
        chain.commit();
        self.rogue_send(app, chain.export(loro::ExportMode::updates(&before)).unwrap()).await;
        for i in 0..10_100u32 {
            self.rogue_send_once(app, i.to_le_bytes().to_vec()).await;
        }
        self.note("rogue sends an orphan, then 10 100 updates".into());
    }

    /// The rogue fills a document past the cap (junk it doesn't keep: after the admin's reset it is gone for good).
    /// `valid`: with Loro operations every machine takes and keeps (values the list never shows), so the document's
    /// history alone is over the cap and it stays full after the admin's reset; else with junk bytes, which no
    /// machine keeps, so the reset frees it.
    async fn rogue_fill(&mut self, valid: bool) {
        if self.rogue_quiet() {
            return;
        }
        let app = self.rng.below(self.apps.len());
        let mut sent = 0u64;
        // Well over the cap: the history it leaves (compressed when merged) is over it too.
        while sent <= self.cap * 3 / 2 {
            let bytes = if valid {
                let chain = &self.rogue.chains[app];
                let before = chain.oplog_vv();
                let filler: String = (0..4096).map(|_| char::from(b'a' + (self.rng.next() % 26) as u8)).collect();
                let chain = &self.rogue.chains[app];
                chain.get_map("entries").insert(&format!("fill{sent}"), filler).unwrap();
                chain.commit();
                chain.export(loro::ExportMode::updates(&before)).unwrap()
            } else {
                vec![7; 4096]
            };
            sent += bytes.len() as u64;
            if valid {
                self.rogue_send(app, bytes).await;
            } else {
                self.rogue_send_once(app, bytes).await;
            }
        }
        self.note(format!("rogue fills a document ({})", if valid { "with Loro operations" } else { "with junk" }));
    }

    /// The rogue holds every machine's document past the cap on waiting updates: over 8 MiB of updates that wait for a
    /// commit never sent. Then it goes quiet (in the design, a device that does this is revoked).
    async fn rogue_overflow(&mut self) {
        // What it held back goes first (an entry it counted is in it).
        self.rogue_flush().await;
        let app = self.rng.below(self.apps.len());
        let chain = loro::LoroDoc::new();
        let peer = self.rng.next() >> 1;
        let _ = chain.set_peer_id(peer);
        self.orphans.insert(peer);
        chain.get_map("entries").insert("never", "sent").unwrap();
        chain.commit();
        let mut sent = 0;
        while sent <= MAX_WAITING_BYTES + 200_000 {
            let before = chain.oplog_vv();
            let filler: String = (0..60_000).map(|_| char::from(b'a' + (self.rng.next() % 26) as u8)).collect();
            chain.get_map("entries").insert(&format!("over{sent}"), filler).unwrap();
            chain.commit();
            let bytes = chain.export(loro::ExportMode::updates(&before)).unwrap();
            sent += bytes.len();
            self.rogue_send_once(app, bytes).await;
        }
        self.overflowed = Some(app);
        self.note("rogue holds a document past the cap on waiting updates, then goes quiet".into());
    }

    /// The admin empties every full document (`live reset-doc`); the rogue, like an agent, sends what it has again.
    async fn reset_full(&mut self) -> bool {
        let mut any = false;
        for app in self.apps.clone() {
            let doc = recents_doc(self.space, &app);
            if let Some(found) = self.state.meta.live_doc(self.space, doc).await.unwrap()
                && found.bytes >= self.cap
            {
                self.state.meta.reset_live_doc(self.space, doc).await.unwrap();
                any = true;
            }
        }
        if any {
            self.after_reset().await;
            self.note("the admin empties the full documents".into());
        }
        any
    }

    /// After the admin's reset: a backup is taken (so a later restore goes back to a state after the reset: a restore
    /// here only cuts logs back to a backup's heads, and can't bring back the rows a reset deleted), and the rogue,
    /// like an agent, sends what it has again (unless it went quiet).
    async fn after_reset(&mut self) {
        self.backup = Some(self.state.meta.live_heads(self.space).await.unwrap());
        self.rogue.session = None;
        let again = std::mem::take(&mut self.rogue.sent);
        if self.overflowed.is_none() {
            for (app, bytes) in again {
                self.rogue_send(app, bytes).await;
            }
        }
    }

    /// A restore whose epoch reset lags: the logs go back, but connections stay open and the epoch is the old one
    /// until [`Sim::reset_lagging`].
    async fn restore_lagging(&mut self) {
        let Some(backup) = self.backup.clone() else { return };
        for (doc, _) in self.state.meta.live_heads(self.space).await.unwrap() {
            let head = backup.iter().find(|(d, _)| *d == doc).map(|(_, h)| *h);
            self.state.meta.restore_live_log(self.space, doc, head).await.unwrap();
        }
        self.lagging = true;
        self.note("the server's logs go back (restored), the epoch not yet reset".into());
    }

    /// The admin runs `reset-epoch` at last; the rogue (like an agent) sends what it has again.
    async fn reset_lagging(&mut self) {
        if !std::mem::take(&mut self.lagging) {
            return;
        }
        self.state.meta.reset_epoch(self.space).await.unwrap();
        self.rogue.session = None;
        let again = std::mem::take(&mut self.rogue.sent);
        for (app, bytes) in again {
            self.rogue_send(app, bytes).await;
        }
        self.note("the epoch is reset".into());
    }

    /// The server's ping check of machine `m`'s connection (device revoked, epoch changed): it may end it.
    async fn ping(&mut self, m: usize) {
        let Some(conn) = self.machines[m].conn.as_mut() else { return };
        if let Some(frame) = conn.session.check().await {
            let text = format!("server closes m{m}'s connection: {}", down_text(&frame));
            self.drop_conn(m);
            self.note(text);
        }
    }

    /// The rogue does nothing: after an overflow (revoked), and in the seeds that test honest histories on a small cap.
    fn rogue_quiet(&self) -> bool {
        self.overflowed.is_some() || self.no_rogue
    }

    /// The distinct history of every machine's document of `app`, merged, as the log would hold it sent once: the
    /// bytes of the parts an agent would push of it to an empty log (the log's own measure).
    fn merged_history(&self, app: &AppName) -> usize {
        let merged = Recents::new(u64::MAX - 5);
        let doc = recents_doc(self.space, app);
        for machine in &self.machines {
            if let Some(snapshot) = machine.replica.as_ref().and_then(|r| r.snapshot_of(doc)) {
                let _ = merged.import_any_size(&snapshot);
            }
        }
        // The rogue is a device of the account too: what it holds of the app counts (it sends it again after a reset).
        if let Some(i) = self.apps.iter().position(|a| a == app)
            && let Some(chain) = self.rogue.chains.get(i)
            && let Ok(snapshot) = chain.export(loro::ExportMode::Snapshot)
        {
            let _ = merged.import_any_size(&snapshot);
        }
        let mut has = craft_cloud_agent::live::recents::Spans::new();
        let mut total = 0;
        loop {
            let missing = merged.missing(&has).unwrap();
            if missing.is_empty() {
                return total;
            }
            let Ok((bytes, _)) = merged.export_prefix(&missing, 64 * 1024, 1024 * 1024) else { return total };
            total += bytes.len();
            if let Some(ranges) = craft_cloud_agent::live::recents::spans_of(&bytes) {
                craft_cloud_agent::live::recents::add_spans(&mut has, &ranges);
            } else {
                return total;
            }
        }
    }

    /// How every machine's `app` document stands, for a failure message.
    fn states(&self) -> String {
        let mut out = Vec::new();
        for (m, machine) in self.machines.iter().enumerate() {
            for a in &self.apps {
                out.push(format!("m{m} {a}: {}", machine.replica.as_ref().map_or("down".into(), |r| r.describe(a))));
            }
        }
        out.join("\n")
    }

    /// Everything is delivered until nothing moves; the admin resets full documents (three times at most), and gives
    /// the remedy for a document held past the waiting cap. With `wan`, frames arrive late (a machine's are delivered
    /// in half the rounds) and the hubs tick every few rounds meanwhile. A document may end full only if its distinct
    /// merged history is itself over the cap; anything else that stops moving fails, with why.
    async fn settle(&mut self, seed: u64, wan: bool) {
        let apps = self.apps.clone();
        let mut rounds = 0;
        let mut blocked_rounds = 0;
        loop {
            rounds += 1;
            assert!(rounds < 40_000, "seed {seed}: the machines never settled:\n{}", self.states());
            for m in 0..3 {
                if wan && self.rng.chance(50) {
                    continue;
                }
                self.ping(m).await;
                self.deliver_old(m).await;
                self.deliver_up(m).await;
                self.pump(m).await;
                self.deliver_down(m, true);
                if self.machines[m].conn.is_none() {
                    self.connect(m);
                }
            }
            if wan && rounds % 4 == 0 {
                self.tick_all();
            }
            // (Over a slow network a full document's retries keep frames moving for good: every 50 rounds the check
            // for documents that stay full runs anyway.)
            let idle = !self.busy();
            if !idle && !(wan && rounds % 50 == 0) {
                continue;
            }
            let waiting: usize = self.machines.iter().filter_map(|m| m.replica.as_ref()).map(Replica::waiting).sum();
            let ticking = self.machines.iter().filter_map(|m| m.replica.as_ref()).any(Replica::wants_tick);
            if idle
                && !self.remedied
                && let Some(app) = self.overflowed
            {
                self.remedied = true;
                let said = self.machines.iter().any(|m| m.said.iter().any(|s| s.contains("wait for history that never came")));
                assert!(said, "seed {seed}: no machine said its document is stuck");
                self.state.meta.reset_live_doc(self.space, recents_doc(self.space, &apps[app])).await.unwrap();
                self.after_reset().await;
                self.note("the rogue is revoked; the admin empties its document".into());
                continue;
            }
            if idle && waiting == 0 && !ticking {
                return;
            }
            // Documents full on the server: the admin empties them (the next round's pings end the connections that
            // followed them).
            if self.resets_left > 0 && self.reset_full().await {
                self.resets_left -= 1;
                blocked_rounds = 0;
                continue;
            }
            // What is left waits only on documents that are full: allowed only when the document's distinct history is
            // itself over the cap (compaction is the fix), after every machine had the time to try and say so.
            let mut full = Vec::new();
            for app in &apps {
                let doc = self.state.meta.live_doc(self.space, recents_doc(self.space, app)).await.unwrap();
                if doc.is_some_and(|d| d.bytes >= self.cap) {
                    full.push(app.clone());
                }
            }
            let blocked =
                self.machines.iter().filter_map(|m| m.replica.as_ref()).all(|r| apps.iter().all(|a| full.contains(a) || r.waiting_for(a) == 0));
            blocked_rounds = if blocked && !full.is_empty() { blocked_rounds + 1 } else { 0 };
            if blocked_rounds > 40 {
                for app in &full {
                    let history = self.merged_history(app);
                    assert!(
                        history as u64 >= self.cap,
                        "seed {seed}: the {app} document stays full, though its history ({history} bytes) fits the cap ({}):\n{}",
                        self.cap,
                        self.states()
                    );
                }
                return;
            }
            if !idle {
                continue;
            }
            // The hub's tick, as often as a refused subscribe's longest pause needs.
            for _ in 0..64 {
                self.tick_all();
                if self.busy() {
                    break;
                }
            }
            // (A tick may finish what waited without a frame: a push that found the log lacks nothing.)
            let waiting: usize = self.machines.iter().filter_map(|m| m.replica.as_ref()).map(Replica::waiting).sum();
            if !self.busy() && waiting > 0 {
                panic!("seed {seed}: {waiting} update(s) wait but nothing is sent:\n{}", self.states());
            }
        }
    }

    /// Every hub's tick.
    fn tick_all(&mut self) {
        for m in 0..3 {
            let Some(replica) = self.machines[m].replica.as_mut() else { continue };
            let frames = replica.resend();
            if let Some(conn) = self.machines[m].conn.as_mut() {
                conn.up.extend(frames);
            }
        }
    }

    /// A machine that has left the account long ago left this history: two peers whose commits depend on each other's,
    /// about 6.8 MiB (so twice it stays well under the 16 MiB cap), sent to the log as one device would.
    async fn rogue_retired(&mut self) {
        let app = self.rng.below(self.apps.len());
        let h = loro::LoroDoc::new();
        let (x, y) = (self.rng.next() >> 1, self.rng.next() >> 1);
        let mut vv = h.oplog_vv();
        for i in 0..450u32 {
            for (peer, size) in [(y, 10), (x, 15_000)] {
                h.set_peer_id(peer).unwrap();
                let value: String = (0..size).map(|_| char::from(b'a' + (self.rng.next() % 26) as u8)).collect();
                h.get_map("entries").insert(&format!("retired{peer}-{i}"), value).unwrap();
                h.commit();
            }
            if i % 20 == 19 {
                self.rogue_send_once(app, h.export(loro::ExportMode::updates(&vv)).unwrap()).await;
                vv = h.oplog_vv();
            }
        }
        self.retired = Some(app);
        self.note("a retired machine's long history is in the log".into());
    }

    /// The rogue sends what it held back.
    async fn rogue_flush(&mut self) {
        let deferred = std::mem::take(&mut self.rogue.deferred);
        for (app, bytes) in deferred {
            self.rogue_send(app, bytes).await;
        }
    }

    fn key(&mut self) -> AddKey {
        let mut key = [0u8; 16];
        key[..8].copy_from_slice(&self.rng.next().to_le_bytes());
        key[8..].copy_from_slice(&self.rng.next().to_le_bytes());
        key
    }
}

fn rogue_entry(path: &str, opened_ms: i64) -> RecentEntry {
    RecentEntry { node: None, path: path.into(), title: path.into(), opened_ms, saved_ms: None, machine: "rogue".into() }
}

fn hex(key: &AddKey) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(key: &str) -> Option<AddKey> {
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(key.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

async fn run(seed: u64) -> Vec<String> {
    // One seed in seven fills documents past a small cap (512 KiB): with junk (the admin's reset frees it) or, in every
    // other such seed, with Loro operations every machine keeps (the history alone is over the cap: it stays full).
    // One in seven more has a cap so small (12 KiB) that the honest machines' own adds fill it, and their histories
    // cross it again after a reset, or not. One in 250 of the others has a long stuck run (heavy: over ten thousand
    // updates, some 25 s), and one in 250 the waiting cap reached (over 8 MiB of updates that never apply).
    let features = Features::from_env();
    let quota_ok = features.quota && seed % 7 == 2;
    let small_ok = features.quota && seed % 7 == 5;
    let stuck_ok = !quota_ok && seed % 250 == 7;
    let overflow_ok = features.stuck && !quota_ok && !small_ok && seed % 250 == 57;
    // One in 500 has a retired machine's long history (over 8 MiB, two peers depending on each other), a reset, and a
    // new machine joining after the pushes: heavy.
    let retired_ok = features.quota && !quota_ok && !small_ok && !overflow_ok && seed % 500 == 101;
    let cap = if quota_ok {
        512 * 1024
    } else if small_ok {
        12 * 1024
    } else {
        craft_cloud_meta::MAX_LIVE_DOC_BYTES
    };
    let meta = Meta::open("sqlite::memory:").await.unwrap().with_live_doc_cap(cap);
    let mut rng = Rng::new(seed);
    // IDs from the seed: the recents documents' IDs derive from the space, and the order of documents follows them.
    let mut id = || uuid::Uuid::from_u64_pair(rng.next(), rng.next());
    let ids = (UserId(id()), SpaceId::from_uuid(id()), NodeId::from_uuid(id()));
    let user = meta.create_user_with_ids("ana", "$argon2id$stub", Role::Member, 0, ids).await.unwrap();
    let state = Arc::new(AppState::new(meta.clone(), Store::new(MemoryBackend::new()), vec![]).unwrap());
    let mut machines = Vec::new();
    for i in 0..3 {
        let token = *blake3::hash(format!("sim device {seed} {i}").as_bytes()).as_bytes();
        meta.create_device(user.id, &format!("m{i}"), &token, 0).await.unwrap();
        let auth = meta.device_by_token(&token).await.unwrap().unwrap();
        let store = MemStore::default();
        let base = rng.next();
        if std::env::var("SIM_TRACE").is_ok() {
            eprintln!("m{i} peers from {base}");
        }
        let replica = Replica::open(store.clone(), user.space, peers(base)).unwrap();
        let skew = rng.below(40) as i64 - 20;
        machines.push(Machine {
            name: format!("m{i}"),
            store,
            replica: Some(replica),
            conn: None,
            old: None,
            auth,
            skew,
            said: Vec::new(),
            most_waiting: 0,
        });
    }
    let token = *blake3::hash(format!("sim rogue {seed}").as_bytes()).as_bytes();
    meta.create_device(user.id, "rogue", &token, 0).await.unwrap();
    let rogue_auth = meta.device_by_token(&token).await.unwrap().unwrap();
    let chains = (0..2)
        .map(|_| {
            let doc = loro::LoroDoc::new();
            let _ = doc.set_peer_id(rng.next() >> 1);
            doc
        })
        .collect();
    let rogue = Rogue { auth: rogue_auth, session: None, chains, next_client_seq: 1, deferred: Vec::new(), sent: Vec::new(), on_session: 0 };
    // Half the seeds open many files (the others a few, over and over); a quarter more files than the cap holds.
    let (many, full) = (seed.is_multiple_of(2), seed.is_multiple_of(4));
    let files = if full { FILES_FULL } else { FILES };
    let nodes = (0..files * 2 / 3).map(|_| NodeId::from_uuid(uuid::Uuid::from_u64_pair(rng.next(), rng.next()))).collect();
    let apps = vec![AppName::new("wordcraft").unwrap(), AppName::new("gridcraft").unwrap()];
    let trace = std::env::var("SIM_TRACE").is_ok();
    let mut sim = Sim {
        rng,
        state,
        space: user.space,
        machines,
        apps,
        nodes,
        files,
        accepted: BTreeMap::new(),
        clock: 1_000,
        trace,
        log: Vec::new(),
        rogue,
        backup: None,
        newer_ok: seed % 8 == 3,
        touched: BTreeMap::new(),
        lagging: false,
        flooded: false,
        orphans: std::collections::BTreeSet::new(),
        overflowed: None,
        remedied: false,
        resets_left: 3,
        retired: None,
        no_rogue: small_ok,
        cap,
    };
    let mut stuck_done = false;
    // A flood is heavy (over a thousand updates): one seed in five may have one.
    let flood_ok = seed % 5 == 1;
    if full {
        // Every file once, on one machine or another, before any machine is online.
        for i in 0..files {
            let m = sim.rng.below(3);
            sim.add_file(m, 0, i);
        }
    }

    for step in 0..STEPS {
        // In their seeds, a fill and a stuck run happen for sure (and more fills at random).
        if step == STEPS / 3 && quota_ok {
            sim.rogue_fill(seed.is_multiple_of(2)).await;
        }
        if step == STEPS / 2 && overflow_ok {
            sim.rogue_overflow().await;
        }
        if step == STEPS / 4 && retired_ok {
            sim.rogue_retired().await;
        }
        if step == STEPS / 4 && features.stuck && stuck_ok && !stuck_done {
            stuck_done = true;
            sim.rogue_stuck().await;
        }
        let m = sim.rng.below(3);
        match sim.rng.below(100) {
            0..=17 => sim.add(m, many),
            18..=39 => sim.deliver_up(m).await,
            40..=54 => sim.pump(m).await,
            55..=79 => {
                let flush = sim.rng.chance(80);
                sim.deliver_down(m, flush);
            }
            80..=86 => sim.connect(m),
            87..=90 => {
                sim.drop_conn(m);
                sim.note(format!("m{m} loses its connection"));
            }
            91..=93 => sim.crash(m),
            94..=96 => sim.restart(m),
            // The harsher events, each a few times per run.
            97 => match sim.rng.below(15) {
                0 if features.restore || features.lag => sim.take_backup().await,
                1 if features.restore => sim.restore().await,
                2 | 3 if features.hostile => sim.rogue_act().await,
                4 if features.hostile => sim.rogue_flush().await,
                5 if features.overlap => sim.overlap(m),
                6 if features.forced => sim.force_error(m),
                7 if features.shape => sim.rogue_shape().await,
                8 if features.flood && flood_ok && !sim.flooded => sim.rogue_flood().await,
                9 if features.lag => sim.restore_lagging().await,
                10 if features.lag => sim.reset_lagging().await,
                11 => sim.ping(m).await,
                12 if quota_ok => sim.rogue_fill(seed.is_multiple_of(2)).await,
                13 if quota_ok || small_ok => {
                    sim.reset_full().await;
                }
                14 if features.stuck && stuck_ok && !stuck_done => {
                    stuck_done = true;
                    sim.rogue_stuck().await;
                }
                _ => {}
            },
            98 if features.overlap => sim.deliver_old(m).await,
            _ => {
                if let Some(replica) = sim.machines[m].replica.as_mut() {
                    let frames = replica.resend();
                    if let Some(conn) = sim.machines[m].conn.as_mut() {
                        conn.up.extend(frames);
                    }
                }
            }
        }
    }

    // Everyone comes back, follows every app, and everything is delivered until nothing moves.
    for m in 0..3 {
        sim.restart(m);
        sim.connect(m);
    }
    let apps = sim.apps.clone();
    for m in 0..3 {
        for app in &apps {
            let Some(replica) = sim.machines[m].replica.as_mut() else { continue };
            let frames = replica.follow(app);
            if let Some(conn) = sim.machines[m].conn.as_mut() {
                conn.up.extend(frames);
            }
        }
    }
    sim.rogue_flush().await;
    // The admin resets the epoch at last, if a restore's reset lagged; the server pings every connection.
    sim.reset_lagging().await;
    // Long pushes over a slow network: frames take rounds to arrive, and the hubs tick meanwhile.
    let wan = retired_ok || seed % 13 == 6;
    sim.settle(seed, wan).await;
    if let Some(app) = sim.retired {
        // Every machine has the retired history now; the admin empties the log, and they push what it lacks.
        let doc = recents_doc(sim.space, &sim.apps[app]);
        sim.state.meta.reset_live_doc(sim.space, doc).await.unwrap();
        sim.after_reset().await;
        sim.note("the admin empties the document with the retired history".into());
        sim.settle(seed, wan).await;
        // The machines' pushes alone (no reset used, nothing reaching the cap) hold the history only a few
        // parts over (pushers queue behind whoever is pushing).
        assert_eq!(sim.resets_left, 3, "seed {seed}: a reset was used for the retired history");
        for a in &apps {
            let bytes = sim.state.meta.live_doc(sim.space, recents_doc(sim.space, a)).await.unwrap().map_or(0, |d| d.bytes);
            assert!(bytes < sim.cap, "seed {seed}: the {a} document reached the cap ({bytes} bytes)");
        }
        let log = sim.state.meta.live_doc(sim.space, doc).await.unwrap().map_or(0, |d| d.bytes);
        let history = sim.merged_history(&sim.apps[app].clone());
        sim.note(format!("retired: log {log} bytes for a history of {history} after the pushes"));
        // (1.1: the bound in the report is a few parts over the history; 1.5 was the old one's room.)
        assert!(log * 10 <= history as u64 * 11, "seed {seed}: the pushes put {log} bytes in the log for a history of {history}");
        // A new machine joins (machine 2 wiped: a fresh install), and catches up from the log the pushes made.
        let machine = &mut sim.machines[2];
        machine.conn = None;
        machine.replica = None;
        machine.store = MemStore::default();
        machine.most_waiting = 0;
        sim.note("m2 is wiped and joins afresh".into());
        sim.restart(2);
        sim.connect(2);
        for app in &apps {
            if let Some(replica) = sim.machines[2].replica.as_mut() {
                let frames = replica.follow(app);
                if let Some(conn) = sim.machines[2].conn.as_mut() {
                    conn.up.extend(frames);
                }
            }
        }
        sim.settle(seed, wan).await;
        let most = sim.machines[2].most_waiting;
        assert!(most <= 2 * 64 * 1024, "seed {seed}: the new machine held {most} bytes waiting for their history");
    }

    for (m, machine) in sim.machines.iter().enumerate() {
        // Only the updates of a rogue chain whose first commit never comes may wait for good (and, in a document that
        // stays full, those whose history the log had no room for).
        for app in &apps {
            let full = sim.state.meta.live_doc(sim.space, recents_doc(sim.space, app)).await.unwrap().is_some_and(|d| d.bytes >= sim.cap);
            let waits = machine.replica.as_ref().map(|r| r.waiting_peers_of(app)).unwrap_or_default();
            let honest: Vec<&u64> = waits.difference(&sim.orphans).collect();
            assert!(full || honest.is_empty(), "seed {seed}: m{m} still has {app} updates of peers {honest:?} waiting for their history");
        }
    }
    for app in &apps {
        let lists: Vec<Vec<RecentEntry>> = sim.machines.iter().map(|m| m.replica.as_ref().map(|r| r.entries(app)).unwrap_or_default()).collect();
        let full = sim.state.meta.live_doc(sim.space, recents_doc(sim.space, app)).await.unwrap().is_some_and(|d| d.bytes >= sim.cap);
        if full {
            // A document whose history alone is over the cap stays full (log compaction, later, is the fix). Nothing is lost:
            // every machine still has every add it accepted (or a newer one that pushes it out of the list), and one
            // with updates to send says the document is full.
            for (m, machine) in sim.machines.iter().enumerate() {
                let Some(replica) = machine.replica.as_ref() else { continue };
                // Its own adds, put back into what it has, change nothing it shows.
                let mut all: Vec<(AddKey, RecentEntry)> =
                    replica.readable(app).into_iter().filter_map(|(key, e)| unhex(&key).map(|k| (k, e))).collect();
                let own = sim
                    .accepted
                    .get(app)
                    .map(|a| a.iter().filter(|(_, e)| e.machine == machine.name).cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                for (k, e) in own {
                    if !all.iter().any(|(have, _)| *have == k) {
                        all.push((k, e));
                    }
                }
                assert_eq!(lists[m], view_of(all), "seed {seed}: m{m} lost an add of its own in the full {app} document");
                if replica.waiting_for(app) > 0 {
                    assert!(machine.said.iter().any(|s| s.contains("full")), "seed {seed}: m{m} doesn't say the {app} document is full");
                }
            }
            // And every add any machine (or the rogue) made, put back into what the machines hold together, changes
            // nothing their merged list shows: no add is lost anywhere.
            let mut together: Vec<(AddKey, RecentEntry)> = Vec::new();
            for machine in &sim.machines {
                for (key, e) in machine.replica.as_ref().map(|r| r.readable(app)).unwrap_or_default() {
                    if let Some(k) = unhex(&key)
                        && !together.iter().any(|(have, _)| *have == k)
                    {
                        together.push((k, e));
                    }
                }
            }
            // (The rogue is a device of the account: what it holds counts.)
            let rogue =
                sim.apps.iter().position(|a| a == app).and_then(|i| sim.rogue.chains.get(i)).and_then(|c| c.export(loro::ExportMode::Snapshot).ok());
            if let Some(held) = rogue.and_then(|snapshot| Recents::load(&snapshot, u64::MAX - 7).ok()) {
                for (key, e) in held.readable() {
                    if let Some(k) = unhex(&key)
                        && !together.iter().any(|(have, _)| *have == k)
                    {
                        together.push((k, e));
                    }
                }
            }
            let held = view_of(together.clone());
            for (k, e) in sim.accepted.get(app).cloned().unwrap_or_default() {
                if !together.iter().any(|(have, _)| *have == k) && !sim.touched.get(app).is_some_and(|t| t.contains(&k)) {
                    together.push((k, e));
                }
            }
            assert_eq!(held, view_of(together), "seed {seed}: an add is lost from every machine in the full {app} document");
            sim.note(format!("{app}: full"));
            continue;
        }
        for (m, list) in lists.iter().enumerate() {
            assert_eq!(list, &lists[0], "seed {seed}: m{m}'s {app} list differs from m0's");
        }
        // Keys the rogue wrote over hold whatever Loro's merge of every update makes of them: merged here on its own,
        // from the server's log (every machine's whole document is in it after the settle); every other accepted add
        // is as it was made.
        let touched = sim.touched.get(app).cloned().unwrap_or_default();
        let mut all: Vec<(AddKey, RecentEntry)> =
            sim.accepted.get(app).cloned().unwrap_or_default().into_iter().filter(|(k, _)| !touched.contains(k)).collect();
        if !touched.is_empty() {
            let merged = Recents::new(u64::MAX - 3);
            let doc = recents_doc(sim.space, app);
            let mut after = 0;
            loop {
                let page = sim.state.meta.live_updates(sim.space, doc, after, 1000).await.unwrap();
                let Some(last) = page.last().map(|u| u.seq) else { break };
                for update in page {
                    let _ = merged.import(&update.bytes);
                }
                after = last;
            }
            for (key, entry) in merged.readable() {
                if let Some(k) = touched.iter().find(|k| hex(k) == key) {
                    all.push((*k, entry));
                }
            }
        }
        let reference = view_of(all);
        assert_eq!(lists[0], reference, "seed {seed}: the {app} list isn't what every accepted add makes");
        let shown: Vec<String> = lists[0].iter().map(|e| format!("{}@{}", e.path, e.opened_ms)).collect();
        sim.note(format!("{app}: {}", shown.join(", ")));
    }
    sim.log
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
                    // (`SIM_SAY=1`: each seed as it starts and ends, to find one that aborts the process.)
                    if std::env::var("SIM_SAY").is_ok() {
                        eprintln!("start {seed}");
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(seed)));
                    if std::env::var("SIM_SAY").is_ok() {
                        eprintln!("end {seed}");
                    }
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
fn recents_converge_without_losing_an_add() {
    for_each_seed(40, |seed| {
        block_on(run(seed));
    });
}

#[test]
fn a_seed_replays_exactly() {
    for seed in 0..20 {
        let (a, b) = (block_on(run(seed)), block_on(run(seed)));
        assert_eq!(a, b, "seed {seed} ran two ways");
    }
}
