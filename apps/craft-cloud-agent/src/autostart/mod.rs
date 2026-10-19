//! The agent's own start-at-login entry, so a machine syncs after every restart without anyone starting it.
//!
//! - **Windows** ([`windows`]): the value `Craft Cloud` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`,
//!   running the background build `craft-cloud-agent-bg.exe run --log` (no console window).
//! - **macOS** ([`macos`]): `~/Library/LaunchAgents/ch.craftcloud.agent.plist`, run at load and restarted after a
//!   crash (at most every 30 s) but not after a clean quit.
//! - **Linux** ([`linux`]): a systemd user unit `craft-cloud-agent.service` (restarted on failure after 30 s, but not
//!   after exit code 3) when the session runs systemd, else `~/.config/autostart/craft-cloud-agent.desktop`. A unit of
//!   that name made by hand is adopted: rewritten with this agent's path.
//!
//! There is one entry per user, and it belongs to the agent with the default data folder ([`Roots::owns`]): it runs
//! `<agent> run --log`, which writes what it says to `run.log` in the data folder. An agent with a data folder of its own (`CRAFT_CLOUD_AGENT_DIR`, `--data-dir`) never registers,
//! replaces or removes it. Paths are quoted and escaped for each format, spaces and non-ASCII names included.
//! Registering twice writes one entry; removing one that isn't there is fine.
//!
//! Where things go is a [`Roots`]: the home folder, the registry key and how commands run, so tests write into
//! temporary places and record the commands instead of running them.

pub mod linux;
pub mod macos;
pub mod windows;

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Why the start-at-login entry couldn't be read, written or removed.
#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    #[error("{}: {source}", .path.display())]
    Io { path: PathBuf, source: std::io::Error },
    #[error("the start-at-login entry in the registry: {0}")]
    Registry(String),
    /// A path that can't be written into the entry (not UTF-8, or with control characters).
    #[error("{} can't go into a start-at-login entry (its name isn't plain text)", .0.display())]
    BadPath(PathBuf),
    /// Windows: the background build isn't next to this agent.
    #[error("{} is missing: start at login needs the background build next to this agent", .0.display())]
    NoBackgroundBuild(PathBuf),
    #[error("can't find your home folder")]
    NoHome,
    #[error("start at login isn't supported on this system")]
    Unsupported,
}

pub(crate) fn io_at(path: &Path) -> impl FnOnce(std::io::Error) -> AutostartError + '_ {
    move |source| AutostartError::Io { path: path.to_owned(), source }
}

/// Runs a command for the Linux entry (`systemctl --user …`): whether it ran and exited successfully. An error only if
/// it couldn't be started at all.
pub trait Runner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool>;
}

/// Runs commands for real, with no input and their output thrown away.
#[derive(Debug, Clone, Copy, Default)]
pub struct System;

impl Runner for System {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
        use std::process::Stdio;
        let status = std::process::Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status()?;
        Ok(status.success())
    }
}

/// Where the entry goes. [`Roots::for_user`] is the real one.
#[derive(Clone)]
pub struct Roots {
    /// The user's home folder (macOS and Linux).
    pub home: PathBuf,
    /// The registry key under `HKEY_CURRENT_USER` that holds the value (Windows).
    pub run_key: String,
    /// How `systemctl` runs (Linux).
    pub runner: Arc<dyn Runner>,
    /// The default data folder: the entry belongs to the agent that uses it, and to no other.
    pub default_data: Option<PathBuf>,
}

impl std::fmt::Debug for Roots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Roots").field("home", &self.home).field("run_key", &self.run_key).finish_non_exhaustive()
    }
}

/// The key of programs Windows starts at the user's login.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

impl Roots {
    /// This user's: the home folder from `HOME` (`USERPROFILE` on Windows), [`RUN_KEY`], commands run for real.
    pub fn for_user() -> Result<Self, AutostartError> {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).filter(|p| p.is_absolute());
        let default_data = craft_cloud_proto::local::default_data_dir(|name| std::env::var_os(name));
        Ok(Self { home: home.ok_or(AutostartError::NoHome)?, run_key: RUN_KEY.to_owned(), runner: Arc::new(System), default_data })
    }

    /// Whether the entry belongs to an agent with data folder `data`: only to the one with the default data folder.
    /// An agent run with `CRAFT_CLOUD_AGENT_DIR` or `--data-dir` elsewhere (a spare folder, a test) never registers,
    /// replaces or removes it. The two are compared as absolute paths with `.` and `..` taken out (without
    /// following links), so `~/.local/share/x/../craft-cloud-agent` is the default folder too.
    pub fn owns(&self, data: &Path) -> bool {
        self.default_data.as_deref().is_some_and(|default| comparable(default) == comparable(data))
    }
}

/// `path` made absolute (against the current folder), with `.` and `..` resolved by name.
fn comparable(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// What the entry starts: `exe run --log`, with the default data folder (the only agent with an entry). The agent
/// writes what it says to `run.log` in its data folder itself (rotated at 10 MiB: see [`crate::runlog`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub exe: PathBuf,
    /// Where the service manager writes what reaches the process's own output despite `--log` (a crash's message),
    /// where it keeps that itself (macOS: `launchd.log` in the data folder). Windows has no such output; systemd
    /// keeps a journal.
    pub log: Option<PathBuf>,
}

impl Launch {
    /// The arguments after the program, as text.
    pub(crate) fn args(&self) -> Result<Vec<String>, AutostartError> {
        Ok(vec!["run".to_owned(), "--log".to_owned()])
    }

    /// The program, as text.
    pub(crate) fn program(&self) -> Result<String, AutostartError> {
        text(&self.exe)
    }
}

/// `path` as plain text for an entry: UTF-8 without control characters.
pub(crate) fn text(path: &Path) -> Result<String, AutostartError> {
    match path.to_str() {
        Some(s) if !s.chars().any(char::is_control) => Ok(s.to_owned()),
        _ => Err(AutostartError::BadPath(path.to_owned())),
    }
}

/// The entry for this agent, whose data folder is `data` (the default one: see [`Roots::owns`]): its own program
/// (on Windows the background build next to it), logging to `run.log` there; on macOS what launchd catches goes to
/// `launchd.log` next to it.
pub fn this_agent(data: &Path) -> Result<Launch, AutostartError> {
    let exe = std::env::current_exe().map_err(io_at(Path::new("this program")))?;
    let exe = if cfg!(windows) { windows::background_build(&exe)? } else { exe };
    Ok(Launch { exe, log: Some(data.join(macos::LAUNCHD_LOG)) })
}

/// Why an agent with data folder `data` leaves the entry alone ([`Roots::owns`] is false).
pub fn not_ours(data: &Path) -> String {
    format!(
        "this agent uses its own data folder ({}): only the agent with the default data folder starts at login, so its \
         entry is left as it is",
        data.display()
    )
}

/// Registers `launch` to start at login, replacing an earlier entry of the agent's.
pub fn register(roots: &Roots, launch: &Launch) -> Result<(), AutostartError> {
    if cfg!(windows) {
        windows::register(roots, launch)
    } else if cfg!(target_vendor = "apple") {
        macos::register(roots, launch)
    } else if cfg!(unix) {
        linux::register(roots, launch)
    } else {
        Err(AutostartError::Unsupported)
    }
}

/// Removes the agent's start-at-login entry; one that isn't there is fine.
pub fn unregister(roots: &Roots) -> Result<(), AutostartError> {
    if cfg!(windows) {
        windows::unregister(roots)
    } else if cfg!(target_vendor = "apple") {
        macos::unregister(roots)
    } else if cfg!(unix) {
        linux::unregister(roots)
    } else {
        Err(AutostartError::Unsupported)
    }
}

/// Whether the agent starts at login.
pub fn is_registered(roots: &Roots) -> Result<bool, AutostartError> {
    if cfg!(windows) {
        windows::is_registered(roots)
    } else if cfg!(target_vendor = "apple") {
        macos::is_registered(roots)
    } else if cfg!(unix) {
        linux::is_registered(roots)
    } else {
        Err(AutostartError::Unsupported)
    }
}

/// Where the `run` loop puts its start-at-login entry.
#[derive(Debug, Clone, Copy, Default)]
pub enum Target {
    /// This user's ([`Roots::for_user`]): what `run` does.
    #[default]
    User,
    /// Nowhere: the status says nothing about it and `POST /v1/autostart` is refused (tests that must not touch the
    /// machine's own entry).
    Off,
    /// These roots (tests).
    At(&'static Roots),
}

impl Target {
    /// The roots, or `None` for [`Target::Off`].
    pub fn roots(self) -> Result<Option<Roots>, AutostartError> {
        match self {
            Target::User => Roots::for_user().map(Some),
            Target::Off => Ok(None),
            Target::At(roots) => Ok(Some(roots.clone())),
        }
    }
}

/// Writes `content` to `path` atomically, creating its folder; not owner-only (start-at-login files are read by the
/// session manager).
pub(crate) fn write_file(path: &Path, content: &str) -> Result<(), AutostartError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_at(parent))?;
    }
    crate::account::write_atomic(path, content.as_bytes(), false).map_err(io_at(path))
}

/// Removes `path`; one that isn't there is fine.
pub(crate) fn remove_file(path: &Path) -> Result<(), AutostartError> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io_at(path)(e)),
        _ => Ok(()),
    }
}
