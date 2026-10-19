//! Live documents over the simulated network: the documents simulation's random work (apps editing one document's
//! text and map, machines crashing and coming back, the log compacted at random) with the agents' replicas talking to
//! the server's live sessions over real WebSockets (tungstenite's framing) on turmoil's TCP, while the network
//! partitions links (both ways), holds them and delays every message. Then the network heals, everything settles, and
//! the documents simulation's checks run: every machine and every app holds the same document, every edit an agent
//! took is in it, and nothing waits for the server.
//!
//! Not simulated here: the server's axum handler and the agent's hub (`tests/live_documents.rs` runs those against a
//! loopback server); their protocol halves, the session and the replica, are the real ones.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use craft_cloud_agent::live::replica::{DocEvent, Opening, Peers, Replica};
use craft_cloud_agent::live::store::MemStore;
use craft_cloud_meta::{DeviceAuth, Meta, Role};
use craft_cloud_proto::live::{AppName, ClientFrame, ServerFrame};
use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, DocId, FileHash, Manifest, Name, NewVersion, NodeId, Op, SpaceId, VersionId};
use craft_cloud_server::AppState;
use craft_cloud_server::live::Session;
use craft_cloud_server::state::Limits;
use craft_cloud_store::Store;
use craft_cloud_store::memory::MemoryBackend;
use futures_util::{SinkExt, StreamExt};
use loro::{ExportMode, LoroDoc};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::common::sim::{Rng, tracing};

const SERVER: &str = "server";
const PORT: u16 = 81;
const MACHINES: usize = 3;
/// How long the random work goes on, in simulated time; then the network heals.
const WORK: Duration = Duration::from_secs(120);

fn app() -> AppName {
    AppName::new("wordcraft").unwrap()
}

fn peers(base: u64) -> Peers {
    let mut n = 0u64;
    Box::new(move || {
        n += 1;
        base.wrapping_add(n.wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 1
    })
}

/// What the hosts and the driver share.
struct World {
    meta: Meta,
    state: Arc<AppState>,
    space: SpaceId,
    node: NodeId,
    hosts: Vec<Mutex<Host>>,
    /// What happened, in order: for the trace and the replay check.
    log: Mutex<Vec<String>>,
    /// A task keeps simulated time still while the database works (see the network harness's `hold_time`).
    holding: AtomicBool,
}

impl World {
    fn say(&self, line: String) {
        if tracing() {
            eprintln!("{} {line}", stamp());
        }
        self.log.lock().unwrap().push(line);
    }
}

fn stamp() -> String {
    format!("[{:>8.2}s]", turmoil::sim_elapsed().unwrap_or_default().as_secs_f64())
}

/// One machine: its agent's replica over a store that outlasts its crashes, and its app's own copy.
struct Host {
    token: String,
    store: MemStore,
    replica: Replica<MemStore>,
    runs: u64,
    base: u64,
    app: Option<LoroDoc>,
    app_runs: u64,
    opening: bool,
    doc: Option<DocId>,
    /// Every edit its agent took from its app.
    taken: BTreeSet<String>,
    edits: u64,
    /// Frames for the connection, from the app's side.
    out: Vec<ClientFrame>,
    /// It is to crash (the connection task does it).
    crash: bool,
    /// What its agent said.
    notes: Vec<String>,
}

impl Host {
    fn app_doc(&mut self, doc: DocId) {
        self.app_runs += 1;
        let Some((snapshot, after)) = self.replica.document_state(doc) else { return };
        let app = LoroDoc::new();
        app.set_peer_id((self.base >> 8) + self.app_runs).unwrap();
        app.import(&snapshot).unwrap();
        for bytes in after {
            app.import(&bytes).unwrap();
        }
        self.app = Some(app);
    }

    /// Hands the replica's events to the app.
    fn events(&mut self, node: NodeId) {
        for event in self.replica.take_events() {
            match event {
                DocEvent::Joined(doc) => {
                    self.doc = Some(doc);
                    if std::mem::take(&mut self.opening) {
                        self.app_doc(doc);
                    }
                }
                DocEvent::NeedsImport(doc) => {
                    let start = LoroDoc::new();
                    start.get_text("text").insert(0, "start;").unwrap();
                    start.commit();
                    if let Ok(frames) = self.replica.import_document(doc, &start.export(ExportMode::Snapshot).unwrap()) {
                        self.out.extend(frames);
                    }
                }
                DocEvent::NotLive(..) => self.opening = false,
                DocEvent::Remote(_, bytes) => {
                    if let Some(app) = &self.app {
                        app.import(&bytes).unwrap();
                    }
                }
                DocEvent::Presence(..) => {}
            }
        }
        let _ = node;
        self.replica.flush().unwrap();
        self.notes.extend(self.replica.take_notes());
    }

    fn open(&mut self, node: NodeId) {
        if self.app.is_some() || self.opening {
            return;
        }
        let (doc, opening, frames) = self.replica.open_document(node, &app());
        self.out.extend(frames);
        match opening {
            Opening::Live => self.app_doc(doc),
            Opening::Joining => self.opening = true,
            Opening::NotLive(..) => {}
        }
    }

    /// The app edits: a token between tokens of the text, or a key of the map.
    fn edit(&mut self, m: usize, rng: &mut Rng) -> Option<String> {
        let (Some(doc), Some(app)) = (self.doc, self.app.as_ref()) else { return None };
        self.edits += 1;
        let token = format!("m{m}e{};", self.edits);
        let before = app.oplog_vv();
        if rng.below(10) < 7 {
            let text = app.get_text("text");
            let mut places = vec![0];
            places.extend(text.to_string().chars().enumerate().filter(|(_, c)| *c == ';').map(|(i, _)| i + 1));
            text.insert(places[rng.below(places.len())], &token).unwrap();
        } else {
            app.get_map("map").insert(&token, self.edits as i64).unwrap();
        }
        app.commit();
        let update = app.export(ExportMode::updates(&before)).unwrap();
        match self.replica.local_update(doc, &update) {
            Ok(frames) => {
                self.taken.insert(token.clone());
                self.out.extend(frames);
                Some(token)
            }
            Err(_) => {
                self.app = None;
                None
            }
        }
    }

    /// Crashes: what it hadn't written is gone; it starts again from its store.
    fn restart(&mut self, space: SpaceId) {
        self.runs += 1;
        self.replica = Replica::open(self.store.clone(), space, peers(self.base ^ (self.runs << 40))).unwrap();
        self.app = None;
        self.opening = false;
        self.out.clear();
    }
}

/// Keeps simulated time still while the database works, as the network harness does (see its `hold_time`).
fn hold_time(world: &Arc<World>) {
    if world.holding.swap(true, Ordering::Relaxed) {
        return;
    }
    let world = world.clone();
    tokio::spawn(async move {
        struct Release(Arc<World>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.holding.store(false, Ordering::Relaxed);
            }
        }
        let _release = Release(world.clone());
        let mut idle = 0;
        while idle < 4 {
            if world.meta.busy() {
                idle = 0;
                let meta = world.meta.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    while meta.busy() {
                        std::thread::sleep(Duration::from_micros(20));
                    }
                })
                .await;
            } else {
                idle += 1;
                tokio::task::yield_now().await;
            }
        }
    });
}

/// A future that keeps time still whenever it waits (on the database).
struct Held<F> {
    world: Arc<World>,
    inner: Pin<Box<F>>,
}

impl<F: Future> Future for Held<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        let out = this.inner.as_mut().poll(cx);
        if out.is_pending() {
            hold_time(&this.world);
        }
        out
    }
}

fn held<F: Future>(world: &Arc<World>, f: F) -> Held<F> {
    Held { world: world.clone(), inner: Box::pin(f) }
}

/// The server host: each connection a WebSocket, signed in with its device token, served by a live session.
async fn server(world: Arc<World>) -> turmoil::Result {
    let listener = turmoil::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, PORT)).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(serve(world.clone(), stream));
    }
}

#[allow(clippy::result_large_err)] // (tungstenite's callback type)
async fn serve(world: Arc<World>, stream: turmoil::net::TcpStream) {
    let token = Arc::new(Mutex::new(None::<String>));
    let seen = token.clone();
    let callback = move |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
        let bearer = request.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::to_owned);
        *seen.lock().unwrap() = bearer;
        Ok(response)
    };
    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else { return };
    let Some(token) = token.lock().unwrap().clone() else { return };
    let hash = craft_cloud_server::auth::token_hash(&token);
    let Ok(Some(auth)) = held(&world, world.meta.device_by_token(&hash)).await else { return };
    let mut notes = world.state.live.listen(auth.space);
    let mut presence = world.state.live.listen_presence(auth.space);
    let mut session = Session::new(world.state.clone(), &auth);
    loop {
        tokio::select! {
            biased;
            message = ws.next() => {
                let bytes = match message {
                    Some(Ok(Message::Binary(bytes))) => bytes,
                    // (The pong goes out with the next frame sent.)
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        if ws.flush().await.is_err() {
                            break;
                        }
                        continue;
                    }
                    _ => break,
                };
                let Ok(frame) = ClientFrame::decode(&bytes) else { break };
                let reply = held(&world, session.handle(frame)).await;
                for frame in reply.frames {
                    if ws.send(Message::Binary(frame.encode().unwrap().into())).await.is_err() {
                        return;
                    }
                }
                if reply.close {
                    break;
                }
            }
            note = notes.recv() => match note {
                Ok(doc) => session.note(doc),
                Err(_) => session.lagged(),
            },
            note = presence.recv() => {
                if let Ok(note) = note
                    && let Some(frame) = session.relay(&note)
                    && ws.send(Message::Binary(frame.encode().unwrap().into())).await.is_err()
                {
                    return;
                }
            }
            () = std::future::ready(()), if session.has_work() => {
                let Ok(frames) = held(&world, session.pump()).await else { break };
                for frame in frames {
                    if ws.send(Message::Binary(frame.encode().unwrap().into())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// A machine's host: the app editing at random, and the agent's connection (dialed again whenever it goes).
async fn machine(world: Arc<World>, m: usize, seed: u64, stop: Arc<AtomicBool>) -> turmoil::Result {
    let wake = Arc::new(Notify::new());
    tokio::task::spawn_local(work(world.clone(), m, seed, stop.clone(), wake.clone()));
    let mut rng = Rng::new(seed ^ 0xD1A1_0000 ^ m as u64);
    loop {
        let connected = connection(&world, m, &wake).await;
        world.say(format!("m{m} {}", if connected { "lost its connection" } else { "can't connect" }));
        let crashed = {
            let mut host = world.hosts[m].lock().unwrap();
            host.replica.disconnected();
            host.events(world.node);
            let crashed = std::mem::take(&mut host.crash);
            if crashed {
                host.restart(world.space);
            }
            crashed
        };
        if crashed {
            world.say(format!("m{m} crashed and started again"));
        }
        let backoff = 200 + rng.below(1800) as u64;
        tokio::time::sleep(Duration::from_millis(backoff)).await;
    }
}

/// One connection, until it goes (or the machine crashes): whether it came up.
async fn connection(world: &Arc<World>, m: usize, wake: &Arc<Notify>) -> bool {
    let token = world.hosts[m].lock().unwrap().token.clone();
    let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(10), turmoil::net::TcpStream::connect((SERVER, PORT))).await else { return false };
    let mut request = format!("ws://{SERVER}:{PORT}/api/v1/live").into_client_request().unwrap();
    request.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    let Ok(Ok((ws, _))) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::client_async(request, stream)).await else {
        return false;
    };
    // As the hub does: a writer task of its own, so reading goes on while a send waits.
    let (mut sink, mut ws) = ws.split();
    let (to_writer, mut queue) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let writer = tokio::task::spawn_local(async move {
        while let Some(message) = queue.recv().await {
            if !matches!(tokio::time::timeout(Duration::from_secs(30), sink.send(message)).await, Ok(Ok(()))) {
                return;
            }
        }
    });
    struct Abort(tokio::task::JoinHandle<()>);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let writer = Abort(writer);
    let hello = world.hosts[m].lock().unwrap().replica.connected();
    for frame in hello {
        let _ = to_writer.send(Message::Binary(frame.encode().unwrap().into()));
    }
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut last_heard = tokio::time::Instant::now();
    let api = crate::blobs::ServerBlobs { state: world.state.clone(), space: world.space };
    loop {
        let (out, uploads) = {
            let mut host = world.hosts[m].lock().unwrap();
            if host.crash {
                return true;
            }
            (std::mem::take(&mut host.out), host.replica.take_uploads())
        };
        if writer.0.is_finished() {
            return true;
        }
        // The snapshots these frames name go up first, through the chunk API (in-process here: the simulated network
        // carries the live connection only).
        for up in uploads {
            if let Err(e) = held(world, craft_cloud_agent::live::blobs::upload(&api, &up)).await {
                world.hosts[m].lock().unwrap().replica.upload_failed(up.doc, &e.to_string());
            }
        }
        for frame in out {
            let _ = to_writer.send(Message::Binary(frame.encode().unwrap().into()));
        }
        tokio::select! {
            message = ws.next() => {
                last_heard = tokio::time::Instant::now();
                let bytes = match message {
                    Some(Ok(Message::Binary(bytes))) => bytes,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    _ => return true,
                };
                let Ok(frame) = ServerFrame::decode(&bytes) else { return true };
                let fetches = {
                    let mut host = world.hosts[m].lock().unwrap();
                    match host.replica.on_frame(frame) {
                        Ok(frames) => host.out.extend(frames),
                        Err(_) => return true,
                    }
                    host.events(world.node);
                    host.replica.take_fetches()
                };
                // A snapshot a catch-up starts with: downloaded (in-process), then handed back.
                for (doc, manifest) in fetches {
                    let got = held(world, craft_cloud_agent::live::blobs::download(&api, &manifest)).await.map_err(|e| e.to_string());
                    let mut host = world.hosts[m].lock().unwrap();
                    let frames = host.replica.on_snapshot(doc, manifest, got);
                    host.out.extend(frames);
                    host.events(world.node);
                }
            }
            _ = tick.tick() => {
                // A connection silent for 90 s is gone (as the hub's pings find); a ping keeps a quiet one heard.
                if last_heard.elapsed() > Duration::from_secs(90) {
                    return true;
                }
                let _ = to_writer.send(Message::Ping(Vec::new().into()));
                let mut host = world.hosts[m].lock().unwrap();
                let frames = host.replica.resend();
                host.out.extend(frames);
                host.events(world.node);
            }
            () = wake.notified() => {}
        }
    }
}

/// The app's side of a machine: opens the document, edits it, now and then closes it, writes documents whole; and the
/// machine crashes now and then.
async fn work(world: Arc<World>, m: usize, seed: u64, stop: Arc<AtomicBool>, wake: Arc<Notify>) {
    let mut rng = Rng::new(seed ^ 0xA9_0000 ^ ((m as u64) << 20));
    loop {
        tokio::time::sleep(Duration::from_millis(50 + rng.below(400) as u64)).await;
        let calm = stop.load(Ordering::Relaxed);
        let mut host = world.hosts[m].lock().unwrap();
        host.open(world.node);
        if !calm {
            match rng.below(100) {
                0..=59 => {
                    if let Some(token) = host.edit(m, &mut rng) {
                        drop(host);
                        world.say(format!("m{m} edits {token}"));
                        host = world.hosts[m].lock().unwrap();
                    }
                }
                60..=64 => host.replica.flush_documents().unwrap(),
                65..=66 => {
                    host.crash = true;
                }
                67..=68 => {
                    if let (Some(doc), Some(_)) = (host.doc, host.app.as_ref()) {
                        host.replica.set_open(doc, false);
                        host.app = None;
                    }
                }
                _ => {}
            }
        }
        host.events(world.node);
        drop(host);
        wake.notify_one();
    }
}

/// The driver: the weather while the work goes on, then calm, then the checks.
async fn drive(world: Arc<World>, seed: u64, stop: Arc<AtomicBool>) -> Vec<String> {
    let mut rng = Rng::new(seed ^ 0x3EA7_0000_0000_0011);
    let started = tokio::time::Instant::now();
    while started.elapsed() < WORK {
        tokio::time::sleep(Duration::from_millis(500 + rng.below(4000) as u64)).await;
        let host = format!("m{}", rng.below(MACHINES));
        match rng.below(6) {
            0 => {
                world.say(format!("{host} is cut off from the server"));
                turmoil::partition(host.as_str(), SERVER);
            }
            1 => {
                world.say(format!("{host} reaches the server again"));
                turmoil::repair(host.as_str(), SERVER);
            }
            2 => {
                world.say(format!("{host}'s link holds"));
                turmoil::hold(host.as_str(), SERVER);
            }
            3 => {
                turmoil::release(host.as_str(), SERVER);
            }
            _ => {}
        }
    }
    for i in 0..MACHINES {
        let host = format!("m{i}");
        turmoil::release(host.as_str(), SERVER);
        turmoil::repair(host.as_str(), SERVER);
    }
    stop.store(true, Ordering::Relaxed);
    world.say("the network heals".into());
    // Until every machine and app holds the same document and nothing waits (at most 10 simulated minutes).
    let mut converged = false;
    for _ in 0..600 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if settled(&world) {
            converged = true;
            break;
        }
    }
    let views: Vec<String> = (0..MACHINES).map(|m| describe(&world, m)).collect();
    assert!(converged, "seed {seed}: never settled: {views:?}");
    check(&world, seed);
    world.say(format!("settled: {}", views.first().cloned().unwrap_or_default()));
    std::mem::take(&mut *world.log.lock().unwrap())
}

fn view(host: &mut Host) -> Option<(String, String)> {
    let doc = host.doc?;
    let (snapshot, after) = host.replica.document_state(doc)?;
    let held = LoroDoc::new();
    held.import(&snapshot).ok()?;
    for bytes in after {
        held.import(&bytes).ok()?;
    }
    let app = host.app.as_ref()?;
    Some((format!("{:?}", held.get_deep_value()), format!("{:?}", app.get_deep_value())))
}

fn describe(world: &World, m: usize) -> String {
    let mut host = world.hosts[m].lock().unwrap();
    let status = host.doc.and_then(|d| host.replica.document_status(d));
    let inside = host.doc.map(|d| host.replica.describe_doc(d)).unwrap_or_default();
    format!("m{m}: {status:?}, {:?}, said {:?}, {inside}", view(&mut host).map(|(held, _)| held.len()), host.notes)
}

fn settled(world: &World) -> bool {
    let mut first = None;
    for m in 0..MACHINES {
        let mut host = world.hosts[m].lock().unwrap();
        let Some(doc) = host.doc else { return false };
        if host.replica.document_status(doc) != Some((true, 0)) {
            return false;
        }
        let Some((held, app)) = view(&mut host) else { return false };
        if held != app {
            return false;
        }
        match &first {
            None => first = Some(held),
            Some(f) if *f != held => return false,
            Some(_) => {}
        }
    }
    true
}

fn check(world: &World, seed: u64) {
    let host = world.hosts[0].lock().unwrap();
    let doc = host.doc.unwrap();
    let (snapshot, after) = host.replica.document_state(doc).unwrap();
    drop(host);
    let held = LoroDoc::new();
    held.import(&snapshot).unwrap();
    for bytes in after {
        held.import(&bytes).unwrap();
    }
    let text = held.get_text("text").to_string();
    let mut keys = BTreeSet::new();
    held.get_map("map").for_each(|k, _| {
        keys.insert(k.to_owned());
    });
    for m in 0..MACHINES {
        for token in &world.hosts[m].lock().unwrap().taken {
            assert!(text.contains(token.as_str()) || keys.contains(token), "seed {seed}: the edit {token} was taken and is lost");
        }
    }
}

/// One seed's run; its trace.
pub fn run_seed(seed: u64) -> Vec<String> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (world, tokens) = runtime.block_on(async {
        let meta = Meta::open("sqlite::memory:").await.unwrap();
        let user = meta.create_user("ana", "$argon2id$stub", Role::Member, 0).await.unwrap();
        let mut rng = Rng::new(seed ^ 0x11FE);
        let limits = Limits {
            compact_updates: 2 + rng.below(30) as u64,
            compact_timeout: if rng.below(2) == 0 { Duration::ZERO } else { Duration::from_secs(30) },
            ..Limits::default()
        };
        let clock = Arc::new(|| 1_760_000_000_000 + turmoil::sim_elapsed().map_or(0, |t| i64::try_from(t.as_millis()).unwrap_or(i64::MAX)));
        let state = Arc::new(AppState::new(meta.clone(), Store::new(MemoryBackend::new()), vec![]).unwrap().with_limits(limits).with_clock(clock));
        let mut tokens = Vec::new();
        let mut auths: Vec<DeviceAuth> = Vec::new();
        for i in 0..MACHINES {
            let token = format!("token-of-machine-{i}-{seed}");
            meta.create_device(user.id, &format!("m{i}"), &craft_cloud_server::auth::token_hash(&token), 0).await.unwrap();
            auths.push(meta.device_by_token(&craft_cloud_server::auth::token_hash(&token)).await.unwrap().unwrap());
            tokens.push(token);
        }
        // The file, as the file sync uploaded it.
        let chunk = ChunkId::from_bytes(*blake3::hash(b"the file").as_bytes());
        meta.add_chunk(user.space, &chunk, 11, 0).await.unwrap();
        let manifest =
            Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 10, FileHash::from_bytes([1; 32]), vec![ChunkRef { id: chunk, len: 10 }])
                .unwrap();
        meta.add_manifest(user.space, &manifest, 100, 0).await.unwrap();
        let root = meta.space_info(user.space).await.unwrap().root;
        let node = NodeId::from_uuid(uuid::Uuid::from_u64_pair(seed, 7));
        let version = NewVersion { id: VersionId::from_uuid(uuid::Uuid::from_u64_pair(seed, 8)), manifest: manifest.id(), mtime_ms: 0, exec: false };
        meta.commit(user.space, auths[0].device, &[Op::Create { node, parent: root, name: Name::new("doc.txt").unwrap(), version }], 0)
            .await
            .unwrap();
        let hosts = (0..MACHINES)
            .map(|i| {
                let store = MemStore::default();
                let base = ((i as u64 + 1) << 48) ^ seed;
                Mutex::new(Host {
                    token: tokens[i].clone(),
                    replica: Replica::open(store.clone(), user.space, peers(base)).unwrap(),
                    store,
                    runs: 1,
                    base,
                    app: None,
                    app_runs: 0,
                    opening: false,
                    doc: None,
                    taken: BTreeSet::new(),
                    edits: 0,
                    out: Vec::new(),
                    crash: false,
                    notes: Vec::new(),
                })
            })
            .collect();
        // SQLite's connection goes back to its pool before the setup runtime ends (as the network harness does).
        while meta.busy() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let world = Arc::new(World { meta, state, space: user.space, node, hosts, log: Mutex::default(), holding: AtomicBool::new(false) });
        (world, tokens)
    });
    drop(runtime);
    drop(tokens);
    let latency = [2, 20, 100][(seed % 3) as usize];
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(3600))
        .tick_duration(Duration::from_millis(10))
        .min_message_latency(Duration::from_millis(1))
        .max_message_latency(Duration::from_millis(latency))
        .rng_seed(seed)
        .build();
    let on_server = world.clone();
    sim.host(SERVER, move || server(on_server.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    for m in 0..MACHINES {
        let (on_machine, stop) = (world.clone(), stop.clone());
        sim.host(format!("m{m}"), move || machine(on_machine.clone(), m, seed, stop.clone()));
    }
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (driving, out) = (world.clone(), trace.clone());
    sim.client("driver", async move {
        let log = drive(driving, seed, stop).await;
        *out.lock().unwrap() = log;
        Ok(())
    });
    if let Err(e) = sim.run() {
        panic!("seed {seed}: {e}");
    }
    std::mem::take(&mut *trace.lock().unwrap())
}
