//! Live documents through the agent's local API (`GET /v1/docs`), at the level of bytes: an app opens a file of the
//! synced folder by path and gets its state (a Loro snapshot), then sends its own changes (Loro updates) and gets the
//! other machines' and apps' as they come. What the bytes mean is the app's (its Loro document's); nothing here reads
//! them.
//!
//! One [`Documents`] is one connection, which may have many documents open. Blocking, with no async runtime: a thread
//! of its own reads the connection and hands each document's events to a channel the app polls
//! ([`Document::try_recv`]) or waits on ([`Document::recv_timeout`]); sending ([`Document::send`],
//! [`Document::presence`]) writes at once, from the caller's thread.
//!
//! ```no_run
//! # fn main() -> Result<(), craft_cloud_app::Error> {
//! use craft_cloud_app::live::{Event, Opened};
//! let agent = craft_cloud_app::Client::connect()?;
//! let docs = agent.documents()?;
//! let document = match docs.open("Letters/doc1.txt", "wordcraft")? {
//!     Opened::Live(document) => document,
//!     // Never live yet: send the file's content as a Loro snapshot.
//!     Opened::NeedsImport(importer) => importer.import(&[/* a Loro snapshot */])?,
//!     // A plain file for now (offline, not uploaded yet, …): open and save it as always.
//!     Opened::NotLive(_reason) => return Ok(()),
//! };
//! let _state = document.state(); // Build the app's Loro document from this.
//! document.send(&[/* a Loro update */])?;
//! while let Some(event) = document.try_recv() {
//!     match event {
//!         Event::Update(_bytes) => { /* import into the app's document */ }
//!         _ => {}
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! The connection isn't made again here when it goes ([`Event::Closed`]): an app opens its documents again on a new
//! [`Documents`].

use std::collections::HashMap;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use craft_cloud_api::DocId;
use craft_cloud_api::api::ErrorAnswer as ErrorBody;
use craft_cloud_api::fragments::split;
use craft_cloud_api::limits::MAX_UPDATE_BYTES;
use craft_cloud_api::local::docs::{IMPORT, MAX_BINARY_BYTES, PRESENCE, STATE, UPDATE, decode_binary, encode_binary};
use craft_cloud_api::local::{DocState, DocsEvent, DocsRequest, LOCAL_API_FRAGMENTS, MAX_LOCAL_JSON_BYTES, codes, paths};
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

pub use craft_cloud_api::local::NotLiveReason;

use crate::http::CONNECT_TIMEOUT;
use crate::{Client, Error};

/// What a document calls when an event came for it (from the reader's thread): an egui app's `request_repaint`.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// How long an open may take: finding the file's node (the agent may sync it first, up to 5 s), joining the document on
/// the server, and its state.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long writing one message to the agent may take (a big import too, the agent being on this machine).
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the reader waits for data before it looks whether it is to stop.
const IDLE_WAKE: Duration = Duration::from_millis(250);

/// What an agent too old for this app's live documents is refused with ([`codes::AGENT_TOO_OLD`]), for people.
pub const AGENT_TOO_OLD: &str = "Craft Cloud on this computer is older than this app; update Craft Cloud to edit live";

/// What happens to an open document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A change from another machine or another app on this one: import it as it is.
    Update(Vec<u8>),
    /// Another machine's or app's presence (Loro's ephemeral state), as it was sent.
    Presence(Vec<u8>),
    /// This app's change number `seq` ([`Document::send`]'s answer) is on the agent's disk.
    Acked(u64),
    /// This app's change number `seq` was refused: the agent's copy doesn't have it. `code` says whether sending it
    /// again can help: `invalid` (it doesn't decode) and `too_large` (over 1 MiB) never will; anything else (`internal`:
    /// the agent couldn't write it to disk) is worth sending again later.
    Refused { seq: u64, code: String, message: String },
    /// How the document stands: `online` (edits go out as they are made), changes waiting for the server, and whether
    /// its file was deleted (the document still works; a later open of the path is a new file), and whether the agent
    /// stopped following it until it restarts (Loro failed on it twice; changes made meanwhile are kept).
    Status { online: bool, waiting: u64, deleted: bool, stopped: bool },
    /// The document's file on this machine's disk now: the live document's frontiers (`(peer, counter)` heads) a Craft
    /// app saved it from, or none when no save record is known for it (see `DocsEvent::File`).
    File(Option<Vec<(u64, u32)>>),
    /// The connection to the agent ended.
    Closed(String),
}

/// What the agent's answer for a fragment of this app's that isn't its change's last is ([`Document::send`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inner {
    /// Not the app's (its change is answered under `last`), unless it is a refusal: then the change is refused.
    Of(u64),
    /// A change's last, whose change was refused already: dropped.
    Answered,
}

/// For tests and simulations: updates over `bytes` go in fragments of at most `bytes` from now on, in this process (so
/// changes of a few dozen KB exercise what changes over 1 MiB do). At most the frame limit.
#[doc(hidden)]
pub fn set_frame_limit_for_tests(bytes: usize) {
    FRAME_LIMIT.store(bytes.clamp(craft_cloud_api::fragments::HEADER_BYTES + 1, MAX_UPDATE_BYTES), Ordering::Relaxed);
}

static FRAME_LIMIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(MAX_UPDATE_BYTES);

/// What opening a file gives.
#[derive(Debug)]
pub enum Opened {
    /// The file is live: its state, and its events from now on.
    Live(Document),
    /// The file has never been live: send its content to make it a live document ([`Importer::import`]).
    NeedsImport(Importer),
    /// The file opens as a plain file (save it as always).
    NotLive(NotLiveReason),
}

/// What an app may tell the agent when it opens a file ([`Documents::open_with`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenOptions {
    /// The BLAKE3 hash of the file's bytes as the app read them: [`Document::changed_outside`] says whether a Craft
    /// app saved them.
    pub file_hash: Option<[u8; 32]>,
}

/// What `opened` says, before its state comes.
#[derive(Debug, Clone)]
struct Head {
    doc: DocId,
    state: DocState,
    changed_outside: Option<bool>,
}

/// An open's state so far: what `opened` said, the snapshot, the updates that are part of it, and how many of its
/// update frames came.
struct Filling {
    head: Head,
    snapshot: Vec<u8>,
    updates: Vec<Vec<u8>>,
    frames: u64,
}

/// What the reader hands an open in progress.
enum Answer {
    Live { head: Head, snapshot: Vec<u8>, updates: Vec<Vec<u8>> },
    NeedsImport,
    NotLive(NotLiveReason),
    Refused(Error),
}

/// The connection's shared state: the socket (written from the caller's thread, read by the reader), and where each
/// document's events go.
struct Shared {
    socket: Mutex<WebSocket<TcpStream>>,
    /// Each document number's events, and opens waiting for their answer.
    routes: Mutex<HashMap<u64, Sender<Event>>>,
    answers: Mutex<HashMap<u64, Sender<Answer>>>,
    /// An `opened` whose state hasn't come yet, by number.
    opening: Mutex<HashMap<u64, Head>>,
    /// An open whose state came and whose state's updates are still coming, by number.
    filling: Mutex<HashMap<u64, Filling>>,
    /// Which numbers a document is open as (its status names the document).
    numbers: Mutex<HashMap<DocId, Vec<u64>>>,
    /// What to call after an event went to a document's channel, by number ([`Document::set_waker`]).
    wakers: Mutex<HashMap<u64, Waker>>,
    /// The numbers (document number, `seq`) of the fragments sent that aren't a change's last, and what their answers
    /// are ([`Inner`]): the agent answers for a change by its last fragment's number.
    inner_seqs: Mutex<HashMap<(u64, u64), Inner>>,
    closed: AtomicBool,
    next_id: AtomicU64,
    /// A clone of the stream: dropping the connection shuts it down (which wakes the reader).
    stream: TcpStream,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn send(&self, message: Message) -> Result<(), Error> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Io("the connection to the agent is closed".into()));
        }
        lock(&self.socket).send(message).map_err(|e| Error::Io(e.to_string()))
    }

    fn send_json(&self, request: &DocsRequest) -> Result<(), Error> {
        let text = serde_json::to_string(request).map_err(|e| Error::Protocol(e.to_string()))?;
        self.send(Message::Text(text.into()))
    }

    fn send_binary(&self, kind: u8, id: u64, bytes: &[u8]) -> Result<(), Error> {
        self.send(Message::Binary(encode_binary(kind, id, bytes).into()))
    }

    /// Sends `frames` (one change's fragments) back to back: nothing of another sender's goes between them, so their
    /// numbers follow on.
    fn send_frames(&self, kind: u8, id: u64, frames: &[Vec<u8>]) -> Result<(), Error> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Io("the connection to the agent is closed".into()));
        }
        let mut socket = lock(&self.socket);
        for frame in frames {
            socket.send(Message::Binary(encode_binary(kind, id, frame).into())).map_err(|e| Error::Io(e.to_string()))?;
        }
        Ok(())
    }

    /// What the agent's answer for (`id`, `seq`) is, when it isn't simply the app's: a fragment that isn't a change's
    /// last, or a change's last whose change was refused already.
    fn inner(&self, id: u64, seq: u64) -> Option<Inner> {
        lock(&self.inner_seqs).remove(&(id, seq))
    }

    /// A fragment of a change (not its last, `last`) was refused: the change is, under its last number, and the
    /// answers still to come for it are dropped.
    fn refuse_change(&self, id: u64, last: u64, code: String, message: String) {
        {
            let mut inner = lock(&self.inner_seqs);
            inner.retain(|(i, _), what| !(*i == id && *what == Inner::Of(last)));
            inner.insert((id, last), Inner::Answered);
        }
        self.route(id, Event::Refused { seq: last, code, message });
    }

    fn route(&self, id: u64, event: Event) {
        if let Some(events) = lock(&self.routes).get(&id) {
            let _ = events.send(event);
        }
        self.wake(id);
    }

    fn wake(&self, id: u64) {
        let waker = lock(&self.wakers).get(&id).cloned();
        if let Some(waker) = waker {
            waker();
        }
    }

    fn answer(&self, id: u64, answer: Answer) {
        if let Some(waiting) = lock(&self.answers).remove(&id) {
            let _ = waiting.send(answer);
        }
    }

    /// One message from the agent.
    fn on_message(&self, message: Message) {
        match message {
            Message::Text(text) => match serde_json::from_str::<DocsEvent>(text.as_str()) {
                Ok(DocsEvent::Opened { id, doc, state, changed_outside }) => {
                    lock(&self.opening).insert(id, Head { doc, state, changed_outside });
                }
                Ok(DocsEvent::NeedsImport { id }) => self.answer(id, Answer::NeedsImport),
                Ok(DocsEvent::NotLive { id, reason, .. }) => self.answer(id, Answer::NotLive(reason)),
                Ok(DocsEvent::Ack { id, seq }) => {
                    if self.inner(id, seq).is_none() {
                        self.route(id, Event::Acked(seq));
                    }
                }
                Ok(DocsEvent::Status { doc, online, waiting, deleted, stopped }) => {
                    let ids = lock(&self.numbers).get(&doc).cloned().unwrap_or_default();
                    for id in ids {
                        self.route(id, Event::Status { online, waiting, deleted, stopped });
                    }
                }
                Ok(DocsEvent::File { doc, frontiers }) => {
                    let ids = lock(&self.numbers).get(&doc).cloned().unwrap_or_default();
                    for id in ids {
                        self.route(id, Event::File(frontiers.clone()));
                    }
                }
                Ok(DocsEvent::Error { id: Some(id), seq: Some(seq), code, message }) => match self.inner(id, seq) {
                    None => self.route(id, Event::Refused { seq, code, message }),
                    Some(Inner::Of(last)) => self.refuse_change(id, last, code, message),
                    Some(Inner::Answered) => {}
                },
                Ok(DocsEvent::Error { id: Some(id), seq: None, code, message }) => self.answer(id, Answer::Refused(Error::Refused { code, message })),
                Ok(DocsEvent::Error { id: None, .. }) | Err(_) => {}
            },
            Message::Binary(bytes) => {
                let Some((kind, id, body)) = decode_binary(&bytes) else { return };
                match kind {
                    STATE => {
                        if let Some(head) = lock(&self.opening).remove(&id) {
                            lock(&self.numbers).entry(head.doc).or_default().push(id);
                            if head.state.updates == 0 {
                                self.answer(id, Answer::Live { head, snapshot: body.to_vec(), updates: Vec::new() });
                            } else {
                                lock(&self.filling).insert(id, Filling { head, snapshot: body.to_vec(), updates: Vec::new(), frames: 0 });
                            }
                        }
                    }
                    UPDATE => {
                        // An update that is part of an open's state: kept with it until the last has come. (The agent
                        // sends changes whole, never in fragments: it joins the server's before.)
                        let mut filling = lock(&self.filling);
                        if let Some(f) = filling.get_mut(&id) {
                            f.frames += 1;
                            f.updates.push(body.to_vec());
                            if f.frames >= f.head.state.updates
                                && let Some(f) = filling.remove(&id)
                            {
                                drop(filling);
                                self.answer(id, Answer::Live { head: f.head, snapshot: f.snapshot, updates: f.updates });
                            }
                            return;
                        }
                        drop(filling);
                        self.route(id, Event::Update(body.to_vec()));
                    }
                    PRESENCE => self.route(id, Event::Presence(body.to_vec())),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// The connection ended: every document hears it, and every open waiting gets the error.
    fn end(&self, why: &str) {
        self.closed.store(true, Ordering::Relaxed);
        for (_, events) in lock(&self.routes).drain() {
            let _ = events.send(Event::Closed(why.to_owned()));
        }
        let wakers: Vec<Waker> = lock(&self.wakers).drain().map(|(_, w)| w).collect();
        for waker in wakers {
            waker();
        }
        lock(&self.filling).clear();
        for (_, waiting) in lock(&self.answers).drain() {
            let _ = waiting.send(Answer::Refused(Error::Io(why.to_owned())));
        }
    }
}

/// The reader's thread, on a WebSocket of its own over the connection: every message as it comes (waking now and then
/// to look whether to stop).
fn read_loop(shared: Arc<Shared>, mut socket: WebSocket<TcpStream>) {
    loop {
        if shared.closed.load(Ordering::Relaxed) {
            return;
        }
        match socket.read() {
            Ok(Message::Close(_)) => return shared.end("the agent closed the connection"),
            Ok(message) => shared.on_message(message),
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return shared.end(&e.to_string()),
        }
    }
}

/// One connection to the agent's live documents. Dropping it ends the connection (its documents hear
/// [`Event::Closed`]); the agent keeps them.
pub struct Documents {
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    /// The local API version the agent spoke when this connected.
    api: u32,
}

impl std::fmt::Debug for Documents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Documents").field("open", &lock(&self.shared.routes).len()).finish()
    }
}

impl Documents {
    pub(crate) fn connect(client: &Client) -> Result<Self, Error> {
        // An agent from before changes in fragments would take each fragment as a change of its own (and its apps
        // and machines would get them so): live documents need an update of Craft Cloud first.
        let api = client.agent_api()?;
        if api < LOCAL_API_FRAGMENTS {
            return Err(Error::Refused { code: codes::AGENT_TOO_OLD.into(), message: AGENT_TOO_OLD.into() });
        }
        let stream = TcpStream::connect_timeout(&client.addr(), CONNECT_TIMEOUT).map_err(|e| Error::NotRunning(format!("can't reach it: {e}")))?;
        stream.set_nodelay(true).map_err(|e| Error::Io(e.to_string()))?;
        stream.set_read_timeout(Some(OPEN_TIMEOUT)).map_err(|e| Error::Io(e.to_string()))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT)).map_err(|e| Error::Io(e.to_string()))?;
        let mut request = format!("ws://{}{}", client.addr(), paths::DOCS).into_client_request().map_err(|e| Error::Protocol(e.to_string()))?;
        let auth = format!("Bearer {}", client.token()).parse().map_err(|_| Error::Protocol("the token can't be sent".into()))?;
        request.headers_mut().insert("authorization", auth);
        let limit = MAX_BINARY_BYTES.max(MAX_LOCAL_JSON_BYTES);
        let config = WebSocketConfig::default().max_message_size(Some(limit)).max_frame_size(Some(limit));
        let socket = match tungstenite::client::client_with_config(request, stream, Some(config)) {
            Ok((socket, _)) => socket,
            Err(HandshakeError::Failure(tungstenite::Error::Http(response))) => {
                let status = response.status().as_u16();
                let body = response.into_body().unwrap_or_default();
                return Err(match serde_json::from_slice::<ErrorBody>(&body) {
                    Ok(body) => Error::Refused { code: body.error, message: body.message },
                    Err(_) => Error::Protocol(format!("HTTP {status}")),
                });
            }
            Err(e) => return Err(Error::Io(e.to_string())),
        };
        // Two WebSockets over the one connection, a reader's and a writer's, so a write never waits for a read. (A
        // read timeout is the socket's, not one handle's: the reader's wakes it now and then to look whether to
        // stop.) The agent sends nothing before the app's first request, so the handshake left nothing unread; the
        // reader never writes (the agent sends no pings), so writes never interleave.
        let clone = |s: &TcpStream| s.try_clone().map_err(|e| Error::Io(e.to_string()));
        let (read_side, write_side, stream) = (clone(socket.get_ref())?, clone(socket.get_ref())?, clone(socket.get_ref())?);
        drop(socket);
        read_side.set_read_timeout(Some(IDLE_WAKE)).map_err(|e| Error::Io(e.to_string()))?;
        let reader_socket = WebSocket::from_raw_socket(read_side, tungstenite::protocol::Role::Client, Some(config));
        let socket = WebSocket::from_raw_socket(write_side, tungstenite::protocol::Role::Client, Some(config));
        let shared = Arc::new(Shared {
            socket: Mutex::new(socket),
            routes: Mutex::new(HashMap::new()),
            answers: Mutex::new(HashMap::new()),
            opening: Mutex::new(HashMap::new()),
            filling: Mutex::new(HashMap::new()),
            numbers: Mutex::new(HashMap::new()),
            wakers: Mutex::new(HashMap::new()),
            inner_seqs: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            stream,
        });
        let reader_shared = shared.clone();
        let reader = std::thread::Builder::new()
            .name("craft-cloud-docs".into())
            .spawn(move || read_loop(reader_shared, reader_socket))
            .map_err(|e| Error::Io(e.to_string()))?;
        Ok(Self { shared, reader: Some(reader), api })
    }

    /// The local API version the agent spoke when this connected (at least [`LOCAL_API_FRAGMENTS`]): an app tells the
    /// document's other machines with its presence, so a newer one can say to update an older one.
    pub fn agent_api(&self) -> u32 {
        self.api
    }

    /// Opens the file at `path` (relative to the synced folder, `/`-separated, or absolute inside it) for `app` (its
    /// lowercase name, as in recents). Waits for the answer, [`OPEN_TIMEOUT`] at most.
    pub fn open(&self, path: &str, app: &str) -> Result<Opened, Error> {
        self.open_with(path, app, &OpenOptions::default())
    }

    /// [`Documents::open`], with what the app knows already ([`OpenOptions`]).
    pub fn open_with(&self, path: &str, app: &str, options: &OpenOptions) -> Result<Opened, Error> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (events_tx, events) = mpsc::channel();
        lock(&self.shared.routes).insert(id, events_tx);
        let file_hash = options.file_hash.map(|h| h.iter().map(|b| format!("{b:02x}")).collect());
        let request = DocsRequest::Open { id, path: path.to_owned(), app: app.to_owned(), file_hash };
        let answer = self.ask(id, || self.shared.send_json(&request));
        self.opened(id, answer, events)
    }

    /// Sends what `send` sends and waits for the answer to `id`.
    fn ask(&self, id: u64, send: impl FnOnce() -> Result<(), Error>) -> Result<Answer, Error> {
        let (answer_tx, answer) = mpsc::channel();
        lock(&self.shared.answers).insert(id, answer_tx);
        if let Err(e) = send() {
            lock(&self.shared.answers).remove(&id);
            return Err(e);
        }
        match answer.recv_timeout(OPEN_TIMEOUT) {
            Ok(answer) => Ok(answer),
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.shared.answers).remove(&id);
                Err(Error::Io("the agent didn't answer the open in time".into()))
            }
            Err(RecvTimeoutError::Disconnected) => Err(Error::Io("the connection to the agent is closed".into())),
        }
    }

    fn opened(&self, id: u64, answer: Result<Answer, Error>, events: Receiver<Event>) -> Result<Opened, Error> {
        let forget = || {
            lock(&self.shared.routes).remove(&id);
        };
        match answer {
            Ok(Answer::Live { head, snapshot, updates }) => {
                Ok(Opened::Live(Document { id, head, snapshot, updates, events, shared: self.shared.clone(), seq: AtomicU64::new(0) }))
            }
            Ok(Answer::NeedsImport) => Ok(Opened::NeedsImport(Importer { id, events, shared: self.shared.clone() })),
            Ok(Answer::NotLive(reason)) => {
                forget();
                Ok(Opened::NotLive(reason))
            }
            Ok(Answer::Refused(e)) | Err(e) => {
                forget();
                Err(e)
            }
        }
    }
}

impl Drop for Documents {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        let _ = lock(&self.shared.socket).close(None);
        let _ = self.shared.stream.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.shared.end("the connection was closed here");
    }
}

/// A file that has never been live, waiting for its content.
pub struct Importer {
    id: u64,
    events: Receiver<Event>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Importer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Importer").field("id", &self.id).finish()
    }
}

impl Importer {
    /// Sends the file's content (a Loro snapshot, at most the snapshot limit) and waits until it is live: this app's
    /// import, or another machine's that was there first (the [`Document::state`] is then that one). A file that can't
    /// be made live now opens as a plain file ([`Error::Refused`] with `not_live`).
    pub fn import(self, snapshot: &[u8]) -> Result<Document, Error> {
        let (answer_tx, answer) = mpsc::channel();
        lock(&self.shared.answers).insert(self.id, answer_tx);
        if let Err(e) = self.shared.send_binary(IMPORT, self.id, snapshot) {
            lock(&self.shared.answers).remove(&self.id);
            return Err(e);
        }
        let answer = answer.recv_timeout(OPEN_TIMEOUT).map_err(|_| Error::Io("the agent didn't answer the import in time".into()))?;
        match answer {
            Answer::Live { head, snapshot, updates } => {
                Ok(Document { id: self.id, head, snapshot, updates, events: self.events, shared: self.shared, seq: AtomicU64::new(0) })
            }
            Answer::NotLive(reason) => {
                Err(Error::Refused { code: "not_live".into(), message: format!("the file opens as a plain file ({reason:?})") })
            }
            Answer::NeedsImport => Err(Error::Protocol("asked for the import again".into())),
            Answer::Refused(e) => Err(e),
        }
    }
}

/// An open live document.
pub struct Document {
    id: u64,
    head: Head,
    snapshot: Vec<u8>,
    updates: Vec<Vec<u8>>,
    events: Receiver<Event>,
    shared: Arc<Shared>,
    /// This app's changes sent so far (the agent numbers them the same way).
    seq: AtomicU64,
}

impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document").field("doc", &self.head.doc).field("state", &self.head.state).finish()
    }
}

impl Document {
    /// The document as it was when opened: a Loro snapshot, then [`Document::state_updates`] (from an older agent,
    /// updates still waiting for their history come as the first [`Event::Update`]s instead).
    pub fn state(&self) -> &[u8] {
        &self.snapshot
    }

    /// The updates that are part of the state (still waiting for their history), to import after it, in order.
    pub fn state_updates(&self) -> &[Vec<u8>] {
        &self.updates
    }

    /// Whether it was online, and how many changes waited, when opened.
    pub fn opened_state(&self) -> DocState {
        self.head.state
    }

    /// The live document's ID.
    pub fn id(&self) -> DocId {
        self.head.doc
    }

    /// For an open that named the file's hash ([`OpenOptions::file_hash`]): whether no Craft app's save of this
    /// document has that content (the file was edited outside Craft). None: the agent can't tell (or is older).
    pub fn changed_outside(&self) -> Option<bool> {
        self.head.changed_outside
    }

    /// Sends this app's change (a Loro update, of up to a snapshot's limit: one over 1 MiB goes in fragments, which the
    /// agent takes only together); returns its number, which [`Event::Acked`] (or [`Event::Refused`]) names once it is
    /// on the agent's disk. An update over the limit is refused here (`too_large`), and nothing is sent.
    pub fn send(&self, update: &[u8]) -> Result<u64, Error> {
        let frames = split(update, FRAME_LIMIT.load(Ordering::Relaxed))
            .map_err(|e| Error::Refused { code: craft_cloud_api::api::codes::TOO_LARGE.into(), message: e.to_string() })?;
        let count = frames.len() as u64;
        let first = self.seq.fetch_add(count, Ordering::Relaxed) + 1;
        let last = first + count - 1;
        if count > 1 {
            lock(&self.shared.inner_seqs).extend((first..last).map(|seq| ((self.id, seq), Inner::Of(last))));
        }
        self.shared.send_frames(UPDATE, self.id, &frames)?;
        Ok(last)
    }

    /// Tells the agent the app is about to write the document's file: the content's BLAKE3 `hash`, and the live
    /// document's frontiers it was made from (`(peer, counter)` heads). Send it after every change it covers
    /// ([`Document::send`]): the agent keeps it once those are in its copy, and records it with the file version it
    /// uploads for that content. Doesn't wait for an answer.
    pub fn saved(&self, hash: [u8; 32], frontiers: &[(u64, u32)]) -> Result<(), Error> {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let mut frontiers = frontiers.to_vec();
        frontiers.sort_unstable();
        frontiers.dedup();
        self.shared.send_json(&DocsRequest::Saved { doc: self.head.doc, hash: hex(&hash), frontiers })
    }

    /// Tells the agent, at once and before the file is written, that the app is writing the document's file with
    /// content `hash`, and that its record ([`Document::saved`]) follows: until it comes, the agent's sync waits for it
    /// rather than take the content for an edit outside Craft. An older agent answers `error`; nothing else changes.
    pub fn saving(&self, hash: [u8; 32]) -> Result<(), Error> {
        let hex = hash.iter().map(|x| format!("{x:02x}")).collect::<String>();
        self.shared.send_json(&DocsRequest::Saving { doc: self.head.doc, hash: hex })
    }

    /// Sends this app's presence (at most 4 KiB, at most 20 a second; more are dropped).
    pub fn presence(&self, bytes: &[u8]) -> Result<(), Error> {
        self.shared.send_binary(PRESENCE, self.id, bytes)
    }

    /// Calls `waker` (from the connection's reader thread) after each event that comes for this document from now on,
    /// and once when the connection ends: an app's UI repaints, then takes the events with [`Document::try_recv`].
    pub fn set_waker(&self, waker: Waker) {
        lock(&self.shared.wakers).insert(self.id, waker);
    }

    /// The next event, if one came.
    pub fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// The next event, waiting at most `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }

    /// Done with it (the agent keeps it).
    pub fn close(self) {
        drop(self);
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        lock(&self.shared.routes).remove(&self.id);
        lock(&self.shared.wakers).remove(&self.id);
        let mut numbers = lock(&self.shared.numbers);
        let doc = self.head.doc;
        let remaining = numbers.get_mut(&doc).map(|ids| {
            ids.retain(|id| *id != self.id);
            ids.len()
        });
        if remaining == Some(0) {
            numbers.remove(&doc);
            drop(numbers);
            let _ = self.shared.send_json(&DocsRequest::Close { doc });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shared state on a connection to nowhere (a socket pair stands in), to feed the reader's messages to.
    fn shared() -> (Arc<Shared>, std::net::TcpListener) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let socket = WebSocket::from_raw_socket(stream.try_clone().unwrap(), tungstenite::protocol::Role::Client, None);
        let shared = Arc::new(Shared {
            socket: Mutex::new(socket),
            routes: Mutex::new(HashMap::new()),
            answers: Mutex::new(HashMap::new()),
            opening: Mutex::new(HashMap::new()),
            filling: Mutex::new(HashMap::new()),
            numbers: Mutex::new(HashMap::new()),
            wakers: Mutex::new(HashMap::new()),
            inner_seqs: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            stream,
        });
        (shared, listener)
    }

    /// A state with updates that are part of it: answered once the last of them came, with them; the updates after
    /// it are the document's events.
    #[test]
    fn an_open_waits_for_the_updates_of_its_state() {
        let (shared, _listener) = shared();
        let (answer_tx, answer) = mpsc::channel();
        lock(&shared.answers).insert(5, answer_tx);
        let (events_tx, events) = mpsc::channel();
        lock(&shared.routes).insert(5, events_tx);
        let opened = DocsEvent::Opened {
            id: 5,
            doc: DocId::generate(),
            state: DocState { online: true, waiting: 0, bytes: 1, updates: 2 },
            changed_outside: None,
        };
        shared.on_message(Message::Text(serde_json::to_string(&opened).unwrap().into()));
        shared.on_message(Message::Binary(encode_binary(STATE, 5, b"s").into()));
        shared.on_message(Message::Binary(encode_binary(UPDATE, 5, b"1").into()));
        assert!(answer.try_recv().is_err(), "one of its updates is still to come");
        shared.on_message(Message::Binary(encode_binary(UPDATE, 5, b"2").into()));
        shared.on_message(Message::Binary(encode_binary(UPDATE, 5, b"3").into()));
        let Ok(Answer::Live { updates, .. }) = answer.try_recv() else { panic!("no answer") };
        assert_eq!(updates, vec![b"1".to_vec(), b"2".to_vec()]);
        assert_eq!(events.try_recv().unwrap(), Event::Update(b"3".to_vec()));
        assert!(events.try_recv().is_err());
    }

    /// The agent's answers for fragments that aren't a change's last aren't the app's; a refusal of one is the
    /// change's, under its last number, once (the answers still to come for the change are dropped).
    #[test]
    fn inner_answers_are_not_the_apps_but_a_refusal_refuses_the_change() {
        let (shared, _listener) = shared();
        let (events_tx, events) = mpsc::channel();
        lock(&shared.routes).insert(7, events_tx);
        let ack = |seq: u64| Message::Text(serde_json::to_string(&DocsEvent::Ack { id: 7, seq }).unwrap().into());
        let refusal = |seq: u64| {
            let e = DocsEvent::Error { id: Some(7), seq: Some(seq), code: "invalid".into(), message: "no".into() };
            Message::Text(serde_json::to_string(&e).unwrap().into())
        };
        lock(&shared.inner_seqs).extend([((7, 1), Inner::Of(3)), ((7, 2), Inner::Of(3))]);
        for seq in 1..=3 {
            shared.on_message(ack(seq));
        }
        assert_eq!(events.try_recv().unwrap(), Event::Acked(3));
        assert!(events.try_recv().is_err());
        // The next change, of fragments 4 to 6: its second is refused.
        lock(&shared.inner_seqs).extend([((7, 4), Inner::Of(6)), ((7, 5), Inner::Of(6))]);
        shared.on_message(ack(4));
        shared.on_message(refusal(5));
        assert_eq!(events.try_recv().unwrap(), Event::Refused { seq: 6, code: "invalid".into(), message: "no".into() });
        shared.on_message(refusal(6));
        assert!(events.try_recv().is_err(), "refused once");
        assert!(lock(&shared.inner_seqs).is_empty());
    }

    /// `opened` then the state: one answer; the events after it go to the document, its status by document ID.
    #[test]
    fn an_open_is_answered_once_its_state_came() {
        let (shared, _listener) = shared();
        let (answer_tx, answer) = mpsc::channel();
        lock(&shared.answers).insert(4, answer_tx);
        let (events_tx, events) = mpsc::channel();
        lock(&shared.routes).insert(4, events_tx);
        let doc = DocId::generate();
        let opened =
            DocsEvent::Opened { id: 4, doc, state: DocState { online: true, waiting: 0, bytes: 3, updates: 0 }, changed_outside: Some(true) };
        shared.on_message(Message::Text(serde_json::to_string(&opened).unwrap().into()));
        assert!(answer.try_recv().is_err(), "not before the state");
        shared.on_message(Message::Binary(encode_binary(STATE, 4, b"abc").into()));
        let Ok(Answer::Live { snapshot, head, .. }) = answer.try_recv() else { panic!("no answer") };
        assert_eq!(snapshot, b"abc");
        assert_eq!(head.changed_outside, Some(true));
        shared.on_message(Message::Binary(encode_binary(UPDATE, 4, b"u").into()));
        let status = DocsEvent::Status { doc, online: false, waiting: 2, deleted: true, stopped: true };
        shared.on_message(Message::Text(serde_json::to_string(&status).unwrap().into()));
        shared.on_message(Message::Binary(vec![9, 1].into()));
        shared.on_message(Message::Text("not json".into()));
        assert_eq!(events.try_recv().unwrap(), Event::Update(b"u".to_vec()));
        assert_eq!(events.try_recv().unwrap(), Event::Status { online: false, waiting: 2, deleted: true, stopped: true });
        assert!(events.try_recv().is_err(), "junk goes nowhere");
        shared.end("gone");
        assert_eq!(events.try_recv().unwrap(), Event::Closed("gone".into()));
    }
}
