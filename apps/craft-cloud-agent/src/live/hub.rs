//! The live hub's task: owns the account's [`Replica`], answers the local API's recents calls, and keeps the live
//! connection to the server up while the agent is signed in (connecting again with backoff, from 1 s doubling to 5
//! minutes with jitter; every 10 minutes for a server that speaks another live protocol). Offline it keeps working:
//! adds are kept on disk and reach the server once the connection is back. While syncing is paused the connection is
//! held the same way (closed, and not dialed until resumed).

use std::collections::BTreeMap;
use std::time::Duration;

use craft_cloud_proto::live::{AppName, ClientFrame, MAX_SUBSCRIPTIONS, ServerFrame};
use craft_cloud_proto::local::{MAX_MACHINE_BYTES, MAX_TITLE_BYTES, NewRecent, RecentEntry, recent_path};
use craft_cloud_proto::{NodeId, SpaceId};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use super::link::{DialError, LiveSocket, dial};
use super::replica::{AddError, LinkError, Replica};
use super::store::DiskStore;
use crate::account::DataDir;
use crate::http::{DeviceToken, ServerUrl};
use crate::local_api::Control;

/// What the hub needs to know about the account.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub data: DataDir,
    pub space: SpaceId,
    pub server: ServerUrl,
    pub token: DeviceToken,
    /// This machine's name in the entries it adds (the device name).
    pub machine: String,
}

/// Why a recents call was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HubError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Failed(String),
    #[error("the agent stopped following recents (signed out, or stopping)")]
    Gone,
}

/// An app's list as its followers see it: the entries, and whether its push stopped ([`Replica::push_stopped`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<RecentEntry>,
    pub push_stopped: bool,
}

enum Request {
    Follow {
        app: AppName,
        reply: oneshot::Sender<Result<watch::Receiver<Listing>, HubError>>,
    },
    Add {
        app: AppName,
        entry: NewRecent,
        reply: oneshot::Sender<Result<RecentEntry, HubError>>,
    },
    /// Hold the live connection (closed, and not dialed) or let it run again; answered once done.
    Pause(bool, oneshot::Sender<()>),
}

/// How the local API reaches the hub. Cheap to clone.
#[derive(Clone)]
pub struct HubHandle {
    tx: mpsc::Sender<Request>,
}

impl HubHandle {
    /// `app`'s list now and after every change. The receiver closes when the hub stops.
    pub async fn follow(&self, app: AppName) -> Result<watch::Receiver<Listing>, HubError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Follow { app, reply }).await.map_err(|_| HubError::Gone)?;
        rx.await.map_err(|_| HubError::Gone)?
    }

    /// Adds a file to `app`'s list: on disk when this returns.
    pub async fn add(&self, app: AppName, entry: NewRecent) -> Result<RecentEntry, HubError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Add { app, entry, reply }).await.map_err(|_| HubError::Gone)?;
        rx.await.map_err(|_| HubError::Gone)?
    }

    /// Holds the live connection while syncing is paused (`true`): it is closed and not dialed again until `false`.
    /// Follows and adds keep working meanwhile; adds reach the server once it runs again.
    pub async fn set_paused(&self, paused: bool) -> Result<(), HubError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Pause(paused, reply)).await.map_err(|_| HubError::Gone)?;
        rx.await.map_err(|_| HubError::Gone)
    }
}

/// The running hub; dropping it stops it (what it was told is on disk already). It runs on a thread of its own with
/// its own runtime, so its disk writes (each add is written and flushed before it is answered) never hold up the
/// `run` loop or the local API.
pub struct Hub {
    handle: HubHandle,
    stop: Option<oneshot::Sender<()>>,
}

impl Hub {
    /// Opens the account's documents and starts the hub, with its live connection held if `paused` (see
    /// [`HubHandle::set_paused`]). `control` hears whether the live connection is up.
    pub fn start(config: HubConfig, control: Control, paused: bool) -> std::io::Result<Self> {
        let dir = config.data.path().join("live").join(config.space.to_string());
        let replica = Replica::open(DiskStore::open(&dir)?, config.space, super::replica::random_peers())?;
        let (tx, rx) = mpsc::channel(64);
        let (stop, stopped) = oneshot::channel();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        std::thread::Builder::new()
            .name("craft-cloud-live".into())
            .spawn(move || runtime.block_on(run(config, replica, rx, control, stopped, paused)))?;
        Ok(Self { handle: HubHandle { tx }, stop: Some(stop) })
    }

    pub fn handle(&self) -> HubHandle {
        self.handle.clone()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        // The hub's thread ends after the request it is on (handles held by open sockets don't keep it).
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Finds a file's node in the sync state (`state.json`, read again only when it changed).
struct NodeLookup {
    data: DataDir,
    seen: Option<(std::time::SystemTime, u64)>,
    state: Option<craft_cloud_sync::State>,
}

impl NodeLookup {
    fn node(&mut self, path: &str) -> Option<NodeId> {
        let file = self.data.path().join(crate::account::STATE_FILE);
        let stamp = std::fs::metadata(&file).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        if stamp != self.seen {
            self.seen = stamp;
            self.state = self.data.load_state().ok().map(|s| s.state);
        }
        self.state.as_ref()?.base_at(&craft_cloud_sync::rel(path))
    }
}

/// `text` cut to `max` bytes at a character boundary, without control characters.
fn clean(text: &str, max: usize) -> String {
    let mut out = String::new();
    for c in text.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

fn random_key() -> std::io::Result<[u8; 16]> {
    let mut key = [0u8; 16];
    getrandom::fill(&mut key).map_err(|e| std::io::Error::other(format!("the system random source failed: {e}")))?;
    Ok(key)
}

fn jitter(delay: Duration) -> Duration {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    let half = u64::try_from((delay / 2).as_millis()).unwrap_or(u64::MAX).max(1);
    delay / 2 + Duration::from_millis(u64::from_le_bytes(bytes) % half)
}

const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(300);
const RETRY_MISMATCH: Duration = Duration::from_secs(600);
/// While updates wait for an ack, the ones refused for a moment are sent again this often.
const RESEND_EVERY: Duration = Duration::from_secs(2);
const PING_EVERY: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

struct State {
    config: HubConfig,
    replica: Replica<DiskStore>,
    lookup: NodeLookup,
    lists: BTreeMap<AppName, watch::Sender<Listing>>,
    socket: Option<LiveSocket>,
    control: Control,
    said: Option<String>,
    /// When to dial again, and the backoff that grows with every connection lost (reset once welcomed).
    retry_at: Instant,
    delay: Duration,
    /// Syncing is paused: no connection is dialed (see [`HubHandle::set_paused`]).
    paused: bool,
}

impl State {
    /// Waits `wait` (or the backoff) before dialing again, and grows the backoff.
    fn back_off(&mut self, wait: Option<Duration>) {
        self.retry_at = Instant::now() + wait.unwrap_or_else(|| jitter(self.delay));
        self.delay = self.delay.saturating_mul(2).min(RETRY_MAX);
    }
    /// Tells the user `line` once (not again until something else was said).
    fn say(&mut self, line: String) {
        if self.said.as_ref() != Some(&line) {
            eprintln!("{line}");
            self.said = Some(line);
        }
    }

    /// Hands the changed lists to their followers, says the notes and writes what changed.
    fn settle(&mut self) {
        for (app, entries) in self.replica.take_changed() {
            if let Some(list) = self.lists.get(&app) {
                let push_stopped = self.replica.push_stopped(&app);
                list.send_replace(Listing { entries, push_stopped });
            }
        }
        for note in self.replica.take_notes() {
            self.say(note);
        }
        if let Err(e) = self.replica.flush() {
            self.say(format!("the recents couldn't be written to disk: {e}"));
        }
        self.control.set_online(self.replica.is_up());
    }

    /// Sends frames; false when the connection broke (it is dropped).
    async fn send(&mut self, frames: Vec<ClientFrame>) -> bool {
        if self.socket.is_none() {
            return true;
        }
        let mut encoded = Vec::with_capacity(frames.len());
        for frame in frames {
            match frame.encode() {
                Ok(bytes) => encoded.push(bytes),
                Err(e) => self.say(format!("a live frame couldn't be encoded: {e}")),
            }
        }
        let Some(socket) = self.socket.as_mut() else { return true };
        for bytes in encoded {
            if !matches!(tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Binary(bytes.into()))).await, Ok(Ok(()))) {
                self.drop_socket();
                return false;
            }
        }
        true
    }

    /// The connection is gone (for whatever reason): dialed again after the backoff, never at once.
    fn drop_socket(&mut self) {
        if self.socket.take().is_some() {
            self.back_off(None);
        }
        self.replica.disconnected();
        self.control.set_online(false);
    }

    fn entry(&mut self, app: &AppName, new: &NewRecent) -> Result<RecentEntry, HubError> {
        let path = recent_path(&new.path).ok_or_else(|| HubError::Invalid(format!("{:?} isn't a path inside the synced folder", new.path)))?;
        let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
        let title = clean(new.title.as_deref().unwrap_or(&name), MAX_TITLE_BYTES);
        let now = crate::cli::now_ms();
        let mut entry = RecentEntry {
            node: self.lookup.node(&path),
            path,
            title,
            opened_ms: now,
            saved_ms: None,
            machine: clean(&self.config.machine, MAX_MACHINE_BYTES),
        };
        entry.saved_ms = if new.saved { Some(now) } else { self.replica.current(app, &entry).and_then(|e| e.saved_ms) };
        Ok(entry)
    }

    async fn handle(&mut self, request: Request) {
        match request {
            // The run loop's to handle (it owns the dialing).
            Request::Pause(..) => {}
            Request::Follow { app, reply } => {
                if !self.lists.contains_key(&app) && self.lists.len() >= MAX_SUBSCRIPTIONS {
                    let _ = reply.send(Err(HubError::Invalid(format!("the agent follows at most {MAX_SUBSCRIPTIONS} apps' recents"))));
                    return;
                }
                let frames = self.replica.follow(&app);
                let entries = self.replica.entries(&app);
                let push_stopped = self.replica.push_stopped(&app);
                let list = self.lists.entry(app).or_insert_with(|| watch::channel(Listing { entries, push_stopped }).0);
                let _ = reply.send(Ok(list.subscribe()));
                self.send(frames).await;
            }
            Request::Add { app, entry, reply } => {
                if !self.lists.contains_key(&app) && self.lists.len() >= MAX_SUBSCRIPTIONS {
                    let _ = reply.send(Err(HubError::Invalid(format!("the agent follows at most {MAX_SUBSCRIPTIONS} apps' recents"))));
                    return;
                }
                let added = match (self.entry(&app, &entry), random_key()) {
                    (Ok(entry), Ok(key)) => self.replica.add(&app, key, entry.clone()).map(|frames| (entry, frames)).map_err(|e| match e {
                        AddError::Invalid(m) => HubError::Invalid(m),
                        AddError::Io(e) => HubError::Failed(format!("the recents couldn't be written to disk: {e}")),
                    }),
                    (Err(e), _) => Err(e),
                    (_, Err(e)) => Err(HubError::Failed(e.to_string())),
                };
                if !self.lists.contains_key(&app) {
                    let entries = self.replica.entries(&app);
                    let push_stopped = self.replica.push_stopped(&app);
                    self.lists.insert(app, watch::channel(Listing { entries, push_stopped }).0);
                }
                match added {
                    Ok((entry, frames)) => {
                        let _ = reply.send(Ok(entry));
                        self.send(frames).await;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
        }
    }
}

async fn run(
    config: HubConfig,
    replica: Replica<DiskStore>,
    mut requests: mpsc::Receiver<Request>,
    control: Control,
    mut stop: oneshot::Receiver<()>,
    paused: bool,
) {
    let lookup = NodeLookup { data: config.data.clone(), seen: None, state: None };
    let mut state = State {
        config,
        replica,
        lookup,
        lists: BTreeMap::new(),
        socket: None,
        control,
        said: None,
        retry_at: Instant::now(),
        delay: RETRY_FIRST,
        paused,
    };
    state.settle();
    let mut dialing: Option<tokio::task::JoinHandle<Result<LiveSocket, DialError>>> = None;
    let mut last_heard = Instant::now();
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut resend = tokio::time::interval(RESEND_EVERY);
    loop {
        if state.socket.is_none() && dialing.is_none() && !state.paused && Instant::now() >= state.retry_at {
            let (server, token) = (state.config.server.clone(), state.config.token.clone());
            dialing = Some(tokio::spawn(async move { dial(&server, &token).await }));
        }
        let (up, retry_at) = (state.socket.is_some(), state.retry_at);
        tokio::select! {
            _ = &mut stop => {
                if let Some(mut socket) = state.socket.take() {
                    let _ = socket.close(None).await;
                }
                return;
            }
            request = requests.recv() => match request {
                Some(Request::Pause(paused, done)) => {
                    state.paused = paused;
                    if paused {
                        if let Some(task) = dialing.take() {
                            task.abort();
                        }
                        if let Some(mut socket) = state.socket.take() {
                            let _ = socket.close(None).await;
                        }
                        state.replica.disconnected();
                        // Offline before the caller hears it, so the status it reads next says so.
                        state.control.set_online(false);
                    } else {
                        // Dialed at once: nothing was wrong with the connection.
                        state.retry_at = Instant::now();
                        state.delay = RETRY_FIRST;
                    }
                    let _ = done.send(());
                }
                Some(request) => state.handle(request).await,
                None => return,
            },
            dialed = async { match dialing.as_mut() { Some(task) => task.await, None => std::future::pending().await } } => {
                dialing = None;
                match dialed {
                    Ok(Ok(socket)) => {
                        state.socket = Some(socket);
                        last_heard = Instant::now();
                        let hello = state.replica.connected();
                        state.send(hello).await;
                    }
                    Ok(Err(e)) => {
                        state.say(format!("recents stay on this machine for now: {e}"));
                        state.back_off(None);
                    }
                    Err(_) => state.back_off(None),
                }
            }
            message = async { match state.socket.as_mut() { Some(socket) => socket.next().await, None => std::future::pending().await } } => {
                last_heard = Instant::now();
                let frame = match message {
                    Some(Ok(Message::Binary(bytes))) => ServerFrame::decode(&bytes).map_err(|e| LinkError::Retry(format!("a live frame doesn't decode: {e}"))),
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                    Some(Ok(Message::Text(_))) => Err(LinkError::Retry("the server sent a text frame".into())),
                    Some(Ok(Message::Close(_)) | Err(_)) | None => Err(LinkError::Retry("the live connection closed".into())),
                };
                match frame.and_then(|frame| state.replica.on_frame(frame)) {
                    Ok(frames) => {
                        if state.replica.is_up() {
                            state.delay = RETRY_FIRST;
                            state.said = None;
                        }
                        state.send(frames).await;
                    }
                    Err(e) => {
                        if let Some(mut socket) = state.socket.take() {
                            let _ = socket.close(None).await;
                        }
                        state.drop_socket();
                        state.back_off(None);
                        if let LinkError::Mismatch(m) = &e {
                            state.say(format!("recents stay on this machine: {m}"));
                            state.back_off(Some(RETRY_MISMATCH));
                        }
                    }
                }
            }
            _ = ping.tick(), if up => {
                if last_heard.elapsed() > IDLE_TIMEOUT {
                    state.drop_socket();
                } else if let Some(socket) = state.socket.as_mut()
                    && !matches!(tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Ping(Vec::new().into()))).await, Ok(Ok(())))
                {
                    state.drop_socket();
                }
            }
            _ = resend.tick(), if up && state.replica.wants_tick() => {
                let frames = state.replica.resend();
                state.send(frames).await;
            }
            () = tokio::time::sleep_until(retry_at), if !up && dialing.is_none() && !state.paused => {}
        }
        state.settle();
    }
}
