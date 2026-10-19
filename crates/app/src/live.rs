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
use craft_cloud_api::local::docs::{IMPORT, MAX_BINARY_BYTES, PRESENCE, STATE, UPDATE, decode_binary, encode_binary};
use craft_cloud_api::local::{DocState, DocsEvent, DocsRequest, MAX_LOCAL_JSON_BYTES, paths};
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

/// What happens to an open document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A change from another machine or another app on this one: import it as it is.
    Update(Vec<u8>),
    /// Another machine's or app's presence (Loro's ephemeral state), as it was sent.
    Presence(Vec<u8>),
    /// This app's change number `seq` ([`Document::send`]'s answer) is on the agent's disk.
    Acked(u64),
    /// This app's change number `seq` was refused (it doesn't decode, or is over 1 MiB): the agent's copy doesn't have
    /// it.
    Refused { seq: u64, message: String },
    /// How the document stands: `online` (edits go out as they are made), changes waiting for the server, and whether
    /// its file was deleted (the document still works; a later open of the path is a new file), and whether the agent
    /// stopped following it until it restarts (Loro failed on it twice; changes made meanwhile are kept).
    Status { online: bool, waiting: u64, deleted: bool, stopped: bool },
    /// The connection to the agent ended.
    Closed(String),
}

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

/// What the reader hands an open in progress.
enum Answer {
    Live { doc: DocId, state: DocState, snapshot: Vec<u8> },
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
    opening: Mutex<HashMap<u64, (DocId, DocState)>>,
    /// Which numbers a document is open as (its status names the document).
    numbers: Mutex<HashMap<DocId, Vec<u64>>>,
    /// What to call after an event went to a document's channel, by number ([`Document::set_waker`]).
    wakers: Mutex<HashMap<u64, Waker>>,
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
                Ok(DocsEvent::Opened { id, doc, state }) => {
                    lock(&self.opening).insert(id, (doc, state));
                }
                Ok(DocsEvent::NeedsImport { id }) => self.answer(id, Answer::NeedsImport),
                Ok(DocsEvent::NotLive { id, reason, .. }) => self.answer(id, Answer::NotLive(reason)),
                Ok(DocsEvent::Ack { id, seq }) => self.route(id, Event::Acked(seq)),
                Ok(DocsEvent::Status { doc, online, waiting, deleted, stopped }) => {
                    let ids = lock(&self.numbers).get(&doc).cloned().unwrap_or_default();
                    for id in ids {
                        self.route(id, Event::Status { online, waiting, deleted, stopped });
                    }
                }
                Ok(DocsEvent::Error { id: Some(id), seq: Some(seq), message, .. }) => self.route(id, Event::Refused { seq, message }),
                Ok(DocsEvent::Error { id: Some(id), seq: None, code, message }) => self.answer(id, Answer::Refused(Error::Refused { code, message })),
                Ok(DocsEvent::Error { id: None, .. }) | Err(_) => {}
            },
            Message::Binary(bytes) => {
                let Some((kind, id, body)) = decode_binary(&bytes) else { return };
                match kind {
                    STATE => {
                        if let Some((doc, state)) = lock(&self.opening).remove(&id) {
                            lock(&self.numbers).entry(doc).or_default().push(id);
                            self.answer(id, Answer::Live { doc, state, snapshot: body.to_vec() });
                        }
                    }
                    UPDATE => self.route(id, Event::Update(body.to_vec())),
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
}

impl std::fmt::Debug for Documents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Documents").field("open", &lock(&self.shared.routes).len()).finish()
    }
}

impl Documents {
    pub(crate) fn connect(client: &Client) -> Result<Self, Error> {
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
            numbers: Mutex::new(HashMap::new()),
            wakers: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            stream,
        });
        let reader_shared = shared.clone();
        let reader = std::thread::Builder::new()
            .name("craft-cloud-docs".into())
            .spawn(move || read_loop(reader_shared, reader_socket))
            .map_err(|e| Error::Io(e.to_string()))?;
        Ok(Self { shared, reader: Some(reader) })
    }

    /// Opens the file at `path` (relative to the synced folder, `/`-separated, or absolute inside it) for `app` (its
    /// lowercase name, as in recents). Waits for the answer, [`OPEN_TIMEOUT`] at most.
    pub fn open(&self, path: &str, app: &str) -> Result<Opened, Error> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (events_tx, events) = mpsc::channel();
        lock(&self.shared.routes).insert(id, events_tx);
        let answer = self.ask(id, || self.shared.send_json(&DocsRequest::Open { id, path: path.to_owned(), app: app.to_owned() }));
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
            Ok(Answer::Live { doc, state, snapshot }) => {
                Ok(Opened::Live(Document { id, doc, state, snapshot, events, shared: self.shared.clone(), seq: AtomicU64::new(0) }))
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
            Answer::Live { doc, state, snapshot } => {
                Ok(Document { id: self.id, doc, state, snapshot, events: self.events, shared: self.shared, seq: AtomicU64::new(0) })
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
    doc: DocId,
    state: DocState,
    snapshot: Vec<u8>,
    events: Receiver<Event>,
    shared: Arc<Shared>,
    /// This app's changes sent so far (the agent numbers them the same way).
    seq: AtomicU64,
}

impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document").field("doc", &self.doc).field("state", &self.state).finish()
    }
}

impl Document {
    /// The document as it was when opened: a Loro snapshot (updates still waiting for their history come as the first
    /// [`Event::Update`]s).
    pub fn state(&self) -> &[u8] {
        &self.snapshot
    }

    /// Whether it was online, and how many changes waited, when opened.
    pub fn opened_state(&self) -> DocState {
        self.state
    }

    /// The live document's ID.
    pub fn id(&self) -> DocId {
        self.doc
    }

    /// Sends this app's change (a Loro update, at most 1 MiB); returns its number, which [`Event::Acked`] names once
    /// it is on the agent's disk.
    pub fn send(&self, update: &[u8]) -> Result<u64, Error> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.shared.send_binary(UPDATE, self.id, update)?;
        Ok(seq)
    }

    /// Tells the agent the app saved the document's file: the content's BLAKE3 `hash`, and the live document's
    /// version it was made from (Loro's encoded frontiers and its version vector). The agent records it with the file
    /// version it uploads for that content. Doesn't wait for an answer.
    pub fn saved(&self, hash: [u8; 32], frontiers: &[u8], version: &[(u64, u32)]) -> Result<(), Error> {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let mut version = version.to_vec();
        version.sort_unstable();
        self.shared.send_json(&DocsRequest::Saved { doc: self.doc, hash: hex(&hash), frontiers: hex(frontiers), version })
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
        let remaining = numbers.get_mut(&self.doc).map(|ids| {
            ids.retain(|id| *id != self.id);
            ids.len()
        });
        if remaining == Some(0) {
            numbers.remove(&self.doc);
            drop(numbers);
            let _ = self.shared.send_json(&DocsRequest::Close { doc: self.doc });
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
            numbers: Mutex::new(HashMap::new()),
            wakers: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            stream,
        });
        (shared, listener)
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
        let opened = DocsEvent::Opened { id: 4, doc, state: DocState { online: true, waiting: 0, bytes: 3 } };
        shared.on_message(Message::Text(serde_json::to_string(&opened).unwrap().into()));
        assert!(answer.try_recv().is_err(), "not before the state");
        shared.on_message(Message::Binary(encode_binary(STATE, 4, b"abc").into()));
        let Ok(Answer::Live { snapshot, .. }) = answer.try_recv() else { panic!("no answer") };
        assert_eq!(snapshot, b"abc");
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
