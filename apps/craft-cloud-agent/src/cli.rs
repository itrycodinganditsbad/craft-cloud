//! The `craft-cloud-agent` command line: `login`, `sync`, `status`, `logout`. Each command is a library
//! function too, so tests drive exactly what a user runs.
//!
//! The password is read from standard input (one line), so it never appears in the process list or shell
//! history: `printf '%s\n' "$PASSWORD" | craft-cloud-agent login https://cloud.example.com --user ana`.
//!
//! Exit codes ([`Exit`], also in `--help`): **0** done; **1** failed, try again (offline, a server error, a
//! timeout, a wrong password or a usage error; the message says what; a failed pass is saved and simply runs
//! again next time); **2** a sync pass finished but the mass-delete guard holds deletions back (run
//! `sync --approve-deletes` after checking the list); **3** stopped, needs you: a safety stop (the folder is
//! missing, its marker is missing or names another space, the data folder and the synced folder lie inside one
//! another, another agent is running) or an account-level problem where trying again never helps (the device was
//! revoked or signed out, `state.json` belongs to another account, the server speaks another protocol or chunker,
//! answers something this agent can't read, or it or a reverse proxy in front of it refuses a request with
//! another 4xx status, the server sent damaged data (a chunk or manifest that fails its hash), `account.json` or
//! `state.json` is damaged or was written by a newer agent: move the file away or update).
//!
//! A crash or kill during `login`, after the server created the device and before the account was saved, can
//! leave a device registered that nothing here uses; an admin can revoke it (`craft-cloud-server device ...`).

use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};

use clap::{Parser, Subcommand};
use craft_cloud_proto::ConflictStamp;
use craft_cloud_sync::{Config, Engine, SyncError, TransportError};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::account::{ACCOUNT_SCHEMA, Account, AccountError, DataDir, LastPass, Outcome, PassSummary, STATE_SCHEMA, SavedState};
use crate::disk::{AGENT_DIR, DiskFs, create_private_dir};
use crate::http::{self, DeviceToken, HttpTransport, ServerUrl};
use crate::lock::{self, DATA_LOCK, FOLDER_LOCK, Lock, LockError};
use crate::root::{self, RootError, RootMarker};

#[derive(Debug, Parser)]
#[command(
    name = "craft-cloud-agent",
    version,
    about = "Keeps a folder on this computer in step with a Craft Cloud server",
    after_help = "Exit codes: 0 done; 1 failed, try again (offline, server error, wrong password, bad command line); \
                  2 deletions wait for `sync --approve-deletes`; \
                  3 stopped, needs you (folder or its marker missing, another space, data folder and synced folder \
                  inside one another, another agent running, \
                  device revoked, state of another account, server protocol mismatch or a request the server or a proxy \
                  refused, damaged data from the server, damaged or newer account/state files)."
)]
pub struct Cli {
    /// Where the agent keeps its sign-in and sync state. Default: $CRAFT_CLOUD_AGENT_DIR, else the per-user data
    /// folder (~/.local/share/craft-cloud-agent, ~/Library/Application Support/craft-cloud-agent,
    /// %LOCALAPPDATA%\craft-cloud-agent).
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Sign in, register this computer as a device and set up the synced folder. The password is read from
    /// standard input.
    Login(LoginArgs),
    /// Run one sync pass and print what it did.
    Sync {
        /// Carry out the deletions the last pass listed as waiting for approval (exactly those; any new ones
        /// still wait).
        #[arg(long)]
        approve_deletes: bool,
    },
    /// Show the account, the folder and the last pass.
    Status,
    /// Sign this device out on the server and forget the account here. The files stay.
    Logout {
        /// Forget the account here without reaching the server (it is gone, or offline for good). The device
        /// stays valid on the server until an admin revokes it.
        #[arg(long)]
        local: bool,
    },
}

#[derive(Debug, Clone, clap::Args)]
pub struct LoginArgs {
    /// The server, e.g. https://cloud.example.com (http:// only for this computer).
    pub url: String,
    #[arg(long)]
    pub user: String,
    /// The folder to sync. Default: "Craft Cloud" in your home folder.
    #[arg(long)]
    pub folder: Option<PathBuf>,
    /// This computer's name in the device list and in conflict copy names. Default: the computer's name.
    #[arg(long)]
    pub device_name: Option<String>,
    /// Sync a folder that already has files in it. Files that differ from the server's become conflict
    /// copies; nothing is deleted.
    #[arg(long)]
    pub adopt: bool,
}

/// How a command ended, as the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// 0: done.
    Done,
    /// 1: failed.
    Failed,
    /// 2: the pass finished, but deletions wait for `sync --approve-deletes`.
    NeedsApproval,
    /// 3: stopped, needs you: a safety stop, or an account-level problem; trying again won't help until
    /// someone looks.
    NeedsYou,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Exit::Done => 0,
            Exit::Failed => 1,
            Exit::NeedsApproval => 2,
            Exit::NeedsYou => 3,
        }
    }
}

impl AgentError {
    /// The exit code for this error: [`Exit::NeedsYou`] for a safety stop and for an account-level problem where
    /// trying again never helps (the device was revoked, `state.json` is another account's, the server speaks
    /// another protocol or chunker, a file is from a newer agent, the server sent damaged data or the saved state
    /// doesn't hold together); [`Exit::Failed`] for anything that may pass by itself (offline, a server error, a
    /// timeout) or that the message fixes (a wrong password, a usage error).
    pub fn exit(&self) -> Exit {
        match self {
            AgentError::Root(_)
            | AgentError::DataInFolder { .. }
            | AgentError::Lock(LockError::Busy(_) | LockError::NotAFile(_))
            | AgentError::Revoked
            | AgentError::OtherAccountState(_)
            | AgentError::Server(TransportError::Protocol(_))
            | AgentError::Sync(SyncError::Corrupt(_))
            | AgentError::Account(AccountError::Newer { .. } | AccountError::Damaged { .. }) => Exit::NeedsYou,
            _ => Exit::Failed,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("{0}")]
    Usage(String),
    #[error("not signed in: run `craft-cloud-agent login <server URL> --user <name>` first")]
    NotSignedIn,
    #[error("wrong username or password")]
    WrongPassword,
    #[error(
        "the server no longer accepts this device (it was signed out or revoked). Your files are untouched. \
         Run `craft-cloud-agent logout`, then `login` again."
    )]
    Revoked,
    #[error("{} belongs to another account; move it away to start this folder over", .0.display())]
    OtherAccountState(PathBuf),
    #[error("can't reach the server ({0}); nothing is lost, run the command again later")]
    Offline(String),
    #[error(
        "can't reach the server to sign this device out ({0}); nothing was changed. Try again later, or run \
         `craft-cloud-agent logout --local` to forget the account here only (the device stays valid on the server \
         until an admin revokes it)"
    )]
    LogoutOffline(String),
    #[error("server: {0}")]
    Server(TransportError),
    #[error("sync failed: {0}")]
    Sync(SyncError),
    #[error(transparent)]
    Account(#[from] AccountError),
    #[error(transparent)]
    Root(#[from] RootError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(
        "the agent's data folder {} and the synced folder {} lie inside one another, so the sign-in token would be \
         synced; nothing was synced. Choose a synced folder outside the data folder (or a --data-dir outside the \
         synced folder)",
        .data.display(),
        .folder.display()
    )]
    DataInFolder { data: PathBuf, folder: PathBuf },
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}

impl From<TransportError> for AgentError {
    fn from(e: TransportError) -> Self {
        match e {
            TransportError::Unauthorized => AgentError::Revoked,
            TransportError::Unavailable(message) => AgentError::Offline(message),
            other => AgentError::Server(other),
        }
    }
}

impl From<SyncError> for AgentError {
    fn from(e: SyncError) -> Self {
        match e {
            SyncError::Transport(t) => t.into(),
            other => AgentError::Sync(other),
        }
    }
}

fn io_at(path: &Path) -> impl FnOnce(std::io::Error) -> AgentError + '_ {
    move |source| AgentError::Io { path: path.to_owned(), source }
}

/// Runs a parsed command line. Errors are returned for the caller to print; the exit code is
/// [`Exit::Failed`] for them.
pub async fn run(cli: Cli, input: &mut impl BufRead, out: &mut impl Write) -> Result<Exit, AgentError> {
    let data = DataDir::resolve(cli.data_dir)?;
    match cli.command {
        Command::Login(args) => {
            // Before reading the password: signed in already means there is nothing to type it for.
            if let Some(account) = data.load_account()? {
                return Err(already_signed_in(&account));
            }
            let password = read_password(input)?;
            login(&data, &args, &password, out).await?;
            Ok(Exit::Done)
        }
        Command::Sync { approve_deletes } => Ok(sync(&data, approve_deletes, out).await?.0),
        Command::Status => {
            status(&data, out)?;
            Ok(Exit::Done)
        }
        Command::Logout { local } => {
            logout(&data, local, out).await?;
            Ok(Exit::Done)
        }
    }
}

/// Reads one line, at most 1100 bytes (the server's 1024-byte password limit plus a line ending).
pub fn read_password(input: &mut impl BufRead) -> Result<String, AgentError> {
    let mut line = String::new();
    input.take(1100).read_line(&mut line).map_err(|e| AgentError::Usage(format!("can't read the password from standard input: {e}")))?;
    let password = line.trim_end_matches(['\r', '\n']).to_owned();
    if password.is_empty() {
        return Err(AgentError::Usage("no password on standard input".into()));
    }
    Ok(password)
}

fn write_out(out: &mut impl Write, text: std::fmt::Arguments<'_>) -> Result<(), AgentError> {
    out.write_fmt(text).map_err(io_at(Path::new("standard output")))
}

macro_rules! say {
    ($out:expr, $($arg:tt)*) => { write_out($out, format_args!("{}\n", format_args!($($arg)*))) };
}

/// The conflict-copy time: the machine's local time, or UTC marked `Z` when the local offset can't be read.
pub fn conflict_stamp() -> ConflictStamp {
    let now = Timestamp::now();
    let (zoned, utc) = match TimeZone::try_system() {
        Ok(tz) => (now.to_zoned(tz), false),
        Err(_) => (now.to_zoned(TimeZone::UTC), true),
    };
    let byte = |v: i8| u8::try_from(v).unwrap_or(0);
    ConflictStamp {
        year: u16::try_from(zoned.year()).unwrap_or(0),
        month: byte(zoned.month()),
        day: byte(zoned.day()),
        hour: byte(zoned.hour()),
        minute: byte(zoned.minute()),
        utc,
    }
}

/// A time for people: local, to the minute.
fn show_time(ms: i64) -> String {
    let Ok(ts) = Timestamp::from_millisecond(ms) else { return format!("{ms} ms") };
    match TimeZone::try_system() {
        Ok(tz) => ts.to_zoned(tz).strftime("%Y-%m-%d %H:%M").to_string(),
        Err(_) => ts.to_zoned(TimeZone::UTC).strftime("%Y-%m-%d %H:%MZ").to_string(),
    }
}

fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

/// This computer's name: `COMPUTERNAME` (Windows), `HOSTNAME`, `/etc/hostname`, else a generic one.
fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| valid_device_name(name))
        .unwrap_or_else(|| if cfg!(target_vendor = "apple") { "Mac".into() } else { "computer".into() })
}

/// The server's rule: 1-128 bytes, no control characters.
fn valid_device_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
}

/// `Craft Cloud` in the home folder.
fn default_folder() -> Result<PathBuf, AgentError> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).filter(|p| p.is_absolute());
    home.map(|h| h.join("Craft Cloud")).ok_or_else(|| AgentError::Usage("can't find your home folder; pass --folder".into()))
}

/// Takes the lock at `path` for as long as the result lives, printing a warning (to standard error, so it never
/// mixes into the output a script reads) if the filesystem can't lock.
fn hold(path: &Path) -> Result<Lock, AgentError> {
    let (held, warning) = lock::acquire(path)?;
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    Ok(held)
}

/// Takes the data folder's lock (creating the data folder).
fn hold_data(data: &DataDir) -> Result<Lock, AgentError> {
    data.create()?;
    hold(&data.path().join(DATA_LOCK))
}

/// Takes the synced folder's lock, in its `.craft-cloud/` folder (created owner-only if missing; a link there is
/// refused, never followed).
fn hold_folder(folder: &Path) -> Result<Lock, AgentError> {
    let dir = folder.join(AGENT_DIR);
    match std::fs::symlink_metadata(&dir) {
        Ok(md) if md.file_type().is_dir() => {}
        Ok(_) => return Err(RootError::Damaged { path: dir, message: "not a real folder (a link?)".into() }.into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_private_dir(&dir).map_err(io_at(&dir))?,
        Err(e) => return Err(io_at(&dir)(e)),
    }
    hold(&dir.join(FOLDER_LOCK))
}

/// Whether `folder` holds anything besides the agent's own folder.
fn has_files(folder: &Path) -> Result<bool, AgentError> {
    for entry in std::fs::read_dir(folder).map_err(io_at(folder))? {
        let entry = entry.map_err(io_at(folder))?;
        if entry.file_name() != AGENT_DIR {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `path` as the OS resolves it, walked one name at a time: every prefix that exists is made canonical (links
/// followed, `..` resolved), a name that doesn't exist yet is appended as it is, `.` is dropped and `..` steps back
/// to the parent by name. That is exactly what the OS does, also for a `..` in the part that doesn't exist yet
/// (`new/../data` once `new` is made) and for one that climbs above the existing part: what came before it is
/// either canonical, so it holds no links, or a folder still to be made. So nothing is refused here; the nesting
/// check sees the folder the OS will use.
fn resolved(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                out.pop();
                continue;
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => out.push(component.as_os_str()),
        }
        if let Ok(real) = std::fs::canonicalize(&out) {
            out = real;
        }
    }
    out
}

/// Whether one of the two paths lies inside the other (or they are the same), as the OS resolves them. Names are
/// compared ignoring case on Windows and macOS, whose filesystems usually do.
fn nested(a: &Path, b: &Path) -> bool {
    let fold = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| {
                let s = c.as_os_str().to_string_lossy();
                if cfg!(any(windows, target_vendor = "apple")) { s.to_lowercase() } else { s.into_owned() }
            })
            .collect()
    };
    let (a, b) = (fold(&resolved(a)), fold(&resolved(b)));
    a.starts_with(&b) || b.starts_with(&a)
}

/// Refuses a synced folder and a data folder that lie inside one another: the data folder holds the device token,
/// which would then be synced (or the synced folder would sync the agent's own state).
fn check_apart(data: &DataDir, folder: &Path) -> Result<(), AgentError> {
    if nested(data.path(), folder) {
        return Err(AgentError::DataInFolder { data: resolved(data.path()), folder: resolved(folder) });
    }
    Ok(())
}

fn already_signed_in(account: &Account) -> AgentError {
    AgentError::Usage(format!(
        "already signed in as {} on {} for {}; run `craft-cloud-agent logout` first",
        account.username,
        account.server,
        account.folder.display()
    ))
}

/// Signs in, creates a device and sets up the folder with its marker. Any failure after the device was
/// created revokes it again (best effort) and leaves no account behind.
pub async fn login(data: &DataDir, args: &LoginArgs, password: &str, out: &mut impl Write) -> Result<Account, AgentError> {
    let _data_lock = hold_data(data)?;
    if let Some(account) = data.load_account()? {
        return Err(already_signed_in(&account));
    }
    let server = ServerUrl::parse(&args.url).map_err(|e| AgentError::Usage(e.to_string()))?;
    let folder = match &args.folder {
        Some(folder) => std::path::absolute(folder).map_err(io_at(folder))?,
        None => default_folder()?,
    };
    check_apart(data, &folder)?;
    if folder.exists() {
        if !folder.is_dir() {
            return Err(AgentError::Usage(format!("{} is not a folder", folder.display())));
        }
        if !args.adopt && has_files(&folder)? {
            return Err(AgentError::Usage(format!(
                "{} already has files in it; pass --adopt to sync them (files that differ from the server's become conflict copies; nothing is deleted)",
                folder.display()
            )));
        }
    }
    let existing_marker = root::read(&folder)?;
    let device_name = args.device_name.clone().unwrap_or_else(default_device_name);
    if !valid_device_name(&device_name) {
        return Err(AgentError::Usage("device names are 1-128 bytes without control characters".into()));
    }

    http::check_compatible(&http::info(&server).await?)?;
    let signed_in = match http::login(&server, &args.user, password, &device_name).await {
        Err(TransportError::Unauthorized) => return Err(AgentError::WrongPassword),
        other => other?,
    };
    let token = DeviceToken::new(signed_in.token);
    let transport = HttpTransport::new(server.clone(), &token)?;
    let setup = async {
        let space = craft_cloud_sync::Transport::space(&transport).await?;
        let marker = RootMarker { space: space.space, root: space.root };
        if existing_marker.is_some_and(|m| m != marker) {
            return Err(RootError::SetUpForOtherSpace(folder.clone()).into());
        }
        std::fs::create_dir_all(&folder).map_err(io_at(&folder))?;
        let _folder_lock = hold_folder(&folder)?;
        root::write(&folder, &marker)?;
        let account = Account {
            schema: ACCOUNT_SCHEMA,
            server: server.clone(),
            username: space.username,
            device: signed_in.device,
            device_name: device_name.clone(),
            folder: folder.clone(),
            space: marker.space,
            root: marker.root,
            token,
        };
        data.remove_state()?;
        data.save_account(&account)?;
        Ok::<_, AgentError>(account)
    };
    let account = match setup.await {
        Ok(account) => account,
        Err(e) => {
            let _ = transport.revoke(signed_in.device).await;
            return Err(e);
        }
    };
    say!(out, "signed in as {} on {} (device \"{}\")", account.username, account.server, account.device_name)?;
    say!(out, "syncing {}; run `craft-cloud-agent sync` to sync it", account.folder.display())?;
    Ok(account)
}

/// Runs one sync pass and saves the state, whether the pass finished or not. Returns the exit code and what
/// the pass did.
pub async fn sync(data: &DataDir, approve_deletes: bool, out: &mut impl Write) -> Result<(Exit, PassSummary), AgentError> {
    let _data_lock = hold_data(data)?;
    // Temporary files a crash left while `account.json` or `state.json` was written.
    data.clean_tmp()?;
    let account = data.load_account()?.ok_or(AgentError::NotSignedIn)?;
    check_apart(data, &account.folder)?;
    root::check(&account.folder, &RootMarker { space: account.space, root: account.root })?;
    let _folder_lock = hold_folder(&account.folder)?;
    let mut saved = data.load_state()?;
    if saved.state.space.is_some_and(|s| s != account.space) {
        return Err(AgentError::OtherAccountState(data.path().join(crate::account::STATE_FILE)));
    }
    let fs = DiskFs::new(&account.folder).with_device_name(account.device_name.clone());
    fs.clean_tmp().map_err(io_at(&fs.tmp_dir()))?;
    let transport = HttpTransport::new(account.server.clone(), &account.token)?;
    let config = Config::new(account.device_name.clone(), account.device);
    let mut engine = Engine::new(fs, transport, std::mem::take(&mut saved.state), config, conflict_stamp)?;
    // `state.json` is also written at the pass's checkpoints, so a crash or a kill loses at most what the pass did
    // since the last one (the last pass's outcome stays as it was until this one ends).
    let (checkpoint_data, last_pass) = (data.clone(), saved.last_pass.clone());
    engine.on_checkpoint(move |state| {
        let saved = SavedState { schema: STATE_SCHEMA, state: state.clone(), last_pass: last_pass.clone() };
        checkpoint_data.save_state(&saved).map_err(|e| std::io::Error::other(e.to_string()))
    });
    let previous = saved.last_pass.as_ref().and_then(|l| l.summary.clone());
    if approve_deletes {
        // Exactly the deletions the user saw listed; new ones wait and are listed again.
        let shown = previous.as_ref().map(|s| s.pending_nodes.clone()).unwrap_or_default();
        if shown.is_empty() {
            say!(out, "nothing was waiting for approval")?;
        }
        engine.approve_deletes_of(shown);
    }
    // The server is asked what it speaks before every pass (it may have been updated since the last one); a
    // mismatch fails the pass like any other server answer, so it is saved and shown by `status`.
    let compatible = match http::info(&account.server).await {
        Ok(info) => http::check_compatible(&info),
        Err(e) => Err(e),
    };
    let result = match compatible {
        Ok(()) => engine.sync_once().await,
        Err(e) => Err(SyncError::Transport(e)),
    };
    saved.schema = STATE_SCHEMA;
    saved.state = engine.state().clone();
    let (outcome, summary) = match &result {
        Ok(report) => {
            let summary = PassSummary::from_report(report);
            (if summary.needs_approval() { Outcome::NeedsApproval } else { Outcome::Done }, Some(summary))
        }
        // A failed pass keeps the deletions the pass before it listed: the user was shown them, and a retry
        // after a network blip must still release exactly those.
        Err(e) => (Outcome::Failed(failure_message(e)), previous.map(|s| s.pending_only()).filter(PassSummary::needs_approval)),
    };
    saved.last_pass = Some(LastPass { finished_ms: now_ms(), outcome, summary: summary.clone() });
    data.save_state(&saved)?;
    result?;
    let summary = summary.unwrap_or_default();
    print_summary(out, &summary)?;
    Ok((if summary.needs_approval() { Exit::NeedsApproval } else { Exit::Done }, summary))
}

/// The message a failed pass is remembered with: what [`AgentError`] would say.
fn failure_message(e: &SyncError) -> String {
    match e {
        SyncError::Transport(TransportError::Unauthorized) => AgentError::Revoked.to_string(),
        SyncError::Transport(TransportError::Unavailable(m)) => AgentError::Offline(m.clone()).to_string(),
        SyncError::Transport(other) => AgentError::Server(other.clone()).to_string(),
        other => format!("sync failed: {other}"),
    }
}

fn print_summary(out: &mut impl Write, s: &PassSummary) -> Result<(), AgentError> {
    let counts = [
        (s.downloaded, "downloaded"),
        (s.uploaded, "uploaded"),
        (s.removed_locally, "removed here"),
        (s.removed_remotely, "removed on the server"),
        (s.moved_locally, "moved here"),
        (s.moved_remotely, "moved on the server"),
        (s.created_dirs, "folders created"),
        (s.conflict_copies.len(), "conflict copies"),
        (s.refused, "refused by the server (retried next pass)"),
    ];
    let done: Vec<String> = counts.iter().filter(|(n, _)| *n > 0).map(|(n, what)| format!("{n} {what}")).collect();
    if done.is_empty() {
        say!(out, "in sync: nothing to do")?;
    } else {
        say!(out, "synced: {}", done.join(", "))?;
    }
    if !s.conflict_copies.is_empty() {
        say!(out, "conflict copies (both versions kept):")?;
        for path in &s.conflict_copies {
            say!(out, "  {path}")?;
        }
    }
    if !s.skipped.is_empty() {
        say!(out, "skipped:")?;
        for (path, why) in &s.skipped {
            say!(out, "  {path}: {why}")?;
        }
    }
    print_waiting(out, s)
}

/// The deletions waiting for approval, if any.
fn print_waiting(out: &mut impl Write, s: &PassSummary) -> Result<(), AgentError> {
    if s.needs_approval() {
        say!(out, "waiting for approval (check the list, then run `craft-cloud-agent sync --approve-deletes`):")?;
        for path in &s.pending_deletes {
            say!(out, "  delete on the server: {path}")?;
        }
        for path in &s.pending_local_deletes {
            say!(out, "  delete here: {path}")?;
        }
    }
    Ok(())
}

/// Prints the account, the folder and the last pass. Never prints the token.
pub fn status(data: &DataDir, out: &mut impl Write) -> Result<(), AgentError> {
    let Some(account) = data.load_account()? else {
        say!(out, "not signed in")?;
        return Ok(());
    };
    say!(out, "signed in as {} on {}", account.username, account.server)?;
    say!(out, "device: {} ({})", account.device_name, account.device)?;
    say!(out, "folder: {}", account.folder.display())?;
    if let Err(e) = root::check(&account.folder, &RootMarker { space: account.space, root: account.root }) {
        say!(out, "folder problem: {e}")?;
    }
    // What recovery of interrupted downloads left for the user, until it is gone.
    if let Ok(left) = DiskFs::new(&account.folder).needs_you()
        && !left.is_empty()
    {
        say!(out, "needs you:")?;
        for (path, what) in left {
            say!(out, "  {path}: {what}")?;
        }
    }
    let saved = data.load_state()?;
    match &saved.last_pass {
        None => say!(out, "last pass: never synced")?,
        Some(last) => {
            let how = match &last.outcome {
                Outcome::Done => "done".to_owned(),
                Outcome::NeedsApproval => "done, deletions wait for approval".to_owned(),
                Outcome::Failed(message) => format!("failed: {message}"),
            };
            say!(out, "last pass: {} ({how})", show_time(last.finished_ms))?;
            if let Some(summary) = &last.summary {
                // After a failed pass the summary is only the kept list of waiting deletions.
                if matches!(last.outcome, Outcome::Failed(_)) {
                    print_waiting(out, summary)?;
                } else {
                    print_summary(out, summary)?;
                }
            }
        }
    }
    Ok(())
}

/// Revokes this device on the server and forgets the account and state here. The folder and its files stay.
/// A device the server already revoked is simply forgotten.
///
/// With `local`, the server isn't asked: the account and state are forgotten here only, and the device stays
/// valid on the server until an admin revokes it (`craft-cloud-server device revoke`).
pub async fn logout(data: &DataDir, local: bool, out: &mut impl Write) -> Result<(), AgentError> {
    let _data_lock = hold_data(data)?;
    let account = data.load_account()?.ok_or(AgentError::NotSignedIn)?;
    if !local {
        let transport = HttpTransport::new(account.server.clone(), &account.token)?;
        match transport.revoke(account.device).await {
            Ok(()) | Err(TransportError::Unauthorized | TransportError::NotFound) => {}
            Err(TransportError::Unavailable(message)) => return Err(AgentError::LogoutOffline(message)),
            Err(e) => return Err(e.into()),
        }
    }
    data.remove_account()?;
    data.remove_state()?;
    if local {
        say!(
            out,
            "forgot the account here; device \"{}\" ({}) stays valid on {} until an admin revokes it",
            account.device_name,
            account.device,
            account.server
        )?;
    } else {
        say!(out, "signed out of {}", account.server)?;
    }
    say!(out, "the files in {} stay", account.folder.display())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_parses() {
        let cli = Cli::try_parse_from(["craft-cloud-agent", "login", "https://c.example", "--user", "ana", "--adopt", "--data-dir", "d"]).unwrap();
        assert_eq!(cli.data_dir.as_deref(), Some(Path::new("d")));
        assert!(matches!(cli.command, Command::Login(LoginArgs { adopt: true, ref user, .. }) if user == "ana"));
        let cli = Cli::try_parse_from(["craft-cloud-agent", "sync", "--approve-deletes"]).unwrap();
        assert!(matches!(cli.command, Command::Sync { approve_deletes: true }));
        assert!(Cli::try_parse_from(["craft-cloud-agent", "login", "https://c.example"]).is_err(), "--user is required");
    }

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(Exit::Done.code(), 0);
        assert_eq!(Exit::Failed.code(), 1);
        assert_eq!(Exit::NeedsApproval.code(), 2);
        assert_eq!(Exit::NeedsYou.code(), 3);
        assert_eq!(AgentError::Root(RootError::MarkerMissing(PathBuf::from("f"))).exit(), Exit::NeedsYou);
        assert_eq!(AgentError::Lock(LockError::Busy(PathBuf::from("l"))).exit(), Exit::NeedsYou);
        assert_eq!(AgentError::Revoked.exit(), Exit::NeedsYou);
        assert_eq!(AgentError::OtherAccountState(PathBuf::from("s")).exit(), Exit::NeedsYou);
        assert_eq!(AgentError::Server(TransportError::Protocol("x".into())).exit(), Exit::NeedsYou);
        let newer = AccountError::Newer { path: PathBuf::from("s"), found: 2, supported: 1 };
        assert_eq!(AgentError::Account(newer).exit(), Exit::NeedsYou);
        let damaged = AccountError::Damaged { path: PathBuf::from("s"), message: "x".into() };
        assert_eq!(AgentError::Account(damaged).exit(), Exit::NeedsYou);
        assert_eq!(AgentError::from(SyncError::Corrupt("chunk x of a.txt is damaged".into())).exit(), Exit::NeedsYou);
        let io = SyncError::Io { path: "a.txt".into(), source: std::io::Error::other("x") };
        assert_eq!(AgentError::from(io).exit(), Exit::Failed);
        assert_eq!(AgentError::Server(TransportError::NotFound).exit(), Exit::Failed);
        assert_eq!(AgentError::WrongPassword.exit(), Exit::Failed);
        assert_eq!(AgentError::NotSignedIn.exit(), Exit::Failed);
        assert_eq!(AgentError::Offline("x".into()).exit(), Exit::Failed);
    }

    #[test]
    fn nesting_is_seen_through_links_and_missing_folders() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        assert!(nested(&data, dir.path()));
        assert!(nested(dir.path(), &data.join("not/made/yet")));
        assert!(nested(&data, &data));
        assert!(!nested(&data, &dir.path().join("Craft Cloud")));
        assert!(!nested(&data, &dir.path().join("data2")), "a shared name prefix isn't nesting");
        // `..` in the part that doesn't exist yet steps back out of a folder still to be made, as the OS will.
        assert!(nested(&data, &dir.path().join("new/../data/sub")));
        assert!(nested(&data, &dir.path().join("new/./more/../../data")));
        let top = dir.path().file_name().unwrap();
        assert!(nested(&data, &dir.path().join("new/../..").join(top).join("data")), "climbing above the existing part");
        assert!(!nested(&data, &dir.path().join("new/../elsewhere")));
        assert!(!nested(&data, &dir.path().join("data/../elsewhere/sub")));
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(dir.path(), &link).unwrap();
            assert!(nested(&data, &link), "a link to the data folder's parent");
            assert!(nested(&link.join("data").join("sub"), &data));
            assert!(nested(&link.join("new/../data/sub"), &data), "a link before a `..` in the missing part");
        }
    }

    #[test]
    fn passwords_come_from_one_line() {
        assert_eq!(read_password(&mut "hunter22\r\nrest".as_bytes()).unwrap(), "hunter22");
        assert!(read_password(&mut "".as_bytes()).is_err());
        assert!(read_password(&mut "\n".as_bytes()).is_err());
    }

    #[test]
    fn conflict_stamps_are_sane() {
        let stamp = conflict_stamp();
        assert!(stamp.year >= 2026 && (1..=12).contains(&stamp.month) && (1..=31).contains(&stamp.day));
        assert!(stamp.hour < 24 && stamp.minute < 60);
    }

    #[test]
    fn device_names_follow_the_server_rule() {
        assert!(valid_device_name("Ana's laptop"));
        assert!(!valid_device_name(""));
        assert!(!valid_device_name("a\nb"));
        assert!(!valid_device_name(&"x".repeat(129)));
        assert!(valid_device_name(&default_device_name()));
    }

    #[test]
    fn revoked_and_offline_errors_say_what_to_do() {
        let revoked = AgentError::from(SyncError::Transport(TransportError::Unauthorized)).to_string();
        assert!(revoked.contains("logout") && revoked.contains("login"), "{revoked}");
        let offline = AgentError::from(TransportError::Unavailable("connection refused".into())).to_string();
        assert!(offline.contains("connection refused") && offline.contains("again later"), "{offline}");
    }
}
