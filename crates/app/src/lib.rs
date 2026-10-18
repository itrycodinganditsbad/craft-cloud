//! What Craft apps and the launcher link to reach the Craft Cloud agent on this machine (`craft-cloud-agent run`)
//! through its local API: the account's status, signing in and out, joining with a code and adding computers (the
//! Craft Cloud app's), pausing, quitting and start-at-login, and each app's recent files, shared by every machine of
//! the account.
//!
//! Blocking, with no async runtime: an egui app calls it and gets changes on a channel ([`RecentsWatch::events`]),
//! which it polls each frame.
//!
//! **From a UI thread** (each call is bounded by a short timeout, the agent being on this machine): [`Client::connect`]
//! and [`Client::status`] (about 2 s at most), [`Client::recents`], [`Client::add_recent`],
//! [`Client::watch_recents`] and [`RecentsWatch::add`], [`Client::pause`], [`Client::resume`], [`Client::quit`] and
//! [`Client::set_autostart`] (a few seconds at most). **Off the UI thread**, since they wait for the server (up to
//! 90 s): [`Client::login`], [`Client::join`], [`Client::logout`], and the add-a-computer and computer-list calls
//! ([`Client::pairing`], [`Client::pairing_state`], [`Client::cancel_pairing`], [`Client::devices`],
//! [`Client::remove_device`]).
//!
//! ```no_run
//! # fn main() -> Result<(), craft_cloud_app::Error> {
//! let agent = craft_cloud_app::Client::connect()?;
//! let status = agent.status()?;
//! if status.signed_in {
//!     let recents = agent.watch_recents("wordcraft")?;
//!     recents.add(craft_cloud_app::NewRecent { path: "Letters/doc1.docx".into(), title: None, saved: true })?;
//!     for event in recents.events().try_iter() {
//!         // Redraw the list, or show "Craft Cloud isn't running" while it is unavailable.
//!         let _ = event;
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! [`Client::connect`] finds the agent as the agent finds its data folder (`$CRAFT_CLOUD_AGENT_DIR`, else the per-user
//! data folder), reads `local-api.json` there and asks the agent for its status; [`Error::NotRunning`] when the file
//! is missing or stale (its process is gone, or no agent answers on its port with its token). The app then works as
//! without Craft Cloud ("Craft Cloud isn't running"); it never blocks on it. A [`RecentsWatch`] outlives the agent:
//! when the agent stops or signs out it says so ([`RecentsEvent::Unavailable`]) and connects again by itself, with a
//! growing pause, to whichever agent runs next ([`RecentsEvent::Available`]).
//!
//! Dependencies: `craft-cloud-proto` (the local API types; serde, uuid, blake3, postcard and Unicode tables), `serde_json`,
//! `thiserror`, `httparse`, and `tungstenite` without TLS (with `http`, `sha1`, `rand`, `data-encoding`, `utf-8`). No
//! TLS: the agent is on this machine.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod http;
mod recents;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use craft_cloud_proto::api::ErrorBody;
use craft_cloud_proto::local::{AutostartRequest, LOCAL_API_FILE, LOCAL_API_MAX, LOCAL_API_MIN, LocalApiFile, LogoutRequest, paths};

pub use craft_cloud_proto::DeviceId;
pub use craft_cloud_proto::api::{DeviceInfo, PairingState, PairingStatus};
pub use craft_cloud_proto::local::{JoinRequest, LoginRequest, NewRecent, PairingInfo, RecentEntry, RunState, Status, codes};
pub use recents::{RecentsEvent, RecentsWatch};
pub use uuid::Uuid;

/// What went wrong talking to the agent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// No agent runs here (or not under this user): live features are off.
    #[error("Craft Cloud isn't running on this computer ({0})")]
    NotRunning(String),
    /// The agent speaks another local API version: one of the two needs an update.
    #[error("the Craft Cloud agent speaks local API version {0}, this app {LOCAL_API_MIN}-{LOCAL_API_MAX}: update the older one")]
    Version(u32),
    /// The agent refused the request: `code` is one of [`codes`] (or the HTTP API's), `message` is for people.
    #[error("{message}")]
    Refused { code: String, message: String },
    /// The connection broke, or the agent didn't answer in time.
    #[error("the connection to the Craft Cloud agent broke: {0}")]
    Io(String),
    /// The agent answered something this crate can't read.
    #[error("the Craft Cloud agent answered something unexpected: {0}")]
    Protocol(String),
}

/// The running agent, as `local-api.json` names it.
#[derive(Debug, Clone)]
pub struct Client {
    addr: SocketAddr,
    token: String,
    /// Where `local-api.json` is: a watch looks there again to reconnect.
    dir: PathBuf,
}

impl Client {
    /// Finds the running agent: its data folder from `$CRAFT_CLOUD_AGENT_DIR` or the per-user data folder.
    pub fn connect() -> Result<Self, Error> {
        let dir = craft_cloud_proto::local::data_dir(|name| std::env::var_os(name))
            .ok_or_else(|| Error::NotRunning("no user data folder to look in".into()))?;
        Self::connect_in(&dir)
    }

    /// Finds the agent running with the data folder `dir` (its `--data-dir`).
    pub fn connect_in(dir: &Path) -> Result<Self, Error> {
        let file: PathBuf = dir.join(LOCAL_API_FILE);
        let bytes = std::fs::read(&file).map_err(|e| Error::NotRunning(format!("{}: {e}", file.display())))?;
        let record: LocalApiFile = serde_json::from_slice(&bytes).map_err(|e| Error::NotRunning(format!("{} doesn't read: {e}", file.display())))?;
        // A file a crashed agent left behind names a port nobody, or someone else, answers on: the connection tells.
        // (Its pid isn't used for that: an app in a sandbox with a PID namespace of its own can't see the agent's.)
        let client = Self { addr: SocketAddr::from((Ipv4Addr::LOCALHOST, record.port)), token: record.token, dir: dir.to_owned() };
        match client.status() {
            Ok(status) if (LOCAL_API_MIN..=LOCAL_API_MAX).contains(&status.api) => Ok(client),
            Ok(status) => Err(Error::Version(status.api)),
            Err(Error::Refused { code, .. }) if code == "unauthorized" => Err(Error::NotRunning(format!("{} is stale", file.display()))),
            Err(Error::Io(e) | Error::Protocol(e)) => Err(Error::NotRunning(format!("{} is stale ({e})", file.display()))),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn call<T: serde::de::DeserializeOwned>(&self, method: &str, path: &str, body: Option<Vec<u8>>, timeout: Duration) -> Result<T, Error> {
        let answer = self.answer(method, path, body, timeout)?;
        serde_json::from_slice(&answer.body).map_err(|e| Error::Protocol(e.to_string()))
    }

    /// A call answered without a body (`204`).
    fn call_empty(&self, method: &str, path: &str, timeout: Duration) -> Result<(), Error> {
        self.answer(method, path, None, timeout).map(|_| ())
    }

    /// The answer if it succeeded, else the agent's refusal.
    fn answer(&self, method: &str, path: &str, body: Option<Vec<u8>>, timeout: Duration) -> Result<http::Answer, Error> {
        let answer = http::request(self.addr, &self.token, method, path, body.as_deref(), timeout)?;
        if (200..300).contains(&answer.status) {
            Ok(answer)
        } else {
            match serde_json::from_slice::<ErrorBody>(&answer.body) {
                Ok(body) => Err(Error::Refused { code: body.error, message: body.message }),
                Err(_) => Err(Error::Protocol(format!("HTTP {}", answer.status))),
            }
        }
    }

    /// Whether the agent is signed in, as whom, to which server, whether it is online and what it is doing. Quick
    /// (about 2 s at most): fine from a UI thread.
    pub fn status(&self) -> Result<Status, Error> {
        self.call("GET", paths::STATUS, None, http::QUICK_TIMEOUT)
    }

    /// Signs the agent in (the launcher's call). The password goes to the agent only. A wrong one is
    /// `Refused { code: "wrong_password" }`. Waits for the server (up to 90 s): call it off the UI thread.
    pub fn login(&self, request: &LoginRequest) -> Result<Status, Error> {
        let body = serde_json::to_vec(request).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::LOGIN, Some(body), http::SLOW_TIMEOUT)
    }

    /// Signs the agent out (the files stay). `local`: without reaching the server (the device stays valid there).
    /// Waits for the server (up to 90 s): call it off the UI thread.
    pub fn logout(&self, local: bool) -> Result<Status, Error> {
        let body = serde_json::to_vec(&LogoutRequest { local }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::LOGOUT, Some(body), http::SLOW_TIMEOUT)
    }

    /// Joins a Craft Cloud with a join string (an invite, or an add-a-computer code from a signed-in computer), then
    /// syncs as after [`Client::login`]. An invite to a new account without a password is
    /// `Refused { code: "password_required" }`: ask for one (twice) and call again with it. A code that doesn't work
    /// is `invalid_code`. Waits for the server (up to 90 s): call it off the UI thread.
    pub fn join(&self, request: &JoinRequest) -> Result<Status, Error> {
        let body = serde_json::to_vec(request).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::JOIN, Some(body), http::SLOW_TIMEOUT)
    }

    /// A new add-a-computer code of the signed-in account, to show on this computer and enter on the new one (it
    /// replaces this computer's earlier one; it works once, for 10 minutes). Asks the server: call it off the UI thread.
    pub fn pairing(&self) -> Result<PairingInfo, Error> {
        self.call("POST", paths::PAIRINGS, None, http::SLOW_TIMEOUT)
    }

    /// Where the add-a-computer code `id` stands: waiting, used (with the new computer's name) or expired. One that is
    /// gone is `Refused { code: "not_found" }`. Asks the server: call it off the UI thread (every 2 s is plenty).
    pub fn pairing_state(&self, id: Uuid) -> Result<PairingStatus, Error> {
        self.call("GET", &format!("{}/{id}", paths::PAIRINGS), None, http::SLOW_TIMEOUT)
    }

    /// Cancels the add-a-computer code `id`. Asks the server: call it off the UI thread.
    pub fn cancel_pairing(&self, id: Uuid) -> Result<(), Error> {
        self.call_empty("DELETE", &format!("{}/{id}", paths::PAIRINGS), http::SLOW_TIMEOUT)
    }

    /// The account's computers, this one marked `current`. Asks the server: call it off the UI thread.
    pub fn devices(&self) -> Result<Vec<DeviceInfo>, Error> {
        self.call("GET", paths::DEVICES, None, http::SLOW_TIMEOUT)
    }

    /// Removes another computer from the account (its next request is refused; its files stay there), and gives the
    /// list after. This computer is `Refused { code: "cannot_remove_self" }`: that is [`Client::logout`]. Asks the
    /// server: call it off the UI thread.
    pub fn remove_device(&self, id: DeviceId) -> Result<Vec<DeviceInfo>, Error> {
        self.call("DELETE", &format!("{}/{id}", paths::DEVICES), None, http::SLOW_TIMEOUT)
    }

    /// Pauses syncing until [`Client::resume`], also across restarts; recents keep working. Answered by the agent
    /// itself (a few seconds at most): fine from a UI thread.
    pub fn pause(&self) -> Result<Status, Error> {
        self.call("POST", paths::PAUSE, None, http::COMMAND_TIMEOUT)
    }

    /// Syncs again after [`Client::pause`]. Answered by the agent itself (a few seconds at most): fine from a UI thread.
    pub fn resume(&self) -> Result<Status, Error> {
        self.call("POST", paths::RESUME, None, http::COMMAND_TIMEOUT)
    }

    /// Stops the agent (after the pass under way): nothing syncs until it starts again (at the next login, with
    /// autostart on). Answered at once, as it stops: fine from a UI thread.
    pub fn quit(&self) -> Result<Status, Error> {
        self.call("POST", paths::QUIT, None, http::COMMAND_TIMEOUT)
    }

    /// Starts the agent by itself at login on this computer (`on`), or not any more; [`Status::autostart`] says which.
    /// Answered by the agent itself (it writes a file or a registry value): fine from a UI thread.
    pub fn set_autostart(&self, on: bool) -> Result<Status, Error> {
        let body = serde_json::to_vec(&AutostartRequest { on }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::AUTOSTART, Some(body), http::COMMAND_TIMEOUT)
    }

    /// `app`'s recent files now, newest first.
    pub fn recents(&self, app: &str) -> Result<Vec<RecentEntry>, Error> {
        Ok(self.watch_recents(app)?.entries())
    }

    /// Adds a file to `app`'s recent files (kept on this machine's disk when this returns; on the others once the agent
    /// reaches the server).
    pub fn add_recent(&self, app: &str, entry: NewRecent) -> Result<RecentEntry, Error> {
        RecentsWatch::open(self, app, false)?.add(entry)
    }

    /// Follows `app`'s recent files: the list now ([`RecentsWatch::entries`]), and a [`RecentsEvent`] after every change,
    /// from this machine or another, and when the agent goes away or comes back. The connection is held by a thread of
    /// its own, until the watch is dropped. `NotRunning` if no agent runs right now.
    pub fn watch_recents(&self, app: &str) -> Result<RecentsWatch, Error> {
        RecentsWatch::open(self, app, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// What the fake agent was asked: method, path and body.
    type Asked = (String, String, String);

    /// A fake agent on a loopback port with `local-api.json` in a temporary folder: it answers `GET /v1/status` (the
    /// client asks it on connecting) and then each request with the next answer of `script` (status and body), and
    /// tells what it was asked.
    fn fake_agent(script: Vec<(u16, String)>) -> (tempfile::TempDir, Client, std::sync::mpsc::Receiver<Asked>) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (asked_tx, asked) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut script = script.into_iter();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (method, path, body) = loop {
                    let n = stream.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let mut headers = [httparse::EMPTY_HEADER; 16];
                    let mut request = httparse::Request::new(&mut headers);
                    if let httparse::Status::Complete(head) = request.parse(&buf).unwrap() {
                        let length = request
                            .headers
                            .iter()
                            .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                            .map_or(0, |h| std::str::from_utf8(h.value).unwrap().parse::<usize>().unwrap());
                        if buf.len() >= head + length {
                            let body = String::from_utf8(buf[head..head + length].to_vec()).unwrap();
                            break (request.method.unwrap().to_owned(), request.path.unwrap().to_owned(), body);
                        }
                    }
                    assert!(n > 0, "the client closed the connection mid-request");
                };
                let (status, answer) = if path == paths::STATUS && method == "GET" {
                    (200, serde_json::to_string(&status(false)).unwrap())
                } else {
                    let _ = asked_tx.send((method, path, body));
                    script.next().unwrap_or((500, String::new()))
                };
                let head = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n", answer.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(answer.as_bytes());
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let file = serde_json::json!({ "schema": 1, "port": port, "token": "t", "pid": std::process::id() });
        std::fs::write(dir.path().join(LOCAL_API_FILE), file.to_string()).unwrap();
        let client = Client::connect_in(dir.path()).unwrap();
        (dir, client, asked)
    }

    fn status(signed_in: bool) -> Status {
        Status {
            api: 1,
            signed_in,
            account: signed_in.then(|| "ana".into()),
            server: None,
            folder: None,
            machine: None,
            online: false,
            run_state: RunState::SignedOut,
            autostart: Some(signed_in),
        }
    }

    fn json(value: &impl serde::Serialize) -> String {
        serde_json::to_string(value).unwrap()
    }

    /// Every call sends its request to its path, and reads its answer; a refusal comes back with its code.
    #[test]
    fn each_call_sends_its_request_and_reads_its_answer() {
        let id = uuid::Uuid::now_v7();
        let device = craft_cloud_proto::DeviceId::generate();
        let info = PairingInfo { id, join: "c.example/K7M4-Q9TX".into(), link: "https://c.example/join/K7M4-Q9TX".into(), expires_ms: 9 };
        let used = PairingStatus { state: PairingState::Used, device_name: Some("lptp1".into()) };
        let list = vec![DeviceInfo { id: device, name: "lptp1".into(), created_ms: 1, last_used_ms: 2, revoked: false, current: false }];
        let refused = json(&ErrorBody { error: "password_required".into(), message: "choose a password".into(), ..ErrorBody::default() });
        let script = vec![
            (400, refused),
            (200, json(&status(true))),
            (200, json(&info)),
            (200, json(&used)),
            (204, String::new()),
            (200, json(&list)),
            (200, json(&list)),
            (200, json(&status(true))),
            (200, json(&status(true))),
            (200, json(&status(true))),
            (200, json(&status(true))),
        ];
        let (_dir, agent, asked) = fake_agent(script);
        let next = || asked.recv_timeout(Duration::from_secs(5)).unwrap();

        let request =
            JoinRequest { join: "c.example/K7M4-Q9TX".into(), folder: None, device_name: Some("lptp1".into()), adopt: true, password: None };
        let refused = agent.join(&request);
        assert!(matches!(refused, Err(Error::Refused { ref code, .. }) if code == "password_required"), "{refused:?}");
        let (method, path, body) = next();
        assert_eq!((method.as_str(), path.as_str()), ("POST", paths::JOIN));
        assert_eq!(serde_json::from_str::<JoinRequest>(&body).unwrap(), request);
        let with = JoinRequest { password: Some("a new password".into()), ..request };
        assert!(agent.join(&with).unwrap().signed_in);
        assert_eq!(serde_json::from_str::<JoinRequest>(&next().2).unwrap().password.as_deref(), Some("a new password"));

        assert_eq!(agent.pairing().unwrap(), info);
        assert_eq!(next().1, paths::PAIRINGS);
        assert_eq!(agent.pairing_state(id).unwrap(), used);
        assert_eq!(next().1, format!("{}/{id}", paths::PAIRINGS));
        agent.cancel_pairing(id).unwrap();
        assert_eq!(next(), ("DELETE".into(), format!("{}/{id}", paths::PAIRINGS), String::new()));
        assert_eq!(agent.devices().unwrap(), list);
        assert_eq!(next(), ("GET".into(), paths::DEVICES.to_owned(), String::new()));
        assert_eq!(agent.remove_device(device).unwrap(), list);
        assert_eq!(next(), ("DELETE".into(), format!("{}/{device}", paths::DEVICES), String::new()));
        for (call, path) in
            [(Client::pause as fn(&Client) -> Result<Status, Error>, paths::PAUSE), (Client::resume, paths::RESUME), (Client::quit, paths::QUIT)]
        {
            assert!(call(&agent).unwrap().signed_in);
            assert_eq!(next(), ("POST".into(), path.to_owned(), String::new()));
        }
        assert_eq!(agent.set_autostart(true).unwrap().autostart, Some(true));
        let (method, path, body) = next();
        assert_eq!((method.as_str(), path.as_str()), ("POST", paths::AUTOSTART));
        assert_eq!(serde_json::from_str::<AutostartRequest>(&body).unwrap(), AutostartRequest { on: true });
    }

    /// An agent that takes the connection but never answers (hung, or something else on its port) costs a
    /// UI thread at most a couple of seconds; it used to wait 90 s, the sign-in's time.
    #[test]
    fn a_silent_agent_is_given_up_on_quickly() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let held = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(6));
            drop(stream);
        });
        let dir = tempfile::tempdir().unwrap();
        let file = serde_json::json!({ "schema": 1, "port": port, "token": "t", "pid": std::process::id() });
        std::fs::write(dir.path().join(LOCAL_API_FILE), file.to_string()).unwrap();
        let started = std::time::Instant::now();
        let result = Client::connect_in(dir.path());
        assert!(matches!(result, Err(Error::NotRunning(_))), "{result:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(4), "took {:?}", started.elapsed());
        held.join().unwrap();
    }

    /// A pid this process can't see (an app in a sandbox with its own PID namespace, as Flatpak's) is
    /// no sign the agent stopped; the connect attempt decides.
    #[test]
    fn an_invisible_pid_leaves_it_to_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let file = serde_json::json!({ "schema": 1, "port": port, "token": "t", "pid": u32::MAX - 7 });
        std::fs::write(dir.path().join(LOCAL_API_FILE), file.to_string()).unwrap();
        let Err(Error::NotRunning(why)) = Client::connect_in(dir.path()) else { panic!("connected to nothing") };
        assert!(!why.contains("its agent stopped"), "told by the pid: {why}");
    }
}
