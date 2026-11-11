//! An app's recent files over the local API's WebSocket (`GET /v1/recents`), held by a thread of its own that connects
//! again whenever the connection goes (the agent stopped, restarted or signed out).

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use craft_cloud_api::SpaceId;
use craft_cloud_api::api::ErrorAnswer as ErrorBody;
use craft_cloud_api::local::{MAX_LOCAL_JSON_BYTES, NewRecent, RecentEntry, RecentsEvent as Wire, RecentsRequest, codes, paths};
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

use crate::http::CONNECT_TIMEOUT;
use crate::{Client, Error};

/// How long the first list may take (the handshake included).
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the answer to an add may take: the agent writes it to disk first.
const ADD_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the watch's thread looks for requests to send while it waits for the agent.
const POLL: Duration = Duration::from_millis(50);
/// The first pause before connecting again, doubled up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// What a watched list says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecentsEvent {
    /// The whole list, newest first, after a change (here or on another machine), and after connecting again.
    Changed(Vec<RecentEntry>),
    /// The agent can't be reached, or is signed out: live features are off ("Craft Cloud isn't running"). The watch
    /// connects again by itself; [`RecentsEvent::Available`] says when it did.
    Unavailable(String),
    /// Connected again after [`RecentsEvent::Unavailable`]; a [`RecentsEvent::Changed`] with the list follows.
    Available,
    /// Whether the agent stopped sending the list up whole (Loro failed twice on its copy): new entries still go up one
    /// by one, but other machines may lack some of the rest. An app may say so. Sent when it changes; see
    /// [`RecentsWatch::push_stopped`].
    PushStopped(bool),
}

/// What a list event from the agent brings: the list as last heard, and the push status (a [`RecentsEvent::PushStopped`]
/// when it changed, then the [`RecentsEvent::Changed`]).
fn heard(latest: &Mutex<Vec<RecentEntry>>, stopped: &AtomicBool, events: &Sender<RecentsEvent>, entries: Vec<RecentEntry>, push_stopped: bool) {
    *latest.lock().unwrap_or_else(PoisonError::into_inner) = entries.clone();
    if stopped.swap(push_stopped, Ordering::Relaxed) != push_stopped {
        let _ = events.send(RecentsEvent::PushStopped(push_stopped));
    }
    let _ = events.send(RecentsEvent::Changed(entries));
}

enum Command {
    Add { id: u64, entry: NewRecent, reply: Sender<Result<RecentEntry, Error>> },
}

/// A followed recents list. Dropping it ends the connection and its thread.
pub struct RecentsWatch {
    app: String,
    latest: Arc<Mutex<Vec<RecentEntry>>>,
    stopped: Arc<AtomicBool>,
    events: Receiver<RecentsEvent>,
    commands: Option<Sender<Command>>,
    next_id: AtomicU64,
    thread: Option<JoinHandle<()>>,
}

type Socket = WebSocket<TcpStream>;

fn refused(event: Wire) -> Error {
    match event {
        Wire::Error { code, message, .. } => Error::Refused { code, message, name_taken: None },
        other => Error::Protocol(format!("unexpected {other:?}")),
    }
}

fn read_event(socket: &mut Socket) -> Result<Option<Wire>, Error> {
    match socket.read() {
        Ok(Message::Text(text)) => serde_json::from_str(text.as_str()).map(Some).map_err(|e| Error::Protocol(e.to_string())),
        Ok(Message::Close(_)) => Err(Error::Io("the agent closed the connection".into())),
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
        Err(e) => Err(Error::Io(e.to_string())),
    }
}

fn send(socket: &mut Socket, request: &RecentsRequest) -> Result<(), Error> {
    let text = serde_json::to_string(request).map_err(|e| Error::Protocol(e.to_string()))?;
    socket.send(Message::Text(text.into())).map_err(|e| Error::Io(e.to_string()))
}

/// The error for a list the agent answered under another space than the one asked for: an agent from before team spaces answers
/// the personal list to every `subscribe`, whatever its `space`, and that is not the space's list.
fn wrong_space() -> Error {
    Error::Refused {
        code: codes::AGENT_TOO_OLD.into(),
        message: "the Craft Cloud agent answered another list than the space's: it is too old for team spaces".into(),
        name_taken: None,
    }
}

/// Opens the WebSocket to the agent `client` names; with `follow`, subscribes to `app` and returns the first list.
fn open(client: &Client, app: &str, space: Option<SpaceId>, follow: bool) -> Result<(Socket, (Vec<RecentEntry>, bool)), Error> {
    let stream = TcpStream::connect_timeout(&client.addr(), CONNECT_TIMEOUT).map_err(|e| Error::NotRunning(format!("can't reach it: {e}")))?;
    stream.set_read_timeout(Some(OPEN_TIMEOUT)).map_err(|e| Error::Io(e.to_string()))?;
    stream.set_write_timeout(Some(OPEN_TIMEOUT)).map_err(|e| Error::Io(e.to_string()))?;
    let mut request = format!("ws://{}{}", client.addr(), paths::RECENTS).into_client_request().map_err(|e| Error::Protocol(e.to_string()))?;
    let auth = format!("Bearer {}", client.token()).parse().map_err(|_| Error::Protocol("the token can't be sent".into()))?;
    request.headers_mut().insert("authorization", auth);
    let config = WebSocketConfig::default().max_message_size(Some(MAX_LOCAL_JSON_BYTES)).max_frame_size(Some(MAX_LOCAL_JSON_BYTES));
    let mut socket = match tungstenite::client::client_with_config(request, stream, Some(config)) {
        Ok((socket, _)) => socket,
        Err(HandshakeError::Failure(tungstenite::Error::Http(response))) => {
            let status = response.status().as_u16();
            let body = response.into_body().unwrap_or_default();
            return Err(match serde_json::from_slice::<ErrorBody>(&body) {
                Ok(body) => Error::Refused { code: body.error, message: body.message, name_taken: body.name_taken },
                Err(_) => Error::Protocol(format!("HTTP {status}")),
            });
        }
        Err(e) => return Err(Error::Io(e.to_string())),
    };
    let mut first = (Vec::new(), false);
    if follow {
        send(&mut socket, &RecentsRequest::Subscribe { app: app.to_owned(), space })?;
        let deadline = Instant::now() + OPEN_TIMEOUT;
        loop {
            match read_event(&mut socket)? {
                Some(Wire::Entries { entries, push_stopped, space: echoed, .. }) => {
                    // A space was asked for: the list must say it is that space's.
                    if space.is_some() && echoed != space {
                        return Err(wrong_space());
                    }
                    first = (entries, push_stopped);
                    break;
                }
                Some(error @ Wire::Error { .. }) => return Err(refused(error)),
                Some(_) => {}
                None if Instant::now() < deadline => {}
                None => return Err(Error::Io("no list from the agent".into())),
            }
        }
    }
    socket.get_mut().set_read_timeout(Some(POLL)).map_err(|e| Error::Io(e.to_string()))?;
    Ok((socket, first))
}

/// What [`lists`] is asked for: an app's list of a team space (`Some`) or the personal one.
pub type ListRequest = (String, Option<SpaceId>);
/// What [`lists`] answers for each request: the request, and the list or why there is none.
pub type ListAnswer = (String, Option<SpaceId>, Result<Vec<RecentEntry>, Error>);
/// One slot per request: its answer once it came.
type Slots = Vec<Option<Result<Vec<RecentEntry>, Error>>>;

/// Puts what the agent said (a list of `app`, echoing `space`, or an error) at the request it answers: `answers` is one slot per
/// request in `wanted`. A list answers the request of the same app and space; the personal list that an agent from before team
/// spaces sends for a team request is that request's [`wrong_space`]. An error names no request, so it is kept in `errors`
/// until it is known which request it was (see [`settle_errors`]).
fn place_answer(wanted: &[ListRequest], answers: &mut Slots, errors: &mut Vec<Error>, event: Wire) {
    let find = |answers: &Slots, wants: &dyn Fn(&ListRequest) -> bool| {
        (0..wanted.len()).find(|&i| answers.get(i).is_some_and(Option::is_none) && wanted.get(i).is_some_and(wants))
    };
    match event {
        Wire::Entries { app, entries, space, .. } => {
            let slot = find(answers, &|(a, s)| *a == app && *s == space);
            match (slot, space) {
                (Some(i), _) => answers[i] = Some(Ok(entries)),
                // The personal list again: a push of a change from a new agent while team lists are still to come, so it is
                // taken for the old agent's answer only when the personal list of that app was not asked for or has not come.
                (None, None) => {
                    let personal_has_come = wanted.iter().zip(answers.iter()).any(|((a, s), slot)| *a == app && s.is_none() && slot.is_some());
                    if !personal_has_come && let Some(i) = find(answers, &|(a, s)| *a == app && s.is_some()) {
                        answers[i] = Some(Err(wrong_space()));
                    }
                }
                // A list nobody asked for (or one that has come already): nothing to put.
                (None, Some(_)) => {}
            }
        }
        error @ Wire::Error { .. } => errors.push(refused(error)),
        Wire::Added { .. } => {}
    }
}

/// The errors the agent sent name no request, and its lists come in any order, so which request failed is only known once
/// every other has its list: when as many requests are open as errors came, the errors belong to them, in the order they were
/// asked (the agent refuses a `subscribe` before it reads the next).
fn settle_errors(answers: &mut Slots, errors: &mut Vec<Error>) {
    let open: Vec<usize> = answers.iter().enumerate().filter(|(_, a)| a.is_none()).map(|(i, _)| i).collect();
    if !errors.is_empty() && open.len() == errors.len() {
        for (i, error) in open.into_iter().zip(errors.drain(..)) {
            answers[i] = Some(Err(error));
        }
    }
}

/// The lists `wanted` over one connection: every `subscribe` is sent at once and the lists are collected as they come, until all
/// have or [`OPEN_TIMEOUT`] has passed (a list that did not come is an `Io` error), then the connection is closed. An error
/// that stops the connection itself is the call's; the rest are each list's.
pub(crate) fn lists(client: &Client, wanted: &[ListRequest]) -> Result<Vec<ListAnswer>, Error> {
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let (mut socket, _) = open(client, "", None, false)?;
    for (app, space) in wanted {
        send(&mut socket, &RecentsRequest::Subscribe { app: app.clone(), space: *space })?;
    }
    let mut answers: Slots = wanted.iter().map(|_| None).collect();
    let mut errors = Vec::new();
    let deadline = Instant::now() + OPEN_TIMEOUT;
    while Instant::now() < deadline && answers.iter().any(Option::is_none) {
        if let Some(event) = read_event(&mut socket)? {
            place_answer(wanted, &mut answers, &mut errors, event);
            settle_errors(&mut answers, &mut errors);
        }
    }
    let _ = socket.close(None);
    Ok(wanted
        .iter()
        .zip(answers)
        .map(|((app, space), answer)| (app.clone(), *space, answer.unwrap_or_else(|| Err(Error::Io("no list from the agent".into())))))
        .collect())
}

/// Whether an error means "no agent to talk to right now" (stopped, restarted elsewhere, signed out), so the watch
/// says so and connects again later.
fn unavailable(e: &Error) -> bool {
    match e {
        Error::NotRunning(_) | Error::Io(_) => true,
        Error::Refused { code, .. } | Error::RefusedWith { code, .. } => code == codes::NOT_SIGNED_IN,
        Error::Version(_) | Error::Protocol(_) => false,
    }
}

impl RecentsWatch {
    /// Connects; with `follow`, subscribes and waits for the first list. Fails if no agent answers now.
    pub(crate) fn open(client: &Client, app: &str, space: Option<SpaceId>, follow: bool) -> Result<Self, Error> {
        let (socket, (first, push_stopped)) = open(client, app, space, follow)?;
        let latest = Arc::new(Mutex::new(first));
        let stopped = Arc::new(AtomicBool::new(push_stopped));
        let (events_tx, events) = mpsc::channel();
        let (commands, commands_rx) = mpsc::channel();
        let link = Link {
            dir: client.dir().to_owned(),
            app: app.to_owned(),
            space,
            follow,
            latest: latest.clone(),
            stopped: stopped.clone(),
            events: events_tx,
            commands: commands_rx,
        };
        let thread =
            std::thread::Builder::new().name("craft-cloud-recents".into()).spawn(move || link.run(socket)).map_err(|e| Error::Io(e.to_string()))?;
        Ok(Self { app: app.to_owned(), latest, stopped, events, commands: Some(commands), next_id: AtomicU64::new(1), thread: Some(thread) })
    }

    /// Whether the agent stopped sending this list up whole ([`RecentsEvent::PushStopped`]): a status an app may show.
    pub fn push_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// The list as last heard, newest first.
    pub fn entries(&self) -> Vec<RecentEntry> {
        self.latest.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Changes and availability, as they come (poll with `try_recv`/`try_iter`, or wait with `recv`).
    pub fn events(&self) -> &Receiver<RecentsEvent> {
        &self.events
    }

    /// Adds a file to the list; returns once the agent has it on disk (a few seconds at most). `NotRunning` while the
    /// agent can't be reached.
    pub fn add(&self, entry: NewRecent) -> Result<RecentEntry, Error> {
        let (reply, answer) = mpsc::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let commands = self.commands.as_ref().ok_or_else(|| Error::Io("the watch is closed".into()))?;
        commands.send(Command::Add { id, entry, reply }).map_err(|_| Error::Io("the watch is closed".into()))?;
        match answer.recv_timeout(ADD_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(Error::Io("the agent didn't answer the add in time".into())),
            Err(RecvTimeoutError::Disconnected) => Err(Error::Io("the watch is closed".into())),
        }
    }

    /// The app whose list this is.
    pub fn app(&self) -> &str {
        &self.app
    }
}

impl std::fmt::Debug for RecentsWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecentsWatch").field("app", &self.app).field("entries", &self.entries().len()).finish()
    }
}

impl Drop for RecentsWatch {
    fn drop(&mut self) {
        // The thread sees its requests end and closes the connection.
        self.commands = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The watch's thread.
struct Link {
    dir: PathBuf,
    app: String,
    space: Option<SpaceId>,
    follow: bool,
    latest: Arc<Mutex<Vec<RecentEntry>>>,
    stopped: Arc<AtomicBool>,
    events: Sender<RecentsEvent>,
    commands: Receiver<Command>,
}

/// Why a connected stretch ended.
enum Ended {
    /// The watch was dropped.
    Dropped,
    /// The connection went: why.
    Gone(Error),
}

impl Link {
    /// Serves the connection, and after it goes, connects again with a growing pause until the watch is dropped.
    fn run(self, socket: Socket) {
        let mut socket = Some(socket);
        let mut pause = RETRY_FIRST;
        loop {
            if let Some(s) = socket.take() {
                match self.serve(s) {
                    Ended::Dropped => return,
                    Ended::Gone(e) => {
                        let _ = self.events.send(RecentsEvent::Unavailable(e.to_string()));
                        if !self.follow || !unavailable(&e) {
                            // A one-shot connection (an add), or one the agent will never take: done.
                            self.drain_until_dropped(&e);
                            return;
                        }
                    }
                }
            }
            if !self.wait(pause) {
                return;
            }
            match Client::connect_in(&self.dir).and_then(|client| open(&client, &self.app, self.space, true)) {
                Ok((s, (entries, push_stopped))) => {
                    pause = RETRY_FIRST;
                    let _ = self.events.send(RecentsEvent::Available);
                    heard(&self.latest, &self.stopped, &self.events, entries, push_stopped);
                    socket = Some(s);
                }
                Err(_) => pause = pause.saturating_mul(2).min(RETRY_MAX),
            }
        }
    }

    /// Waits `pause`, answering adds with `NotRunning` meanwhile; false once the watch was dropped.
    fn wait(&self, pause: Duration) -> bool {
        let until = Instant::now() + pause;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            match self.commands.recv_timeout(left) {
                Ok(Command::Add { reply, .. }) => {
                    let _ = reply.send(Err(Error::NotRunning("the Craft Cloud agent can't be reached right now".into())));
                }
                Err(RecvTimeoutError::Timeout) => return true,
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    /// After a connection that won't come back: every add is answered with why, until the watch is dropped.
    fn drain_until_dropped(&self, why: &Error) {
        while let Ok(Command::Add { reply, .. }) = self.commands.recv() {
            let _ = reply.send(Err(why.clone()));
        }
    }

    /// One connected stretch: sends adds, reads lists and answers, until the watch is dropped or the connection goes.
    fn serve(&self, mut socket: Socket) -> Ended {
        let mut waiting: HashMap<u64, Sender<Result<RecentEntry, Error>>> = HashMap::new();
        let end = loop {
            match self.commands.try_recv() {
                Ok(Command::Add { id, entry, reply }) => {
                    waiting.insert(id, reply);
                    if let Err(e) = send(&mut socket, &RecentsRequest::Add { id, app: self.app.clone(), entry }) {
                        break e;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = socket.close(None);
                    let _ = socket.flush();
                    return Ended::Dropped;
                }
            }
            match read_event(&mut socket) {
                Ok(Some(Wire::Entries { entries, push_stopped, .. })) => {
                    heard(&self.latest, &self.stopped, &self.events, entries, push_stopped);
                }
                Ok(Some(Wire::Added { id, entry })) => {
                    if let Some(reply) = waiting.remove(&id) {
                        let _ = reply.send(Ok(entry));
                    }
                }
                Ok(Some(error @ Wire::Error { id: Some(id), .. })) => {
                    if let Some(reply) = waiting.remove(&id) {
                        let _ = reply.send(Err(refused(error)));
                    }
                }
                Ok(Some(Wire::Error { id: None, code, message })) => {
                    if code == codes::NOT_SIGNED_IN {
                        break Error::Refused { code, message, name_taken: None };
                    }
                }
                Ok(None) => {}
                Err(e) => break e,
            }
        };
        for (_, reply) in waiting.drain() {
            let _ = reply.send(Err(end.clone()));
        }
        Ended::Gone(end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(app: &str, space: Option<SpaceId>, path: &str) -> Wire {
        let entry = RecentEntry {
            node: None,
            path: path.into(),
            title: path.into(),
            opened_ms: 1,
            saved_ms: None,
            machine: "pc".into(),
            open_now: Vec::new(),
        };
        Wire::Entries { app: app.into(), entries: vec![entry], push_stopped: false, space }
    }

    fn path_of(slot: &Option<Result<Vec<RecentEntry>, Error>>) -> Option<String> {
        slot.as_ref().and_then(|r| r.as_ref().ok()).and_then(|l| l.first()).map(|e| e.path.clone())
    }

    /// Lists are put at the request of the same app and space, in any order; a list asked for twice is not mixed up.
    #[test]
    fn lists_are_put_at_their_app_and_space() {
        let (a, b) = (SpaceId::generate(), SpaceId::generate());
        let wanted: Vec<ListRequest> =
            vec![("wordcraft".into(), None), ("wordcraft".into(), Some(a)), ("wordcraft".into(), Some(b)), ("photocraft".into(), Some(a))];
        let mut answers: Slots = wanted.iter().map(|_| None).collect();
        for event in
            [list("photocraft", Some(a), "p"), list("wordcraft", Some(b), "wb"), list("wordcraft", None, "w"), list("wordcraft", Some(a), "wa")]
        {
            place_answer(&wanted, &mut answers, &mut Vec::new(), event);
        }
        let paths: Vec<Option<String>> = answers.iter().map(path_of).collect();
        assert_eq!(paths, [Some("w".into()), Some("wa".into()), Some("wb".into()), Some("p".into())]);
        // A later push of a list that came already, or of one nobody asked for, changes nothing.
        place_answer(&wanted, &mut answers, &mut Vec::new(), list("wordcraft", Some(a), "newer"));
        place_answer(&wanted, &mut answers, &mut Vec::new(), list("soundcraft", Some(a), "x"));
        assert_eq!(path_of(&answers[1]).as_deref(), Some("wa"));
    }

    /// An agent from before team spaces answers the personal list to a team request: that is the space's error, never its list.
    /// A personal list that has come already and comes again (a change pushed by a new agent while the team lists are still to
    /// come) is not that: the team request stays open for its own list.
    #[test]
    fn a_list_under_another_space_is_not_the_spaces_list() {
        let a = SpaceId::generate();
        let wanted: Vec<ListRequest> = vec![("wordcraft".into(), None), ("wordcraft".into(), Some(a))];
        let mut answers: Slots = wanted.iter().map(|_| None).collect();
        place_answer(&wanted, &mut answers, &mut Vec::new(), list("wordcraft", None, "mine"));
        place_answer(&wanted, &mut answers, &mut Vec::new(), list("wordcraft", None, "mine, changed"));
        assert_eq!(path_of(&answers[0]).as_deref(), Some("mine"));
        assert!(answers[1].is_none(), "a repeated personal push leaves the team slot open: {:?}", answers[1]);
        place_answer(&wanted, &mut answers, &mut Vec::new(), list("wordcraft", Some(a), "theirs"));
        assert_eq!(path_of(&answers[1]).as_deref(), Some("theirs"));
        // An old agent, asked for the team list only, answers the personal one: that is the space's error.
        let only_team: Vec<ListRequest> = vec![("wordcraft".into(), Some(a))];
        let mut old: Slots = only_team.iter().map(|_| None).collect();
        place_answer(&only_team, &mut old, &mut Vec::new(), list("wordcraft", None, "mine"));
        assert!(matches!(&old[0], Some(Err(Error::Refused { code, .. })) if code == codes::AGENT_TOO_OLD), "{:?}", old[0]);
        // Asked for both, an old agent's second answer is not told from a push: the team request is left to time out (`Io`).
        let both: Vec<ListRequest> = wanted.clone();
        let mut slots: Slots = both.iter().map(|_| None).collect();
        place_answer(&both, &mut slots, &mut Vec::new(), list("wordcraft", None, "mine"));
        place_answer(&both, &mut slots, &mut Vec::new(), list("wordcraft", None, "mine"));
        assert!(slots[1].is_none());
        // An error names no request: it waits until as many requests are open as errors came, and then they are those.
        let (mut errors, mut open): (Vec<Error>, Slots) = (Vec::new(), wanted.iter().map(|_| None).collect());
        place_answer(&wanted, &mut open, &mut errors, Wire::Error { id: None, code: "not_found".into(), message: "no".into() });
        settle_errors(&mut open, &mut errors);
        assert!(open.iter().all(Option::is_none) && errors.len() == 1, "two open, one error: not yet known which");
        place_answer(&wanted, &mut open, &mut errors, list("wordcraft", None, "mine"));
        settle_errors(&mut open, &mut errors);
        assert_eq!(path_of(&open[0]).as_deref(), Some("mine"));
        assert!(matches!(&open[1], Some(Err(Error::Refused { code, .. })) if code == "not_found") && errors.is_empty());
    }

    /// A list event that says its push stopped: the watch's status follows, and an app hears it once, when it
    /// changes, before the list.
    #[test]
    fn a_watch_says_when_its_push_stopped() {
        let (latest, stopped) = (Mutex::new(Vec::new()), AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        heard(&latest, &stopped, &tx, Vec::new(), false);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![RecentsEvent::Changed(Vec::new())]);
        heard(&latest, &stopped, &tx, Vec::new(), true);
        heard(&latest, &stopped, &tx, Vec::new(), true);
        assert!(stopped.load(Ordering::Relaxed));
        let events: Vec<RecentsEvent> = rx.try_iter().collect();
        assert_eq!(events.iter().filter(|e| matches!(e, RecentsEvent::PushStopped(true))).count(), 1, "{events:?}");
        assert_eq!(events.first(), Some(&RecentsEvent::PushStopped(true)));
    }
}
