//! What Craft apps and the launcher link to reach the Craft Cloud agent on this machine (`craft-cloud-agent run`)
//! through its local API: the account's status, signing in and out (the launcher's), and each app's recent files,
//! shared by every machine of the account.
//!
//! Blocking, with no async runtime: an egui app calls it and gets changes on a channel ([`RecentsWatch::events`]),
//! which it polls each frame.
//!
//! **From a UI thread** (each call is bounded by a short timeout, the agent being on this machine): [`Client::connect`]
//! and [`Client::status`] (about 2 s at most), [`Client::recents`], [`Client::add_recent`],
//! [`Client::watch_recents`] and [`RecentsWatch::add`] (a few seconds at most). **Off the UI thread**:
//! [`Client::login`] and [`Client::logout`], which wait for the server (up to 90 s).
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
use craft_cloud_proto::local::{LOCAL_API_FILE, LOCAL_API_MAX, LOCAL_API_MIN, LocalApiFile, LogoutRequest, paths};

pub use craft_cloud_proto::local::{LoginRequest, NewRecent, RecentEntry, RunState, Status, codes};
pub use recents::{RecentsEvent, RecentsWatch};

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
        let answer = http::request(self.addr, &self.token, method, path, body.as_deref(), timeout)?;
        if (200..300).contains(&answer.status) {
            serde_json::from_slice(&answer.body).map_err(|e| Error::Protocol(e.to_string()))
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
