//! A fake agent for tests (feature `fake`): it serves the local API's `/v1/status`, `/v1/docs` and `/v1/blobs` on a
//! loopback port of this process, from memory, so an app (or the app kit) is tested through the real client code with
//! no agent and no server.
//!
//! Every [`Fake`] is one machine. Fakes made from one [`Hub`] are machines of one account: the hub stands in for the
//! server, holding each document (its import and every update after it, in order) and the blobs. A machine keeps its
//! own copy of each document it opened (as the agent's journal does): an app's update goes into that copy and is acked
//! at once, and goes to the hub while the machine is online ([`Fake::set_online`]); offline it waits there and goes up
//! when the machine is back, and the other machines' updates come down then. What the bytes mean is never read: the
//! state an app opens is the import followed by the updates as `update` frames (the local API allows that).
//!
//! A change an app sends in fragments goes into the machine's copy (and is acked) only once it is whole, as its
//! fragments, each an update of the log (as the agent's log and the server keep them). A machine joins them before
//! its apps get the change, as the agent does: an app never gets a fragment. [`Fake::set_fragment_faults`] stops a
//! machine's link from the hub inside such a change for a while (a laptop that sleeps part of the way through it;
//! [`Hub::tick`] moves the pauses on), and [`Fake::drop_connections_after`] stops the agent part of the way through an
//! app's change.
//!
//! [`Fake::restart`] drops every app connection and refuses new ones until [`Fake::start`] (an agent that stopped):
//! what the machine held stays, as the agent's files do. [`Fake::too_old`] makes a machine answer the blob calls as an
//! agent from before blobs (`not_found` for the call). [`Hub::set_live`] holds live updates back between machines while
//! saved files' records still go (a proxy that blocks WebSockets).

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use craft_cloud_api::DocId;
use craft_cloud_api::fragments::{SetId, is_fragment, join, parse};
use craft_cloud_api::limits::{MAX_BLOB_BYTES, MAX_PRESENCE_BYTES, MAX_UPDATE_BYTES};
use craft_cloud_api::local::blobs::{Stored, hex, parse_hex};
use craft_cloud_api::local::docs::{IMPORT, MAX_BINARY_BYTES, PRESENCE, STATE, UPDATE, decode_binary, encode_binary};
use craft_cloud_api::local::{DocState, DocsEvent, DocsRequest, LOCAL_API_MAX, MAX_LOCAL_JSON_BYTES, NotLiveReason, RunState, Status, paths};
use tungstenite::protocol::{Role, WebSocketConfig};
use tungstenite::{Message, WebSocket};

use crate::Client;

/// The account the fakes of one hub share: the server's part.
#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Inner {
    docs: HashMap<String, HubDoc>,
    blobs: HashMap<[u8; 32], Vec<u8>>,
    machines: HashMap<usize, Machine>,
    next_machine: usize,
    next_conn: u64,
    /// The record of each path's file on the server (as if every save were written and synced at once: the newest
    /// upload wins); `None` for one edited outside Craft ([`Hub::edit_outside`]).
    files: HashMap<String, Option<Version>>,
    /// The live relay is down while the file sync goes on ([`Hub::set_live`]).
    live_down: bool,
}

struct HubDoc {
    id: DocId,
    import: Vec<u8>,
    /// Every update after the import, with the machine it came from.
    log: Vec<(usize, Vec<u8>)>,
}

/// Where an update in a machine's copy came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// An app connection of this machine.
    Conn(u64),
    /// Another machine.
    Remote,
}

/// A machine's copy of a document.
struct MachineDoc {
    path: String,
    import: Vec<u8>,
    log: Vec<(Origin, Vec<u8>)>,
    /// How much of the hub's log this copy has taken.
    hub_seen: usize,
    /// Positions in `log` of this machine's updates the hub doesn't have yet.
    pending: VecDeque<usize>,
    /// Ticks the hub's rows wait for (from `hub_seen` on): the machine's link stopped inside a change in fragments
    /// ([`FragmentFaults`]).
    pause: u32,
    /// The hub row a pause stopped at: taken without asking the faults again once the pause is over.
    paused_at: Option<usize>,
}

struct Machine {
    name: String,
    token: String,
    online: bool,
    running: bool,
    too_old: bool,
    docs: HashMap<DocId, MachineDoc>,
    blobs: HashMap<[u8; 32], Vec<u8>>,
    pending_blobs: Vec<[u8; 32]>,
    conns: HashMap<u64, Conn>,
    /// The save records apps sent: document, content hash (hex), frontiers.
    saves: Vec<(DocId, String, Version)>,
    /// Codes the next updates are refused with ([`Fake::refuse_next`]).
    refuse: VecDeque<String>,
    /// Saves made here that haven't gone up yet (offline): each path's file record on this machine until they do.
    file_pending: HashMap<String, Version>,
    /// What happens to fragments on their way from the hub to this machine ([`Fake::set_fragment_faults`]).
    faults: Option<Faults>,
    /// How many times the faults stopped the link inside a change.
    pauses: u64,
    /// App updates (frames) left before every app connection ends ([`Fake::drop_connections_after`]).
    drop_after: Option<usize>,
}

/// What happens to the fragments of changes on their way from the hub (the server's log) to a machine, from a seeded
/// generator: before some fragment after a set's first, the machine's link stops for a while (the laptop sleeps
/// part of the way through the change, or its connection drops), and the rest of the log comes later. The machine
/// keeps what came, as the agent does, and its apps get the change whole once the rest is there, never part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentFaults {
    pub seed: u64,
    /// Of every 1000 fragments after a set's first, before how many the machine sleeps ([`SLEEP_TICKS`] ticks).
    pub sleep: u32,
    /// Of every 1000 fragments after a set's first, before how many the link stops a few ticks.
    pub delay: u32,
    /// The most ticks such a stop lasts.
    pub max_delay: u32,
}

/// How many [`Hub::tick`]s a machine sleeps inside a change ([`FragmentFaults::sleep`]).
pub const SLEEP_TICKS: u32 = 20;

struct Faults {
    spec: FragmentFaults,
    state: u64,
}

impl Faults {
    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut x = self.state;
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }

    /// For one fragment: how many ticks the link stops before it (0: it comes now).
    fn fate(&mut self, bytes: &[u8]) -> u32 {
        if parse(bytes).is_none_or(|f| f.index == 0) {
            return 0;
        }
        let roll = u32::try_from(self.next() % 1000).unwrap_or(0);
        if roll < self.spec.sleep {
            SLEEP_TICKS
        } else if roll < self.spec.sleep.saturating_add(self.spec.delay) {
            1 + u32::try_from(self.next() % u64::from(self.spec.max_delay.max(1))).unwrap_or(0)
        } else {
            0
        }
    }
}

/// A row of a machine's log on its way to an app, joined as the agent joins: a change in fragments is kept (in
/// `kept`, by set) until it is whole, and goes whole; anything else goes as it is. `None` while it isn't whole (or
/// for a fragment that doesn't read).
fn joined_row(kept: &mut HashMap<SetId, Vec<Vec<u8>>>, bytes: &[u8]) -> Option<Vec<u8>> {
    if !is_fragment(bytes) {
        return Some(bytes.to_vec());
    }
    let f = parse(bytes)?;
    let set = kept.entry(f.set).or_default();
    set.push(bytes.to_vec());
    let mut indexes: Vec<u16> = set.iter().filter_map(|b| parse(b).map(|x| x.index)).collect();
    indexes.sort_unstable();
    indexes.dedup();
    if indexes.len() < usize::from(f.count) {
        return None;
    }
    let mut frames = kept.remove(&f.set)?;
    frames.sort_by_key(|b| parse(b).map(|x| x.index));
    frames.dedup_by_key(|b| parse(b).map(|x| x.index));
    join(frames.iter().map(Vec::as_slice))
}

/// A live document's frontiers: `(peer, counter)` heads.
type Version = Vec<(u64, u32)>;

/// One app connection to the docs socket.
struct Conn {
    writer: Arc<Mutex<WebSocket<TcpStream>>>,
    stream: TcpStream,
    /// Each document number opened: the document, how much of the machine's copy went out, and the app's updates.
    open: HashMap<u64, Opened>,
    /// A `needs_import` waiting for its import, by number: the path, and the document it will be.
    importing: HashMap<u64, (String, DocId)>,
    /// The hash (hex) of the file each open read, by number.
    apps: HashMap<u64, Option<String>>,
    told: HashMap<DocId, (bool, u64)>,
    /// The file record each document's apps were last told.
    told_file: HashMap<DocId, Option<Version>>,
    /// The fragments of the app's changes still coming, by document number and set: with their numbers. A change goes
    /// into the machine's copy (and is acked) only once it is whole, as the agent takes it.
    sets: HashMap<(u64, SetId), Vec<Numbered>>,
}

/// An app's update frame and its number.
type Numbered = (u64, Vec<u8>);

struct Opened {
    doc: DocId,
    sent: usize,
    seq: u64,
    /// The fragments of the log's changes not whole yet, by set: the app gets each change whole ([`joined_row`]).
    kept: HashMap<SetId, Vec<Vec<u8>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new machine of this account, signed in and online, its local API listening on a loopback port.
    pub fn machine(&self, name: &str) -> std::io::Result<Fake> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?;
        let token = format!("fake-{}-{}", name, addr.port());
        let id = {
            let mut inner = lock(&self.inner);
            let id = inner.next_machine;
            inner.next_machine += 1;
            inner.machines.insert(
                id,
                Machine {
                    name: name.to_owned(),
                    token: token.clone(),
                    online: true,
                    running: true,
                    too_old: false,
                    docs: HashMap::new(),
                    blobs: HashMap::new(),
                    pending_blobs: Vec::new(),
                    conns: HashMap::new(),
                    saves: Vec::new(),
                    refuse: VecDeque::new(),
                    file_pending: HashMap::new(),
                    faults: None,
                    pauses: 0,
                    drop_after: None,
                },
            );
            id
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (hub, stopping) = (self.clone(), stop.clone());
        std::thread::Builder::new().name(format!("fake-agent-{name}")).spawn(move || {
            for stream in listener.incoming() {
                if stopping.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let hub = hub.clone();
                let _ = std::thread::Builder::new().name("fake-agent-conn".into()).spawn(move || hub.serve(id, stream));
            }
        })?;
        Ok(Fake { hub: self.clone(), id, addr, token, stop })
    }

    /// Another file takes `path`'s place (the old one renamed away while agents were down): with `Some(import)`, a
    /// live document of its own (a new ID); with `None`, one that was never live. Every machine forgets its copy of
    /// the old one there.
    pub fn replace(&self, path: &str, import: Option<Vec<u8>>) {
        let mut inner = lock(&self.inner);
        match import {
            Some(import) => {
                inner.docs.insert(path.to_owned(), HubDoc { id: DocId::generate(), import, log: Vec::new() });
            }
            None => {
                inner.docs.remove(path);
            }
        }
        for m in inner.machines.values_mut() {
            m.docs.retain(|_, d| d.path != path);
        }
    }

    /// Whether live updates go between machines (`false`: a proxy that blocks WebSockets, the relay out) while the
    /// file sync goes on (saved files' records still reach every machine).
    pub fn set_live(&self, on: bool) {
        let mut inner = lock(&self.inner);
        inner.live_down = !on;
        sync(&mut inner);
    }

    /// Another program writes `path`'s file (no save record): every machine's apps hear it has none.
    pub fn edit_outside(&self, path: &str) {
        let mut inner = lock(&self.inner);
        inner.files.insert(path.to_owned(), None);
        sync(&mut inner);
    }

    /// The record of `path`'s file on the server: the live document's version a Craft app saved it from.
    pub fn file(&self, path: &str) -> Option<Vec<(u64, u32)>> {
        lock(&self.inner).files.get(path).cloned().flatten()
    }

    /// How many updates the hub holds of the document at `path` (0 when it has none).
    pub fn updates(&self, path: &str) -> usize {
        lock(&self.inner).docs.get(path).map_or(0, |d| d.log.len())
    }

    /// A tick: a machine's link stopped inside a change ([`FragmentFaults`]) comes one tick closer to going on, and the
    /// rows of the ones due come.
    pub fn tick(&self) {
        let mut inner = lock(&self.inner);
        for m in inner.machines.values_mut() {
            for d in m.docs.values_mut() {
                d.pause = d.pause.saturating_sub(1);
            }
        }
        sync(&mut inner);
    }

    fn serve(&self, machine: usize, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let (token, running, too_old, name) = {
            let inner = lock(&self.inner);
            let Some(m) = inner.machines.get(&machine) else { return };
            (m.token.clone(), m.running, m.too_old, m.name.clone())
        };
        // An agent from before blobs answers an unknown call from its fallback without reading the body.
        let Ok(Some(request)) = read_request(&mut stream, !too_old) else { return };
        if too_old && request.path.starts_with(paths::BLOBS) {
            respond_error(&mut stream, 404, "not_found", "no such call");
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        if !running {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        if request.auth.as_deref() != Some(&format!("Bearer {token}")) {
            return respond_error(&mut stream, 401, "unauthorized", "the token is wrong");
        }
        let blob_get = request.path.strip_prefix(paths::BLOBS).and_then(|p| p.strip_prefix('/')).map(str::to_owned);
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", paths::STATUS) => {
                let online = lock(&self.inner).machines.get(&machine).is_some_and(|m| m.online);
                let status = Status {
                    api: LOCAL_API_MAX,
                    signed_in: true,
                    account: Some("fake".into()),
                    server: Some("https://fake.invalid".into()),
                    folder: None,
                    machine: Some(name),
                    online,
                    run_state: RunState::Idle,
                    autostart: None,
                    last_sync_ms: None,
                    removed: false,
                };
                respond_json(&mut stream, 200, &status);
            }
            ("GET", paths::DOCS) => self.docs_socket(machine, stream, &request),
            ("PUT", paths::BLOBS) | ("GET", _) if too_old && (request.path == paths::BLOBS || blob_get.is_some()) => {
                respond_error(&mut stream, 404, "not_found", "no such call");
            }
            ("PUT", paths::BLOBS) => match self.put_blob(machine, &request.body) {
                Ok(stored) => respond_json(&mut stream, 200, &stored),
                Err((status, code, message)) => respond_error(&mut stream, status, code, &message),
            },
            ("GET", _) if blob_get.is_some() => {
                let hash = blob_get.as_deref().and_then(parse_hex);
                match hash.map(|h| self.get_blob(machine, &h)) {
                    Some(Ok(bytes)) => respond(&mut stream, 200, "application/octet-stream", &bytes),
                    Some(Err((status, code, message))) => respond_error(&mut stream, status, code, &message),
                    None => respond_error(&mut stream, 400, "invalid", "not a blob's name"),
                }
            }
            _ => respond_error(&mut stream, 404, "not_found", "no such call"),
        }
    }

    fn put_blob(&self, machine: usize, bytes: &[u8]) -> Result<Stored, (u16, &'static str, String)> {
        if bytes.is_empty() || bytes.len() > MAX_BLOB_BYTES {
            return Err((413, "too_large", format!("a blob is 1 byte to {MAX_BLOB_BYTES} bytes")));
        }
        let hash = fake_hash(bytes);
        let mut inner = lock(&self.inner);
        let Some(m) = inner.machines.get_mut(&machine) else { return Err((500, "internal", "no machine".into())) };
        m.blobs.insert(hash, bytes.to_vec());
        m.pending_blobs.push(hash);
        sync(&mut inner);
        Ok(Stored { hash, len: bytes.len() as u64 })
    }

    fn get_blob(&self, machine: usize, hash: &[u8; 32]) -> Result<Vec<u8>, (u16, &'static str, String)> {
        let mut inner = lock(&self.inner);
        let hub_copy = inner.blobs.get(hash).cloned();
        let Some(m) = inner.machines.get_mut(&machine) else { return Err((500, "internal", "no machine".into())) };
        if let Some(bytes) = m.blobs.get(hash) {
            return Ok(bytes.clone());
        }
        if !m.online {
            return Err((503, "offline", "the server can't be reached to fetch the blob".into()));
        }
        match hub_copy {
            Some(bytes) => {
                m.blobs.insert(*hash, bytes.clone());
                Ok(bytes)
            }
            None => Err((404, "blob_not_found", "no such blob".into())),
        }
    }

    /// The docs WebSocket: the handshake by hand, then a reader here and writes from wherever state changes.
    fn docs_socket(&self, machine: usize, mut stream: TcpStream, request: &Request) {
        let Some(key) = request.ws_key.as_deref() else {
            return respond_error(&mut stream, 400, "invalid", "not a WebSocket request");
        };
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        let head = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(None);
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        let limit = MAX_BINARY_BYTES.max(MAX_LOCAL_JSON_BYTES);
        let config = WebSocketConfig::default().max_message_size(Some(limit)).max_frame_size(Some(limit));
        let (Ok(write_side), Ok(shut)) = (stream.try_clone(), stream.try_clone()) else { return };
        let mut reader = WebSocket::from_raw_socket(stream, Role::Server, Some(config));
        let writer = Arc::new(Mutex::new(WebSocket::from_raw_socket(write_side, Role::Server, Some(config))));
        let conn = {
            let mut inner = lock(&self.inner);
            let conn = inner.next_conn;
            inner.next_conn += 1;
            let Some(m) = inner.machines.get_mut(&machine) else { return };
            m.conns.insert(
                conn,
                Conn {
                    writer,
                    stream: shut,
                    open: HashMap::new(),
                    importing: HashMap::new(),
                    apps: HashMap::new(),
                    told: HashMap::new(),
                    told_file: HashMap::new(),
                    sets: HashMap::new(),
                },
            );
            conn
        };
        loop {
            match reader.read() {
                Ok(Message::Text(text)) => {
                    if let Ok(request) = serde_json::from_str::<DocsRequest>(text.as_str()) {
                        self.on_request(machine, conn, request);
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    if let Some((kind, id, body)) = decode_binary(&bytes) {
                        self.on_binary(machine, conn, kind, id, body);
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let mut inner = lock(&self.inner);
        if let Some(m) = inner.machines.get_mut(&machine) {
            m.conns.remove(&conn);
        }
    }

    fn on_request(&self, machine: usize, conn: u64, request: DocsRequest) {
        let mut inner = lock(&self.inner);
        match request {
            DocsRequest::Open { id, path, app: _, file_hash } => {
                if let Some(c) = inner.machines.get_mut(&machine).and_then(|m| m.conns.get_mut(&conn)) {
                    c.apps.insert(id, file_hash);
                }
                open(&mut inner, machine, conn, id, &path);
            }
            DocsRequest::Close { doc } => {
                if let Some(c) = inner.machines.get_mut(&machine).and_then(|m| m.conns.get_mut(&conn)) {
                    c.open.retain(|_, o| o.doc != doc);
                }
            }
            DocsRequest::Saved { doc, hash, frontiers } => {
                let Some(m) = inner.machines.get_mut(&machine) else { return };
                m.saves.push((doc, hash, frontiers.clone()));
                // The file is written and goes up at once (offline: once the machine is back). (The fake reads no
                // Loro: the record comes after the changes it covers on the app's stream, which the copy took first.)
                let Some(path) = m.docs.get(&doc).map(|d| d.path.clone()) else { return };
                m.file_pending.insert(path, frontiers);
                sync(&mut inner);
            }
            // (The fake's file sync is the records themselves: nothing waits for a save on its way.)
            DocsRequest::Saving { .. } => {}
        }
    }

    fn on_binary(&self, machine: usize, conn: u64, kind: u8, id: u64, body: &[u8]) {
        let mut inner = lock(&self.inner);
        match kind {
            IMPORT => {
                let Some((path, new_id)) =
                    inner.machines.get_mut(&machine).and_then(|m| m.conns.get_mut(&conn)).and_then(|c| c.importing.remove(&id))
                else {
                    return;
                };
                let online = inner.machines.get(&machine).is_some_and(|m| m.online);
                if !online {
                    return tell(&inner, machine, conn, &DocsEvent::NotLive { id, reason: NotLiveReason::Offline, message: None });
                }
                // The first import is kept; a later one joins it.
                inner.docs.entry(path.clone()).or_insert_with(|| HubDoc { id: new_id, import: body.to_vec(), log: Vec::new() });
                open(&mut inner, machine, conn, id, &path);
            }
            UPDATE => {
                let Some(m) = inner.machines.get_mut(&machine) else { return };
                // An agent that stops after so many updates ([`Fake::drop_connections_after`]): this one never
                // arrives, and every app connection ends.
                if let Some(left) = m.drop_after.as_mut() {
                    if *left == 0 {
                        m.drop_after = None;
                        for (_, c) in m.conns.drain() {
                            let _ = c.stream.shutdown(Shutdown::Both);
                        }
                        return;
                    }
                    *left -= 1;
                }
                let Some(c) = m.conns.get_mut(&conn) else { return };
                let Some(opened) = c.open.get_mut(&id) else { return };
                opened.seq += 1;
                let (seq, doc) = (opened.seq, opened.doc);
                if body.is_empty() || body.len() > MAX_UPDATE_BYTES {
                    let message = format!("an update is 1 byte to {MAX_UPDATE_BYTES} bytes");
                    return tell(&inner, machine, conn, &DocsEvent::Error { id: Some(id), seq: Some(seq), code: "too_large".into(), message });
                }
                // A fragment: kept until its change is whole, which then goes into the copy as its fragments (each an
                // update of the log, as the agent's log keeps them) and is acked fragment by fragment.
                let frames: Vec<(u64, Vec<u8>)> = if is_fragment(body) {
                    let Some(f) = parse(body) else {
                        let message = "a fragment that doesn't read".to_owned();
                        return tell(&inner, machine, conn, &DocsEvent::Error { id: Some(id), seq: Some(seq), code: "invalid".into(), message });
                    };
                    let set = c.sets.entry((id, f.set)).or_default();
                    set.push((seq, body.to_vec()));
                    let mut indexes: Vec<u16> = set.iter().filter_map(|(_, b)| parse(b).map(|x| x.index)).collect();
                    indexes.sort_unstable();
                    indexes.dedup();
                    if indexes.len() < usize::from(f.count) {
                        return;
                    }
                    let mut frames = c.sets.remove(&(id, f.set)).unwrap_or_default();
                    if join(frames.iter().map(|(_, b)| b.as_slice())).is_none() {
                        for (seq, _) in frames {
                            // (Each fragment's number is refused: the app hears it by the last one's.)
                            let message = "fragments that don't make the change they name".to_owned();
                            tell(&inner, machine, conn, &DocsEvent::Error { id: Some(id), seq: Some(seq), code: "invalid".into(), message });
                        }
                        return;
                    }
                    frames.sort_by_key(|(_, b)| parse(b).map(|x| x.index));
                    frames.dedup_by_key(|(_, b)| parse(b).map(|x| x.index));
                    frames
                } else {
                    vec![(seq, body.to_vec())]
                };
                // A refusal asked for ([`Fake::refuse_next`]): the change isn't kept.
                if let Some(code) = inner.machines.get_mut(&machine).and_then(|m| m.refuse.pop_front()) {
                    for (seq, _) in &frames {
                        let message = format!("refused as asked ({code})");
                        tell(&inner, machine, conn, &DocsEvent::Error { id: Some(id), seq: Some(*seq), code: code.clone(), message });
                    }
                    return;
                }
                let Some(m) = inner.machines.get_mut(&machine) else { return };
                if let Some(d) = m.docs.get_mut(&doc) {
                    for (_, bytes) in &frames {
                        d.log.push((Origin::Conn(conn), bytes.clone()));
                        d.pending.push_back(d.log.len() - 1);
                    }
                }
                for (seq, _) in &frames {
                    tell(&inner, machine, conn, &DocsEvent::Ack { id, seq: *seq });
                }
                sync(&mut inner);
            }
            PRESENCE => {
                if body.len() > MAX_PRESENCE_BYTES {
                    return;
                }
                let Some(doc) = inner.machines.get(&machine).and_then(|m| m.conns.get(&conn)).and_then(|c| c.open.get(&id)).map(|o| o.doc) else {
                    return;
                };
                let from_online = inner.machines.get(&machine).is_some_and(|m| m.online);
                for (mid, m) in &inner.machines {
                    if *mid != machine && !(m.online && from_online) {
                        continue;
                    }
                    for (cid, c) in &m.conns {
                        if (*mid, *cid) == (machine, conn) {
                            continue;
                        }
                        for (number, o) in &c.open {
                            if o.doc == doc {
                                let _ = lock(&c.writer).send(Message::Binary(encode_binary(PRESENCE, *number, body).into()));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Opens the document at `path` on `machine` for `conn` under `id` (the machine takes the hub's copy if it has none).
fn open(inner: &mut Inner, machine: usize, conn: u64, id: u64, path: &str) {
    let hub_doc = inner.docs.get(path).map(|d| (d.id, d.import.clone()));
    let Some(m) = inner.machines.get_mut(&machine) else { return };
    let known = m.docs.iter().find(|(_, d)| d.path == path).map(|(doc, _)| *doc);
    let doc = match (known, hub_doc) {
        (Some(doc), _) => doc,
        (None, Some((doc, import))) if m.online => {
            m.docs.insert(
                doc,
                MachineDoc { path: path.to_owned(), import, log: Vec::new(), hub_seen: 0, pending: VecDeque::new(), pause: 0, paused_at: None },
            );
            doc
        }
        (None, None) if m.online => {
            let new_id = DocId::generate();
            if let Some(c) = m.conns.get_mut(&conn) {
                c.importing.insert(id, (path.to_owned(), new_id));
            }
            return tell(inner, machine, conn, &DocsEvent::NeedsImport { id });
        }
        (None, _) => return tell(inner, machine, conn, &DocsEvent::NotLive { id, reason: NotLiveReason::Offline, message: None }),
    };
    // Take what the hub has first (when online), so the state is as fresh as it can be.
    sync(inner);
    // Edited outside Craft unless some machine's app saved that content (the server's versions keep every record).
    let file_hash = inner.machines.get(&machine).and_then(|m| m.conns.get(&conn)).and_then(|c| c.apps.get(&id)).and_then(Clone::clone);
    let changed_outside = file_hash.map(|h| !inner.machines.values().any(|m| m.saves.iter().any(|(d, saved, _)| *d == doc && *saved == h)));
    let Some(m) = inner.machines.get_mut(&machine) else { return };
    let online = m.online;
    let Some(d) = m.docs.get(&doc) else { return };
    // (Its changes whole: one whose fragments aren't all here yet is kept, and goes once it is.)
    let mut kept = HashMap::new();
    let log: Vec<Vec<u8>> = d.log.iter().filter_map(|(_, b)| joined_row(&mut kept, b)).collect();
    let state = DocState { online, waiting: d.pending.len() as u64, bytes: d.import.len() as u64, updates: log.len() as u64 };
    let (import, rows) = (d.import.clone(), d.log.len());
    let Some(c) = m.conns.get_mut(&conn) else { return };
    c.open.insert(id, Opened { doc, sent: rows, seq: 0, kept });
    c.told.insert(doc, (online, state.waiting));
    let mut w = lock(&c.writer);
    let opened = DocsEvent::Opened { id, doc, state, changed_outside };
    if let Ok(text) = serde_json::to_string(&opened) {
        let _ = w.send(Message::Text(text.into()));
    }
    let _ = w.send(Message::Binary(encode_binary(STATE, id, &import).into()));
    for bytes in log {
        let _ = w.send(Message::Binary(encode_binary(UPDATE, id, &bytes).into()));
    }
    drop(w);
    // The file's record, after the state.
    sync(inner);
}

/// Sends one control message to one connection.
fn tell(inner: &Inner, machine: usize, conn: u64, event: &DocsEvent) {
    let Some(c) = inner.machines.get(&machine).and_then(|m| m.conns.get(&conn)) else { return };
    if let Ok(text) = serde_json::to_string(event) {
        let _ = lock(&c.writer).send(Message::Text(text.into()));
    }
}

/// Moves updates and blobs between the online machines and the hub, then sends every connection what it lacks.
fn sync(inner: &mut Inner) {
    let live_down = inner.live_down;
    let Inner { docs, blobs, machines, files, .. } = inner;
    // Up: every online machine's waiting updates, blobs and saved files.
    let mut ids: Vec<usize> = machines.keys().copied().collect();
    ids.sort_unstable();
    for mid in &ids {
        let Some(m) = machines.get_mut(mid) else { continue };
        if !m.online {
            continue;
        }
        for (path, version) in m.file_pending.drain() {
            files.insert(path, Some(version));
        }
        for hash in m.pending_blobs.drain(..) {
            if let Some(bytes) = m.blobs.get(&hash) {
                blobs.insert(hash, bytes.clone());
            }
        }
        for d in m.docs.values_mut() {
            let Some(hub) = docs.get_mut(&d.path) else { continue };
            if live_down {
                continue;
            }
            while let Some(at) = d.pending.pop_front() {
                if let Some((_, bytes)) = d.log.get(at) {
                    hub.log.push((*mid, bytes.clone()));
                }
            }
        }
    }
    // Down: what the others sent, in the log's order. The link may stop inside a change in fragments
    // ([`FragmentFaults`]): the rows from there on come once the pause is over.
    for mid in &ids {
        let Some(m) = machines.get_mut(mid) else { continue };
        if !m.online {
            continue;
        }
        let Machine { docs: machine_docs, faults, pauses, .. } = m;
        for d in machine_docs.values_mut() {
            let Some(hub) = docs.get(&d.path) else { continue };
            if live_down || d.pause > 0 {
                continue;
            }
            while let Some((from, bytes)) = hub.log.get(d.hub_seen) {
                if from != mid {
                    if d.paused_at != Some(d.hub_seen)
                        && let Some(ticks) = faults.as_mut().map(|f| f.fate(bytes)).filter(|t| *t > 0)
                    {
                        d.pause = ticks;
                        d.paused_at = Some(d.hub_seen);
                        *pauses += 1;
                        break;
                    }
                    d.log.push((Origin::Remote, bytes.clone()));
                }
                d.hub_seen += 1;
            }
        }
    }
    // Out to the apps: the copy's new updates (an app's own not back to it), each change whole (as the agent joins
    // the server's fragments before its apps get a change), and status when it changed.
    for m in machines.values_mut() {
        let online = m.online;
        let Machine { conns, docs: machine_docs, file_pending, .. } = m;
        for (cid, c) in conns.iter_mut() {
            let mut w = lock(&c.writer);
            for (number, o) in c.open.iter_mut() {
                let Some(d) = machine_docs.get(&o.doc) else { continue };
                for (origin, bytes) in d.log.iter().skip(o.sent) {
                    if *origin == Origin::Conn(*cid) {
                        continue;
                    }
                    if let Some(update) = joined_row(&mut o.kept, bytes) {
                        let _ = w.send(Message::Binary(encode_binary(UPDATE, *number, &update).into()));
                    }
                }
                o.sent = d.log.len();
                let now = (online, d.pending.len() as u64);
                if c.told.get(&o.doc) != Some(&now) {
                    c.told.insert(o.doc, now);
                    let status = DocsEvent::Status { doc: o.doc, online, waiting: now.1, deleted: false, stopped: false };
                    if let Ok(text) = serde_json::to_string(&status) {
                        let _ = w.send(Message::Text(text.into()));
                    }
                }
                // The file here: this machine's save not up yet, else the server's (offline: as it was).
                let file = match file_pending.get(&d.path) {
                    Some(v) => Some(v.clone()),
                    None if online => files.get(&d.path).cloned().flatten(),
                    None => c.told_file.get(&o.doc).cloned().flatten(),
                };
                if c.told_file.get(&o.doc) != Some(&file) {
                    c.told_file.insert(o.doc, file.clone());
                    if let Ok(text) = serde_json::to_string(&DocsEvent::File { doc: o.doc, frontiers: file }) {
                        let _ = w.send(Message::Text(text.into()));
                    }
                }
            }
        }
    }
}

/// One machine of a [`Hub`]. Dropping it stops its local API (its app connections end).
pub struct Fake {
    hub: Hub,
    id: usize,
    addr: SocketAddr,
    token: String,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for Fake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fake").field("addr", &self.addr).finish_non_exhaustive()
    }
}

impl Fake {
    /// A client of this machine's agent, as [`Client::connect`] gives one.
    pub fn client(&self) -> Client {
        Client::at(self.addr, self.token.clone(), std::env::temp_dir())
    }

    /// The machine reaches the hub (`true`) or not: offline, its updates and blobs wait, and others' don't come.
    pub fn set_online(&self, online: bool) {
        let mut inner = lock(&self.hub.inner);
        if let Some(m) = inner.machines.get_mut(&self.id) {
            m.online = online;
        }
        sync(&mut inner);
    }

    /// Whether this machine answers the local API ([`Fake::restart`] stops it until [`Fake::start`]).
    pub fn running(&self) -> bool {
        lock(&self.hub.inner).machines.get(&self.id).is_some_and(|m| m.running)
    }

    /// The agent stops: every app connection ends, and new ones are refused until [`Fake::start`]. What the machine
    /// holds (its copies, its waiting updates) stays, as the agent's files do.
    pub fn restart(&self) {
        let mut inner = lock(&self.hub.inner);
        if let Some(m) = inner.machines.get_mut(&self.id) {
            m.running = false;
            for (_, c) in m.conns.drain() {
                let _ = c.stream.shutdown(Shutdown::Both);
            }
        }
    }

    /// The agent runs again.
    pub fn start(&self) {
        if let Some(m) = lock(&self.hub.inner).machines.get_mut(&self.id) {
            m.running = true;
        }
    }

    /// Every app connection ends (the agent dropped them), but the agent keeps running.
    pub fn drop_connections(&self) {
        if let Some(m) = lock(&self.hub.inner).machines.get_mut(&self.id) {
            for (_, c) in m.conns.drain() {
                let _ = c.stream.shutdown(Shutdown::Both);
            }
        }
    }

    /// From now on (`Some`), this machine's link from the hub stops inside changes in fragments as `faults` say
    /// ([`Hub::tick`] moves the pauses on); `None` stops it, and ends a pause under way (the machine wakes).
    pub fn set_fragment_faults(&self, faults: Option<FragmentFaults>) {
        let mut inner = lock(&self.hub.inner);
        if let Some(m) = inner.machines.get_mut(&self.id) {
            if faults.is_none() {
                for d in m.docs.values_mut() {
                    d.pause = 0;
                }
            }
            m.faults = faults.map(|spec| Faults { spec, state: spec.seed });
        }
        sync(&mut inner);
    }

    /// After `updates` more update frames from its apps, the agent drops every app connection (as when it stops or
    /// crashes): that frame never arrives, and a change whose fragments were coming is never taken (the app sends it
    /// again once it is back).
    pub fn drop_connections_after(&self, updates: usize) {
        if let Some(m) = lock(&self.hub.inner).machines.get_mut(&self.id) {
            m.drop_after = Some(updates);
        }
    }

    /// Refuses the next `count` updates any app of this machine sends with `code`, as the agent does: `internal` when
    /// it can't write a change to disk (worth sending again), `invalid` when Loro doesn't take it (never).
    pub fn refuse_next(&self, code: &str, count: usize) {
        if let Some(m) = lock(&self.hub.inner).machines.get_mut(&self.id) {
            m.refuse.extend(std::iter::repeat_n(code.to_owned(), count));
        }
    }

    /// Answer the blob calls as an agent from before blobs.
    pub fn too_old(&self) {
        if let Some(m) = lock(&self.hub.inner).machines.get_mut(&self.id) {
            m.too_old = true;
        }
    }

    /// The save records this machine's apps sent: content hash (hex) and frontiers, in order.
    pub fn saves(&self) -> Vec<(String, Vec<(u64, u32)>)> {
        lock(&self.hub.inner).machines.get(&self.id).map(|m| m.saves.iter().map(|(_, h, v)| (h.clone(), v.clone())).collect()).unwrap_or_default()
    }

    /// How many of this machine's updates of the document at `path` the hub doesn't have yet.
    pub fn waiting(&self, path: &str) -> usize {
        let inner = lock(&self.hub.inner);
        inner.machines.get(&self.id).map_or(0, |m| m.docs.values().filter(|d| d.path == path).map(|d| d.pending.len()).sum())
    }

    /// How many times this machine's link stopped inside a change in fragments ([`FragmentFaults`]).
    pub fn pauses(&self) -> u64 {
        lock(&self.hub.inner).machines.get(&self.id).map_or(0, |m| m.pauses)
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(m) = lock(&self.hub.inner).machines.get_mut(&self.id) {
            m.running = false;
            for (_, c) in m.conns.drain() {
                let _ = c.stream.shutdown(Shutdown::Both);
            }
        }
        // Wake the accept loop so it sees the stop.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
    }
}

/// A content name for the fake's blobs: 32 bytes, the same for the same bytes (not a cryptographic hash; the real
/// agent names blobs by their manifest's BLAKE3 hash).
fn fake_hash(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (lane, slot) in out.chunks_mut(8).enumerate() {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (lane as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3 ^ ((lane as u64) << 40));
        }
        h ^= bytes.len() as u64;
        slot.copy_from_slice(&h.to_le_bytes());
    }
    out
}

struct Request {
    method: String,
    path: String,
    auth: Option<String>,
    ws_key: Option<String>,
    body: Vec<u8>,
}

/// One HTTP request: its head (at most 16 KiB) and, with `body`, its body (by `Content-Length`, at most the blob
/// limit).
fn read_request(stream: &mut TcpStream, body: bool) -> std::io::Result<Option<Request>> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut parsed = httparse::Request::new(&mut headers);
        match parsed.parse(&buf) {
            Ok(httparse::Status::Complete(head)) => {
                let header = |name: &str| {
                    parsed
                        .headers
                        .iter()
                        .find(|h| h.name.eq_ignore_ascii_case(name))
                        .and_then(|h| std::str::from_utf8(h.value).ok())
                        .map(str::to_owned)
                };
                let length = header("content-length").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                if length > MAX_BLOB_BYTES {
                    return Ok(None);
                }
                let mut request = Request {
                    method: parsed.method.unwrap_or_default().to_owned(),
                    path: parsed.path.unwrap_or_default().to_owned(),
                    auth: header("authorization"),
                    ws_key: header("sec-websocket-key"),
                    body: buf.split_off(head.min(buf.len())),
                };
                if !body {
                    return Ok(Some(request));
                }
                while request.body.len() < length {
                    let n = stream.read(&mut chunk)?;
                    if n == 0 {
                        return Ok(None);
                    }
                    request.body.extend_from_slice(chunk.get(..n).unwrap_or_default());
                }
                request.body.truncate(length);
                return Ok(Some(request));
            }
            Ok(httparse::Status::Partial) if buf.len() <= 16 * 1024 => {}
            _ => return Ok(None),
        }
    }
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let head = format!("HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn respond_json(stream: &mut TcpStream, status: u16, value: &impl serde::Serialize) {
    let body = serde_json::to_vec(value).unwrap_or_default();
    respond(stream, status, "application/json", &body);
}

fn respond_error(stream: &mut TcpStream, status: u16, code: &str, message: &str) {
    respond_json(stream, status, &serde_json::json!({ "error": code, "message": message }));
}

/// The blob name a fake gives `bytes`, for tests that compare.
pub fn blob_name(bytes: &[u8]) -> String {
    hex(&fake_hash(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{Event, Opened};

    fn next_update(doc: &crate::live::Document) -> Vec<u8> {
        for _ in 0..200 {
            if let Some(Event::Update(bytes)) = doc.recv_timeout(Duration::from_millis(25)) {
                return bytes;
            }
        }
        panic!("no update came");
    }

    /// Whether no update comes for `doc` within about 200 ms.
    fn no_update(doc: &crate::live::Document) -> bool {
        (0..8).all(|_| !matches!(doc.recv_timeout(Duration::from_millis(25)), Some(Event::Update(_))))
    }

    fn acked(doc: &crate::live::Document, seq: u64) {
        for _ in 0..200 {
            if let Some(Event::Acked(s)) = doc.recv_timeout(Duration::from_millis(25))
                && s == seq
            {
                return;
            }
        }
        panic!("no ack came");
    }

    /// Two machines of one hub: an import on one is live on the other; updates go both ways, and wait while a machine
    /// is offline; a restarted machine keeps what it held; blobs go up and come down; an old agent's blob calls are
    /// `agent_too_old`.
    #[test]
    fn two_fakes_share_documents_and_blobs() {
        let hub = Hub::new();
        let (a, b) = (hub.machine("a").unwrap(), hub.machine("b").unwrap());
        let docs_a = a.client().documents().unwrap();
        let Opened::NeedsImport(importer) = docs_a.open("x.txt", "test").unwrap() else { panic!("not new") };
        let doc_a = importer.import(b"snapshot").unwrap();
        assert_eq!(doc_a.state(), b"snapshot");
        let docs_b = b.client().documents().unwrap();
        let Opened::Live(doc_b) = docs_b.open("x.txt", "test").unwrap() else { panic!("not live on b") };
        assert_eq!(doc_b.state(), b"snapshot");
        doc_a.send(b"u1").unwrap();
        assert_eq!(next_update(&doc_b), b"u1");
        b.set_online(false);
        acked(&doc_b, doc_b.send(b"u2").unwrap());
        acked(&doc_a, doc_a.send(b"u3").unwrap());
        assert_eq!(b.waiting("x.txt"), 1);
        b.set_online(true);
        assert_eq!(next_update(&doc_a), b"u2");
        assert_eq!(next_update(&doc_b), b"u3");
        assert_eq!(hub.updates("x.txt"), 3);
        // A restart: b's connections end; it keeps its copy.
        b.set_online(false);
        acked(&doc_b, doc_b.send(b"u4").unwrap());
        b.restart();
        assert!(b.client().documents().is_err() || !b.running());
        b.start();
        let docs_b = b.client().documents().unwrap();
        let Opened::Live(again) = docs_b.open("x.txt", "test").unwrap() else { panic!("not live after the restart") };
        assert_eq!(again.opened_state().waiting, 1);
        // Blobs.
        let stored = a.client().blobs().put(b"blob bytes").unwrap();
        assert_eq!(
            b.client().blobs().get(&stored.hash, stored.len).unwrap_err(),
            crate::Error::Refused { code: "offline".into(), message: "the server can't be reached to fetch the blob".into() }
        );
        b.set_online(true);
        assert_eq!(b.client().blobs().get(&stored.hash, stored.len).unwrap(), b"blob bytes");
        assert!(matches!(b.client().blobs().get(&[1; 32], 3), Err(crate::Error::Refused { code, .. }) if code == "blob_not_found"));
        b.too_old();
        assert!(matches!(b.client().blobs().put(b"x"), Err(crate::Error::Refused { code, .. }) if code == "agent_too_old"));
        // A big put too: an old agent answers before reading the body, and the client asks before sending it.
        let put = b.client().blobs().put(&vec![9u8; 8 * 1024 * 1024]);
        assert!(matches!(&put, Err(crate::Error::Refused { code, .. }) if code == "agent_too_old"), "{put:?}");
        assert!(matches!(b.client().blobs().get(&stored.hash, stored.len), Err(crate::Error::Refused { code, .. }) if code == "agent_too_old"));
        assert!(b.client().status().is_ok(), "the rest still works");
        // An update over 1 MiB goes in fragments: it comes whole, acked once; one over a snapshot's limit isn't sent.
        let big: Vec<u8> = (0..3 * MAX_UPDATE_BYTES).map(|i| (i % 253) as u8).collect();
        acked(&doc_a, doc_a.send(&big).unwrap());
        assert_eq!(next_update(&again), big);
        let fragments = craft_cloud_api::fragments::split(&big, MAX_UPDATE_BYTES).unwrap().len();
        assert_eq!(fragments, 4);
        assert_eq!(hub.updates("x.txt"), 4 + fragments, "its fragments, each an update of the log");
        let huge = vec![0u8; craft_cloud_api::fragments::MAX_WHOLE_BYTES + 1];
        assert!(matches!(doc_a.send(&huge), Err(crate::Error::Refused { code, .. }) if code == "too_large"));
    }

    /// A machine whose link stops inside a change in fragments keeps what came: its apps get nothing of the change
    /// (nor what comes after it in the log) until the rest is there, then the change whole; an app opening the
    /// document meanwhile gets it whole once it is, never a fragment.
    #[test]
    fn a_machine_that_sleeps_inside_a_change_gives_its_apps_the_change_whole() {
        let hub = Hub::new();
        let (a, b) = (hub.machine("a").unwrap(), hub.machine("b").unwrap());
        let docs_a = a.client().documents().unwrap();
        let Opened::NeedsImport(importer) = docs_a.open("f.txt", "test").unwrap() else { panic!("not new") };
        let doc_a = importer.import(b"snapshot").unwrap();
        let docs_b = b.client().documents().unwrap();
        let Opened::Live(doc_b) = docs_b.open("f.txt", "test").unwrap() else { panic!("not live on b") };
        let big: Vec<u8> = (0..2 * MAX_UPDATE_BYTES + 5).map(|i| (i % 251) as u8).collect();
        b.set_fragment_faults(Some(FragmentFaults { seed: 1, sleep: 1000, delay: 0, max_delay: 0 }));
        doc_a.send(&big).unwrap();
        doc_a.send(b"small").unwrap();
        assert!(no_update(&doc_b), "asleep inside the change");
        let Opened::Live(meanwhile) = docs_b.open("f.txt", "test").unwrap() else { panic!("not live on b") };
        assert!(meanwhile.state_updates().is_empty(), "nothing of the change in the state");
        // Awake after the sleep: the next fragment, then asleep again before the last.
        for _ in 0..SLEEP_TICKS {
            hub.tick();
        }
        assert!(no_update(&doc_b), "still not whole");
        b.set_fragment_faults(None);
        assert_eq!(next_update(&doc_b), big, "the change, whole");
        assert_eq!(next_update(&doc_b), b"small");
        assert_eq!(next_update(&meanwhile), big, "whole for the app that opened it meanwhile too");
        let Opened::Live(again) = docs_b.open("f.txt", "test").unwrap() else { panic!("not live on b") };
        assert_eq!(again.state_updates(), [big, b"small".to_vec()], "the state has it whole");
    }
}
