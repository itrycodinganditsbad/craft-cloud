//! The agent's own start-at-login entry, so a machine syncs after every restart without anyone starting it.
//!
//! - **Windows** ([`windows`]): the value `Craft Cloud` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`,
//!   running the background build `craft-cloud-agent-bg.exe run` (no console window; it logs to `run.log`).
//! - **macOS** ([`macos`]): `~/Library/LaunchAgents/ch.craftcloud.agent.plist`, run at load and restarted after a
//!   crash but not after a clean quit.
//! - **Linux** ([`linux`]): a systemd user unit `craft-cloud-agent.service` (restarted on failure) when the session
//!   runs systemd, else `~/.config/autostart/craft-cloud-agent.desktop`. A unit of that name made by hand is adopted:
//!   rewritten with this agent's path.
//!
//! Every entry runs `<agent> run`, with `--data-dir` when the agent's data folder isn't the default one (the login
//! session doesn't have the shell's `CRAFT_CLOUD_AGENT_DIR`). Paths are quoted and escaped for each format, spaces and
//! non-ASCII names included. Registering twice writes one entry; removing one that isn't there is fine.
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
        Ok(Self { home: home.ok_or(AutostartError::NoHome)?, run_key: RUN_KEY.to_owned(), runner: Arc::new(System) })
    }
}

/// What the entry starts: `exe run`, with `--data-dir data_dir` if set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub exe: PathBuf,
    pub data_dir: Option<PathBuf>,
}

impl Launch {
    /// The arguments after the program, as text: `run` and maybe `--data-dir <dir>`.
    pub(crate) fn args(&self) -> Result<Vec<String>, AutostartError> {
        let mut args = vec!["run".to_owned()];
        if let Some(dir) = &self.data_dir {
            args.push("--data-dir".to_owned());
            args.push(text(dir)?);
        }
        Ok(args)
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

/// The entry for this agent: its own program (on Windows the background build next to it), with the data folder
/// `data` if that isn't the default one.
pub fn this_agent(data: &Path) -> Result<Launch, AutostartError> {
    let exe = std::env::current_exe().map_err(io_at(Path::new("this program")))?;
    let exe = if cfg!(windows) { windows::background_build(&exe)? } else { exe };
    let default = craft_cloud_proto::local::default_data_dir(|name| std::env::var_os(name));
    let data_dir = (default.as_deref() != Some(data)).then(|| data.to_owned());
    Ok(Launch { exe, data_dir })
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
