//! What the documents tests share: machines (the agent's own [`Replica`] over a store that outlasts its crashes)
//! against the server's own live [`Session`]s over the real metadata database (SQLite in memory), connections as
//! queues of frames, and app stand-ins (a Loro document the way an app's would be). Included by `#[path]`.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;

use craft_cloud_agent::live::replica::{DocEvent, Opening, Peers, Replica};
use craft_cloud_agent::live::store::MemStore;
use craft_cloud_meta::{DeviceAuth, Meta, Role, UserId};
use craft_cloud_proto::live::{AppName, ClientFrame, ServerFrame};
use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, DocId, FileHash, Manifest, Name, NewVersion, NodeId, Op, SpaceId, VersionId};
use craft_cloud_server::AppState;
use craft_cloud_server::live::{PresenceNote, Session};
use craft_cloud_server::state::Limits;
use craft_cloud_store::Store;
use craft_cloud_store::memory::MemoryBackend;
use loro::{ExportMode, LoroDoc};
use tokio::sync::broadcast;

pub fn app() -> AppName {
    AppName::new("wordcraft").unwrap()
}

/// Loro peer IDs for one run of a machine: distinct per document and per run.
pub fn peers(base: u64) -> Peers {
    let mut n = 0u64;
    Box::new(move || {
        n += 1;
        base.wrapping_add(n.wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 1
    })
}

/// An open connection: the server's session for it, what the server hears of its space, and its presence.
pub struct Conn {
    pub session: Session,
    pub notes: broadcast::Receiver<DocId>,
    pub presence: broadcast::Receiver<PresenceNote>,
}

pub struct Machine {
    pub store: MemStore,
    pub replica: Option<Replica<MemStore>>,
    pub auth: DeviceAuth,
    pub conn: Option<Conn>,
    pub up: VecDeque<ClientFrame>,
    pub down: VecDeque<ServerFrame>,
    /// What happened to its documents, as its apps would hear it.
    pub events: Vec<DocEvent>,
    pub said: Vec<String>,
    /// Every frame its agent was sent (for tests that look at what came).
    pub heard: Vec<ServerFrame>,
    /// Every frame its agent sent that reached the server.
    pub said_up: Vec<ClientFrame>,
    /// This machine's runs so far (each gets peers of its own).
    pub runs: u64,
    pub base: u64,
    /// Rewrite its `hello` to name only protocol 1 (an old server, as the agent sees it).
    pub old_server: bool,
}

impl Machine {
    pub fn replica(&mut self) -> &mut Replica<MemStore> {
        self.replica.as_mut().expect("the machine runs")
    }
}

pub struct World {
    pub state: Arc<AppState>,
    pub meta: Meta,
    pub space: SpaceId,
    pub user: UserId,
    pub machines: Vec<Machine>,
    root: NodeId,
    files: u32,
}

impl World {
    pub async fn new(limits: Limits) -> Self {
        Self::with_meta(limits, Meta::open("sqlite::memory:").await.unwrap()).await
    }

    pub async fn with_meta(limits: Limits, meta: Meta) -> Self {
        let user = meta.create_user("ana", "$argon2id$stub", Role::Member, 0).await.unwrap();
        let state = Arc::new(AppState::new(meta.clone(), Store::new(MemoryBackend::new()), vec![]).unwrap().with_limits(limits));
        let root = meta.space_info(user.space).await.unwrap().root;
        Self { state, meta, space: user.space, user: user.id, machines: Vec::new(), root, files: 0 }
    }

    /// A new machine of the account (not connected yet).
    pub async fn machine(&mut self) -> usize {
        let i = self.machines.len();
        let token = [10 + i as u8; 32];
        self.meta.create_device(self.user, &format!("m{i}"), &token, 0).await.unwrap();
        let auth = self.meta.device_by_token(&token).await.unwrap().unwrap();
        let store = MemStore::default();
        let base = (1000 * (i as u64 + 1)) << 20;
        let replica = Replica::open(store.clone(), self.space, peers(base)).unwrap();
        self.machines.push(Machine {
            store,
            replica: Some(replica),
            auth,
            conn: None,
            up: VecDeque::new(),
            down: VecDeque::new(),
            events: Vec::new(),
            said: Vec::new(),
            heard: Vec::new(),
            said_up: Vec::new(),
            runs: 1,
            base,
            old_server: false,
        });
        i
    }

    /// A new file of the account's space, as the file sync would have uploaded it.
    pub async fn file(&mut self) -> NodeId {
        self.files += 1;
        let name = format!("file{}.txt", self.files);
        let chunk = ChunkId::from_bytes(*blake3::hash(name.as_bytes()).as_bytes());
        self.meta.add_chunk(self.space, &chunk, 11, 0).await.unwrap();
        let manifest =
            Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 10, FileHash::from_bytes([1; 32]), vec![ChunkRef { id: chunk, len: 10 }])
                .unwrap();
        self.meta.add_manifest(self.space, &manifest, 100, 0).await.unwrap();
        let node = NodeId::generate();
        let version = NewVersion { id: VersionId::generate(), manifest: manifest.id(), mtime_ms: 0, exec: false };
        let device = self.machines.first().map_or_else(craft_cloud_proto::DeviceId::generate, |m| m.auth.device);
        self.meta.commit(self.space, device, &[Op::Create { node, parent: self.root, name: Name::new(&name).unwrap(), version }], 0).await.unwrap();
        node
    }

    pub fn connect(&mut self, m: usize) {
        let state = self.state.clone();
        let machine = &mut self.machines[m];
        let notes = state.live.listen(machine.auth.space);
        let presence = state.live.listen_presence(machine.auth.space);
        machine.conn = Some(Conn { session: Session::new(state, &machine.auth), notes, presence });
        let hello = machine.replica().connected();
        machine.up.extend(hello);
    }

    /// The connection goes, with whatever was in flight both ways.
    pub fn disconnect(&mut self, m: usize) {
        let machine = &mut self.machines[m];
        machine.conn = None;
        machine.up.clear();
        machine.down.clear();
        if let Some(replica) = machine.replica.as_mut() {
            replica.disconnected();
        }
        machine.events.extend(machine.replica.as_mut().map(|r| r.take_events()).unwrap_or_default());
    }

    /// The machine crashes (what it hadn't written is gone) and starts again from its store.
    pub fn crash(&mut self, m: usize) {
        self.disconnect(m);
        let machine = &mut self.machines[m];
        machine.replica = None;
        machine.runs += 1;
        let base = machine.base ^ (machine.runs << 40);
        let mut replica = Replica::open(machine.store.clone(), self.space, peers(base)).unwrap();
        machine.said.extend(replica.take_notes());
        machine.replica = Some(replica);
    }

    /// The machine stops cleanly: everything it holds is written first.
    pub fn stop(&mut self, m: usize) {
        if let Some(replica) = self.machines[m].replica.as_mut() {
            replica.flush_documents().unwrap();
        }
        self.crash(m);
    }

    /// Hands frames from the agent to its connection's queue.
    pub fn send(&mut self, m: usize, frames: Vec<ClientFrame>) {
        let machine = &mut self.machines[m];
        if machine.conn.is_some() {
            machine.up.extend(frames);
        }
    }

    /// One round of delivery on every connection; whether anything moved.
    pub async fn step(&mut self) -> bool {
        let mut moved = false;
        for machine in &mut self.machines {
            let Some(conn) = machine.conn.as_mut() else { continue };
            while let Some(mut frame) = machine.up.pop_front() {
                moved = true;
                if machine.old_server
                    && let ClientFrame::Hello { min, .. } = frame
                {
                    frame = ClientFrame::Hello { min, max: 1 };
                }
                machine.said_up.push(frame.clone());
                machine.down.extend(conn.session.handle(frame).await.frames);
            }
            while let Ok(doc) = conn.notes.try_recv() {
                conn.session.note(doc);
            }
            while let Ok(note) = conn.presence.try_recv() {
                machine.down.extend(conn.session.relay(&note));
            }
            while conn.session.has_work() {
                moved = true;
                machine.down.extend(conn.session.pump().await.unwrap());
            }
            let Some(replica) = machine.replica.as_mut() else { continue };
            while let Some(frame) = machine.down.pop_front() {
                moved = true;
                machine.heard.push(frame.clone());
                match replica.on_frame(frame) {
                    Ok(frames) => machine.up.extend(frames),
                    Err(e) => {
                        machine.said.push(format!("the link ended: {e}"));
                        machine.conn = None;
                        machine.up.clear();
                        machine.down.clear();
                        replica.disconnected();
                        break;
                    }
                }
            }
            replica.flush().unwrap();
            machine.events.extend(replica.take_events());
            machine.said.extend(replica.take_notes());
        }
        moved
    }

    /// Delivers until nothing moves, ticking the agents (their resends) whenever things are quiet.
    pub async fn settle(&mut self) {
        let mut quiet = 0;
        for _ in 0..100_000 {
            if self.step().await {
                quiet = 0;
                continue;
            }
            quiet += 1;
            if quiet > 40 {
                return;
            }
            for m in 0..self.machines.len() {
                if self.machines[m].conn.is_some()
                    && let Some(replica) = self.machines[m].replica.as_mut()
                {
                    let frames = replica.resend();
                    self.machines[m].up.extend(frames);
                }
            }
        }
        panic!("never settled");
    }

    /// The machine's app opens the file `node`: the replica's answer, and its frames sent.
    pub fn open(&mut self, m: usize, node: NodeId) -> (DocId, Opening) {
        let (doc, opening, frames) = self.machines[m].replica().open_document(node, &app());
        self.send(m, frames);
        (doc, opening)
    }

    /// Takes the machine's events.
    pub fn events(&mut self, m: usize) -> Vec<DocEvent> {
        let machine = &mut self.machines[m];
        let mut events = std::mem::take(&mut machine.events);
        if let Some(replica) = machine.replica.as_mut() {
            events.extend(replica.take_events());
        }
        events
    }

    /// The machine's app imports `content` (a plain-text document) for `doc`.
    pub fn import(&mut self, m: usize, doc: DocId, content: &str) {
        let snapshot = App::new(1).with_text(content).snapshot();
        let frames = self.machines[m].replica().import_document(doc, &snapshot).unwrap();
        self.send(m, frames);
    }

    /// The machine's app makes a change (on disk before this returns).
    pub fn edit(&mut self, m: usize, doc: DocId, update: &[u8]) {
        let frames = self.machines[m].replica().local_update(doc, update).unwrap();
        self.send(m, frames);
    }

    /// The document as an app opening it on the machine would see it.
    pub fn app_of(&mut self, m: usize, doc: DocId, peer: u64) -> App {
        let (snapshot, waiting) = self.machines[m].replica().document_state(doc).expect("the document is live there");
        let app = App::new(peer);
        app.apply(&snapshot);
        for w in waiting {
            app.apply(&w);
        }
        app
    }

    /// The document's text as the machine holds it.
    pub fn text(&mut self, m: usize, doc: DocId) -> String {
        self.app_of(m, doc, 7).text()
    }
}

/// An app's own Loro document: a plain text under `text`, and a map under `map`.
pub struct App {
    pub doc: LoroDoc,
}

impl App {
    pub fn new(peer: u64) -> Self {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer).unwrap();
        Self { doc }
    }

    pub fn with_text(self, text: &str) -> Self {
        self.doc.get_text("text").insert(0, text).unwrap();
        self.doc.commit();
        self
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.doc.export(ExportMode::Snapshot).unwrap()
    }

    /// Types `s` at `at` (characters): the change, as an app sends it.
    pub fn type_at(&self, at: usize, s: &str) -> Vec<u8> {
        let before = self.doc.oplog_vv();
        let text = self.doc.get_text("text");
        let at = at.min(text.len_unicode());
        text.insert(at, s).unwrap();
        self.doc.commit();
        self.doc.export(ExportMode::updates(&before)).unwrap()
    }

    /// Types at the end.
    pub fn append(&self, s: &str) -> Vec<u8> {
        let len = self.doc.get_text("text").len_unicode();
        self.type_at(len, s)
    }

    /// Sets `key` in the map.
    pub fn set(&self, key: &str, value: i64) -> Vec<u8> {
        let before = self.doc.oplog_vv();
        self.doc.get_map("map").insert(key, value).unwrap();
        self.doc.commit();
        self.doc.export(ExportMode::updates(&before)).unwrap()
    }

    pub fn apply(&self, bytes: &[u8]) {
        self.doc.import(bytes).unwrap();
    }

    pub fn text(&self) -> String {
        self.doc.get_text("text").to_string()
    }
}
