//! The `craft-cloud-agent` command line: `login` or `join`, `run`, `sync`, `status`, `logout`, and through the running
//! agent `recents`, `doc` ([`crate::doc`]), `add-computer`, `devices`, `pause`, `resume`, `quit` and `autostart`. Each command is a library
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
//! `run` handles these itself while it runs (it retries, waits for approval or pauses; see [`crate::daemon`]) and
//! exits 0 when stopped, 1 when stopped twice (at once) and 3 when it can't start (another agent is running).
//!
//! A crash or kill during `login`, after the server created the device and before the account was saved, can
//! leave a device registered that nothing here uses; an admin can revoke it (`craft-cloud-server device ...`).

use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};

use clap::{Parser, Subcommand};
use craft_cloud_proto::{ConflictStamp, NodeId};
use craft_cloud_sync::{Config, Engine, SyncError, TransportError};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::account::{ACCOUNT_SCHEMA, Account, AccountError, DataDir, LastPass, LoopState, Outcome, PassSummary, STATE_SCHEMA, SavedState};
use crate::disk::{AGENT_DIR, DiskFs, create_private_dir};
use crate::http::{self, DeviceToken, HttpTransport, JoinError, ReachError, ServerUrl};
use crate::lock::{self, DATA_LOCK, FOLDER_LOCK, Lock, LockError};
use crate::root::{self, RootError, RootMarker};
use crate::settle::Stability;
use craft_cloud_proto::api::LoginResponse;
use craft_cloud_proto::join::JoinString;

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
        /// still wait). With `=<set>` (the set the list was printed with): only if that set is the one waiting.
        #[arg(long, value_name = "SET", num_args = 0..=1, require_equals = true, value_parser = set_name)]
        approve_deletes: Option<Option<String>>,
    },
    /// Keep the folder in sync until stopped (Ctrl-C): sync whenever something changes here or on the server, and
    /// at least every 10 minutes. Ctrl-C lets a running pass finish; a second Ctrl-C stops at once. Exits 0 at once,
    /// with a note, when another agent runs for the same data folder.
    Run {
        /// Write what it says to `run.log` in the data folder (rotated at 10 MiB), not to the console: what the
        /// start-at-login entries run.
        #[arg(long)]
        log: bool,
    },
    /// Show the account, the folder, whether `run` is running and what it does, and the last pass.
    Status,
    /// Sign this device out on the server and forget the account here. The files stay.
    Logout {
        /// Forget the account here without reaching the server (it is gone, or offline for good). The device
        /// stays valid on the server until an admin revokes it.
        #[arg(long)]
        local: bool,
    },
    /// Show an app's recent files (shared by every machine of the account), or add one, through the running agent.
    Recents(RecentsArgs),
    /// Open a file of the synced folder as a live plain-text document, through the running agent, like an app: print
    /// its text, append to it, or print it again after every change. A test tool: it never writes the file.
    Doc(DocArgs),
    /// Join a Craft Cloud with a join string: an invite from the server's admin, or a code from a signed-in computer
    /// (`add-computer` there). An invite for a new account asks for its password, twice, on standard input.
    Join(JoinArgs),
    /// Add another computer to this account: prints a join string to enter on it, and waits until it joined (the code
    /// works once, for 10 minutes; Ctrl-C cancels it). Through the running agent.
    AddComputer,
    /// Pause syncing until `resume`, also across restarts (apps' recents keep working). Through the running agent.
    Pause,
    /// Sync again after `pause`. Through the running agent.
    Resume,
    /// Stop the running agent after the pass under way. It starts again at the next login if autostart is on.
    Quit,
    /// List the account's computers, or remove one. Through the running agent.
    Devices {
        #[command(subcommand)]
        action: Option<DevicesAction>,
    },
    /// Start the agent by itself at login on this computer (`on`), or not any more (`off`). Signing out turns it off.
    Autostart {
        #[arg(value_enum)]
        state: OnOff,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DevicesAction {
    /// Remove a computer from the account (its ID from `devices`): it can't sync any more; its files stay. To remove
    /// this computer, sign out instead.
    Remove { id: craft_cloud_proto::DeviceId },
}

#[derive(Debug, Clone, clap::Args)]
pub struct RecentsArgs {
    /// The app, e.g. wordcraft.
    pub app: String,
    /// Keep showing the list, again after every change, until stopped (Ctrl-C).
    #[arg(long)]
    pub watch: bool,
    #[command(subcommand)]
    pub action: Option<RecentsAction>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct DocArgs {
    /// The file: its path in the synced folder (absolute, or relative to the folder). Plain text (UTF-8).
    pub path: PathBuf,
    /// Append this text to the document (on every machine that has it open).
    #[arg(long)]
    pub append: Option<String>,
    /// Keep printing the text, again after every change, until stopped (Ctrl-C).
    #[arg(long)]
    pub watch: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum RecentsAction {
    /// Add a file: its path in the synced folder (absolute, or relative to the folder).
    Add {
        path: PathBuf,
        /// Shown in the list instead of the file's name.
        #[arg(long)]
        title: Option<String>,
        /// It was saved (rather than only opened).
        #[arg(long)]
        saved: bool,
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

#[derive(Debug, Clone, clap::Args)]
pub struct JoinArgs {
    /// The join string: from the server's admin (an invite) or from a signed-in computer (add a computer), e.g.
    /// cloud.example.com/K7M4-Q9TX or the link https://cloud.example.com/join/K7M4-Q9TX.
    pub join: String,
    /// The folder to sync. Default: "Craft Cloud" in your home folder.
    #[arg(long)]
    pub folder: Option<PathBuf>,
    /// This computer's name in the device list and in conflict copy names. Default: the computer's name.
    #[arg(long, alias = "name")]
    pub device_name: Option<String>,
    /// Sync a folder that already has files in it. Files that differ from the server's become conflict
    /// copies; nothing is deleted.
    #[arg(long)]
    pub adopt: bool,
    /// Once joined, start the agent by itself at every login on this computer (as `autostart on`).
    #[arg(long)]
    pub autostart: bool,
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
    /// another protocol or chunker, the account's storage on the server is full, a file is from a newer agent, the
    /// server sent damaged data or the saved state doesn't hold together); [`Exit::Failed`] for anything that may
    /// pass by itself (offline, a server error, a timeout) or that the message fixes (a wrong password, a usage
    /// error).
    pub fn exit(&self) -> Exit {
        match self {
            AgentError::Root(_)
            | AgentError::DataInFolder { .. }
            | AgentError::Lock(LockError::Busy(_) | LockError::NotAFile(_))
            | AgentError::Revoked
            | AgentError::OtherAccountState(_)
            | AgentError::Server(TransportError::Protocol(_) | TransportError::QuotaExceeded { .. })
            | AgentError::Sync(SyncError::Corrupt(_))
            | AgentError::Account(AccountError::Newer { .. } | AccountError::Damaged { .. })
            | AgentError::NeedsYou(_) => Exit::NeedsYou,
            AgentError::JoinSetup(inner) => inner.exit(),
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
    /// A call to the running agent's local API failed.
    #[error("{0}")]
    Local(craft_cloud_app::Error),
    /// `join`: the code is wrong, expired or used; the server never says which.
    #[error("This code doesn't work any more. Make a new one on a signed-in computer.")]
    InvalidCode,
    /// `join`: an invite to a new account, without a password for it.
    #[error("this invite is for a new account: choose a password for it")]
    PasswordRequired,
    /// `join`: the new password breaks the server's rules (its message).
    #[error("{0}")]
    WeakPassword(String),
    /// `join`: too many codes that didn't work from this address (the server's message).
    #[error("{0}")]
    TooManyTries(String),
    #[error(
        "This computer doesn't trust {host}'s certificate. If the server has a certificate authority of its own \
         (step-ca, say), install that authority's root certificate on this computer, then try again."
    )]
    UntrustedCertificate { host: String },
    /// `join`: the server can't be reached; nothing was created.
    #[error("can't reach {server} ({message}); nothing was created. Check the address and that this computer is online, then try again")]
    Unreachable { server: String, message: String },
    /// `join`: the code was spent, but setting up this computer failed afterwards; the new device was removed again.
    #[error("{0}. This code is used up now: make a new one on a signed-in computer, then join again")]
    JoinSetup(Box<AgentError>),
    /// Removing the computer the request comes from.
    #[error("this is the computer you are using: to take it out of your cloud, sign out instead")]
    CannotRemoveSelf,
    /// `login` or `join` without `--adopt` for a folder with files in it; found before anything reaches the server.
    #[error(
        "{} already has files in it; pass --adopt to sync them (files that differ from the server's become conflict copies; nothing is deleted)",
        .0.display()
    )]
    FolderHasFiles(PathBuf),
    /// Something found as `run` started that only a person can put right (the message says what); the agent waits,
    /// serving the local API, until it is.
    #[error("{0}")]
    NeedsYou(String),
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

fn write_out(out: &mut impl Write, text: std::fmt::Arguments<'_>) -> Result<(), AgentError> {
    out.write_fmt(text).map_err(io_at(Path::new("standard output")))
}

macro_rules! say {
    ($out:expr, $($arg:tt)*) => { write_out($out, format_args!("{}\n", format_args!($($arg)*))) };
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
            match login(&data, &args, &password, out).await {
                // `run` holds the data folder: it signs in, through its local API.
                Err(AgentError::Lock(LockError::Busy(path))) => login_through_agent(&data, &args, password, out).await.map_err(|e| match e {
                    AgentError::Local(craft_cloud_app::Error::NotRunning(_)) => AgentError::Lock(LockError::Busy(path)),
                    other => other,
                })?,
                other => {
                    other?;
                }
            }
            Ok(Exit::Done)
        }
        Command::Sync { approve_deletes } => Ok(sync(&data, Approve::from_flag(approve_deletes), out).await?.0),
        Command::Run { .. } => {
            let (stop, stops) = tokio::sync::mpsc::unbounded_channel();
            let signals = tokio::spawn(forward_stop_signals(stop));
            let exit = crate::daemon::run_loop(&data, crate::daemon::RunOptions::default(), stops, out).await;
            signals.abort();
            ran(exit, out)
        }
        Command::Status => {
            status(&data, out)?;
            Ok(Exit::Done)
        }
        Command::Logout { local } => {
            match logout(&data, local, out).await {
                // Signed out here: the agent no longer starts at login (a running agent does this itself).
                Ok(()) => {
                    // Only the agent with the default data folder has the entry; another leaves it alone.
                    let off = crate::autostart::Roots::for_user()
                        .and_then(|roots| if roots.owns(data.path()) { crate::autostart::unregister(&roots) } else { Ok(()) });
                    if let Err(e) = off {
                        say!(out, "the start-at-login entry couldn't be removed ({e}); `craft-cloud-agent autostart off` tries again")?;
                    }
                }
                Err(AgentError::Lock(LockError::Busy(path))) => {
                    let data_path = data.path().to_owned();
                    let status = blocking(move || craft_cloud_app::Client::connect_in(&data_path)?.logout(local)).await.map_err(|e| match e {
                        AgentError::Local(craft_cloud_app::Error::NotRunning(_)) => AgentError::Lock(LockError::Busy(path)),
                        other => local_error(other),
                    })?;
                    if !status.signed_in {
                        say!(out, "signed out (through the running agent); the files stay")?;
                    }
                }
                other => other?,
            }
            Ok(Exit::Done)
        }
        Command::Recents(args) => recents(&data, args, out).await,
        Command::Doc(args) => crate::doc::doc(&data, args, out).await,
        Command::Join(args) => {
            if let Some(account) = data.load_account()? {
                return Err(already_signed_in(&account));
            }
            let mut password: Option<String> = None;
            loop {
                let joined = match join(&data, &args, password.as_deref(), out).await {
                    // `run` holds the data folder: it joins, through its local API.
                    Err(AgentError::Lock(LockError::Busy(path))) => {
                        join_through_agent(&data, &args, password.clone(), out).await.map_err(|e| match e {
                            AgentError::Local(craft_cloud_app::Error::NotRunning(_)) => AgentError::Lock(LockError::Busy(path)),
                            other => other,
                        })
                    }
                    other => other.map(|_| ()),
                };
                match joined {
                    Err(AgentError::PasswordRequired) if password.is_none() => password = Some(new_password(input, out)?),
                    other => break other?,
                }
            }
            // The join stands whatever happens here; a failure is said.
            // An agent with a data folder of its own has no start-at-login entry: `--autostart` is passed over then.
            let ours = crate::autostart::Roots::for_user().is_ok_and(|roots| roots.owns(data.path()));
            if args.autostart
                && ours
                && let Err(e) = set_autostart(&data, true, out).await
            {
                say!(out, "joined, but this computer won't start syncing by itself after a restart: {e}")?;
                say!(out, "run `craft-cloud-agent autostart on` to try again")?;
            }
            Ok(Exit::Done)
        }
        Command::Autostart { state } => {
            set_autostart(&data, state == OnOff::On, out).await?;
            Ok(Exit::Done)
        }
        Command::AddComputer => {
            let stop = async {
                if tokio::signal::ctrl_c().await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            add_computer(&data, PAIRING_POLL, stop, out).await
        }
        Command::Pause => {
            let client = running_agent(&data).await?;
            blocking(move || client.pause()).await.map_err(local_error)?;
            say!(out, "paused: nothing syncs until `craft-cloud-agent resume` (recents still work on this computer)")?;
            Ok(Exit::Done)
        }
        Command::Resume => {
            let client = running_agent(&data).await?;
            blocking(move || client.resume()).await.map_err(local_error)?;
            say!(out, "syncing again")?;
            Ok(Exit::Done)
        }
        Command::Quit => {
            let client = running_agent(&data).await?;
            blocking(move || client.quit()).await.map_err(local_error)?;
            say!(out, "the agent stops after the pass under way")?;
            Ok(Exit::Done)
        }
        Command::Devices { action } => {
            let client = running_agent(&data).await?;
            let list = match action {
                None => blocking(move || client.devices()).await.map_err(local_error)?,
                Some(DevicesAction::Remove { id }) => {
                    let list = blocking(move || client.remove_device(id)).await.map_err(local_error)?;
                    say!(out, "removed {id}; it can't sync any more (its files stay there)")?;
                    list
                }
            };
            print_devices(out, &list)?;
            Ok(Exit::Done)
        }
    }
}

/// `autostart on|off`: through the running agent if one runs (it handles its own entry), else here, for this agent.
async fn set_autostart(data: &DataDir, on: bool, out: &mut impl Write) -> Result<(), AgentError> {
    let data_path = data.path().to_owned();
    let running = blocking(move || craft_cloud_app::Client::connect_in(&data_path)).await;
    match running {
        Ok(client) => {
            blocking(move || client.set_autostart(on)).await.map_err(local_error)?;
        }
        Err(AgentError::Local(craft_cloud_app::Error::NotRunning(_))) => {
            let roots = crate::autostart::Roots::for_user().map_err(autostart_error)?;
            if !roots.owns(data.path()) {
                return Err(AgentError::Usage(crate::autostart::not_ours(data.path())));
            }
            let data = data.path().to_owned();
            tokio::task::spawn_blocking(move || if on { crate::local_api::set_on(&roots, &data) } else { crate::autostart::unregister(&roots) })
                .await
                .map_err(|e| AgentError::Usage(format!("start at login: {e}")))?
                .map_err(autostart_error)?;
        }
        Err(e) => return Err(e),
    }
    if on {
        say!(out, "this computer starts syncing by itself at every login")
    } else {
        say!(out, "this computer no longer starts syncing by itself at login")
    }
}

fn autostart_error(e: crate::autostart::AutostartError) -> AgentError {
    AgentError::Usage(format!("start at login: {e}"))
}

/// How often `add-computer` asks whether the code was used.
const PAIRING_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// The running agent's local API, or a usage error saying to start it.
async fn running_agent(data: &DataDir) -> Result<craft_cloud_app::Client, AgentError> {
    let data_path = data.path().to_owned();
    blocking(move || craft_cloud_app::Client::connect_in(&data_path)).await.map_err(|e| match e {
        AgentError::Local(craft_cloud_app::Error::NotRunning(why)) => {
            AgentError::Usage(format!("the agent isn't running ({why}); start `craft-cloud-agent run` first"))
        }
        other => other,
    })
}

/// A new account's password for an invite: typed twice (one line each, on standard input), the two the same.
fn new_password(input: &mut impl BufRead, out: &mut impl Write) -> Result<String, AgentError> {
    say!(out, "This invite is for a new account. Choose its password (8 characters or more), then type it again:")?;
    out.flush().map_err(io_at(Path::new("standard output")))?;
    let first = read_password(input)?;
    let again = read_password(input)?;
    if first != again {
        return Err(AgentError::Usage("the two passwords differ; nothing was changed, run `join` again".into()));
    }
    Ok(first)
}

/// `join` while `run` runs: the running agent joins.
async fn join_through_agent(data: &DataDir, args: &JoinArgs, password: Option<String>, out: &mut impl Write) -> Result<(), AgentError> {
    let folder = args.folder.as_ref().map(|f| std::path::absolute(f).map_err(io_at(f))).transpose()?;
    let request = craft_cloud_app::JoinRequest {
        join: args.join.clone(),
        folder: folder.map(|f| f.display().to_string()),
        device_name: args.device_name.clone(),
        adopt: args.adopt,
        password,
    };
    let data_path = data.path().to_owned();
    let status = blocking(move || craft_cloud_app::Client::connect_in(&data_path)?.join(&request)).await.map_err(local_error)?;
    say!(
        out,
        "joined as {} on {} (through the running agent, which syncs {} now)",
        status.account.unwrap_or_default(),
        status.server.unwrap_or_default(),
        status.folder.unwrap_or_default()
    )
}

/// `add-computer`: a new add-a-computer code through the running agent, printed as a join string, then asked about
/// every `every` until it is used or expires. `stop` (Ctrl-C) cancels it.
pub async fn add_computer(
    data: &DataDir,
    every: std::time::Duration,
    stop: impl std::future::Future<Output = ()>,
    out: &mut impl Write,
) -> Result<Exit, AgentError> {
    use craft_cloud_app::PairingState;
    let client = running_agent(data).await?;
    let c = client.clone();
    let pairing = blocking(move || c.pairing()).await.map_err(local_error)?;
    say!(out, "On the other computer, open Craft Cloud and enter: {}", pairing.join)?;
    say!(out, "(or open {} there). The code works once, for 10 minutes; Ctrl-C cancels it.", pairing.link)?;
    out.flush().map_err(io_at(Path::new("standard output")))?;
    let id = pairing.id;
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => {
                let c = client.clone();
                let cancelled = blocking(move || c.cancel_pairing(id)).await;
                say!(out, "{}", if cancelled.is_ok() { "cancelled: the code doesn't work any more" } else { "stopped waiting (the code works until it expires)" })?;
                return Ok(Exit::Failed);
            }
            () = tokio::time::sleep(every) => {}
        }
        let c = client.clone();
        match blocking(move || c.pairing_state(id)).await {
            Ok(status) if status.state == PairingState::Used => {
                say!(out, "{} joined", status.device_name.as_deref().unwrap_or("a computer"))?;
                return Ok(Exit::Done);
            }
            Ok(status) if status.state == PairingState::Expired => break,
            Ok(_) => {}
            Err(AgentError::Local(craft_cloud_app::Error::Refused { code, .. })) if code == craft_cloud_proto::api::codes::NOT_FOUND => break,
            Err(e) => return Err(local_error(e)),
        }
    }
    say!(out, "The code expired")?;
    Ok(Exit::Failed)
}

fn print_devices(out: &mut impl Write, devices: &[craft_cloud_app::DeviceInfo]) -> Result<(), AgentError> {
    say!(out, "computers of this account:")?;
    for d in devices {
        let note = match (d.current, d.revoked) {
            (true, _) => "  (this computer)",
            (false, true) => "  (removed)",
            (false, false) => "",
        };
        say!(out, "  {}  {}  last seen {}{note}", d.id, d.name, show_time(d.last_used_ms))?;
    }
    Ok(())
}

/// How `run` ended, as the process exits: another agent holding the data folder is no failure (a start at login may
/// race the app starting one), so it is said in one line and ends with 0, and no service manager restarts it.
pub fn ran(result: Result<Exit, AgentError>, out: &mut impl Write) -> Result<Exit, AgentError> {
    match result {
        Err(AgentError::Lock(LockError::Busy(path))) => {
            say!(out, "another craft-cloud-agent is already running for this data folder ({} is locked); this one exits", path.display())?;
            Ok(Exit::Done)
        }
        other => other,
    }
}

/// The log the command line `args` writes to: `run.log` in its data folder (opened and installed for
/// [`crate::runlog::note`]) for `run --log`, or for every command when `always` (the background build, which has no
/// console). `None` otherwise, or when it can't be opened: the output then goes where it would.
pub fn log_for(args: &[std::ffi::OsString], always: bool) -> Option<crate::runlog::RunLog> {
    let cli = Cli::try_parse_from(args).ok();
    let wanted = always || cli.as_ref().is_some_and(|cli| matches!(cli.command, Command::Run { log: true }));
    if !wanted {
        return None;
    }
    let data = DataDir::resolve(cli.and_then(|cli| cli.data_dir)).ok()?;
    crate::runlog::open_in(&data)
}

/// The agent's `main`, for both builds (the console one and the background one): parses `args` (the program's name
/// first), runs the command, and writes an error to `err`. Returns the exit code, and whether the command was `run`
/// (after a second stop the caller ends the process at once rather than wait for a pass to finish).
pub async fn main(args: Vec<std::ffi::OsString>, input: &mut impl BufRead, out: &mut impl Write, err: &mut impl Write) -> (Exit, bool) {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            // A usage error is a plain failure (1), never the "deletions wait" code 2 that clap's own exit uses;
            // `--help` and `--version` are not errors.
            let failed = e.use_stderr();
            let text = e.render().to_string();
            let _ = if failed { write!(err, "{text}") } else { write!(out, "{text}") };
            return (if failed { Exit::Failed } else { Exit::Done }, false);
        }
    };
    let looping = matches!(cli.command, Command::Run { .. });
    let exit = match run(cli, input, out).await {
        Ok(exit) => exit,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            e.exit()
        }
    };
    let _ = out.flush();
    let _ = err.flush();
    (exit, looping)
}

/// Runs a blocking call of `craft-cloud-app` off the runtime.
pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, craft_cloud_app::Error> + Send + 'static) -> Result<T, AgentError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AgentError::Usage(format!("the call to the running agent failed: {e}")))?
        .map_err(AgentError::Local)
}

/// The local API's refusals as the errors the CLI has for them.
pub(crate) fn local_error(e: AgentError) -> AgentError {
    use craft_cloud_proto::local::codes;
    match e {
        AgentError::Local(craft_cloud_app::Error::Refused { code, message }) => match code.as_str() {
            codes::WRONG_PASSWORD => AgentError::WrongPassword,
            codes::NOT_SIGNED_IN => AgentError::NotSignedIn,
            codes::OFFLINE => AgentError::Offline(message),
            codes::INVALID_CODE => AgentError::InvalidCode,
            codes::PASSWORD_REQUIRED => AgentError::PasswordRequired,
            codes::CANNOT_REMOVE_SELF => AgentError::CannotRemoveSelf,
            _ => AgentError::Usage(message),
        },
        other => other,
    }
}

/// `login` while `run` runs: the running agent signs in.
async fn login_through_agent(data: &DataDir, args: &LoginArgs, password: String, out: &mut impl Write) -> Result<(), AgentError> {
    let folder = args.folder.as_ref().map(|f| std::path::absolute(f).map_err(io_at(f))).transpose()?;
    let request = craft_cloud_app::LoginRequest {
        server: args.url.clone(),
        user: args.user.clone(),
        password,
        folder: folder.map(|f| f.display().to_string()),
        device_name: args.device_name.clone(),
        adopt: args.adopt,
    };
    let data_path = data.path().to_owned();
    let status = blocking(move || craft_cloud_app::Client::connect_in(&data_path)?.login(&request)).await.map_err(local_error)?;
    say!(
        out,
        "signed in as {} on {} (through the running agent, which syncs {} now)",
        status.account.unwrap_or_default(),
        status.server.unwrap_or_default(),
        status.folder.unwrap_or_default()
    )
}

/// `recents <app> [add <path>] [--watch]`: a client of the running agent like an app.
async fn recents(data: &DataDir, args: RecentsArgs, out: &mut impl Write) -> Result<Exit, AgentError> {
    let data_path = data.path().to_owned();
    let client = blocking(move || craft_cloud_app::Client::connect_in(&data_path)).await.map_err(|e| match e {
        AgentError::Local(craft_cloud_app::Error::NotRunning(why)) => {
            AgentError::Usage(format!("the agent isn't running ({why}); start `craft-cloud-agent run` first"))
        }
        other => other,
    })?;
    let app = args.app.clone();
    if let Some(RecentsAction::Add { path, title, saved }) = args.action {
        let status = {
            let c = client.clone();
            blocking(move || c.status()).await?
        };
        let path = relative_to_folder(&path, status.folder.as_deref().map(Path::new))?;
        let (c, a) = (client.clone(), app.clone());
        let entry = blocking(move || c.add_recent(&a, craft_cloud_app::NewRecent { path, title, saved })).await.map_err(local_error)?;
        say!(
            out,
            "added to {app}'s recent files: {}{}",
            entry.path,
            if status.online { "" } else { " (offline: it reaches the other machines later)" }
        )?;
    }
    if !args.watch {
        let a = app.clone();
        let entries = blocking(move || client.recents(&a)).await.map_err(local_error)?;
        print_recents(out, &app, &entries)?;
        return Ok(Exit::Done);
    }
    // Watching: the list now, then again after every change; while the agent is away (stopped, signed out) it says
    // so and goes on once the agent is back. Until stopped (Ctrl-C).
    let (lines, mut printed) = tokio::sync::mpsc::unbounded_channel::<craft_cloud_app::RecentsEvent>();
    let a = app.clone();
    let watching = tokio::task::spawn_blocking(move || -> Result<(), craft_cloud_app::Error> {
        let watch = client.watch_recents(&a)?;
        if lines.send(craft_cloud_app::RecentsEvent::Changed(watch.entries())).is_err() {
            return Ok(());
        }
        while let Ok(event) = watch.events().recv() {
            if lines.send(event).is_err() {
                return Ok(());
            }
        }
        Ok(())
    });
    let mut first = true;
    while let Some(event) = printed.recv().await {
        match event {
            craft_cloud_app::RecentsEvent::Changed(entries) => {
                if !first {
                    say!(out, "--")?;
                }
                first = false;
                print_recents(out, &app, &entries)?;
            }
            craft_cloud_app::RecentsEvent::Unavailable(why) => say!(out, "Craft Cloud isn't available ({why}); still watching")?,
            craft_cloud_app::RecentsEvent::Available => say!(out, "Craft Cloud is back")?,
            craft_cloud_app::RecentsEvent::PushStopped(true) => {
                say!(out, "this list is no longer sent up whole (see the agent's log); new entries still are")?;
            }
            craft_cloud_app::RecentsEvent::PushStopped(false) => {}
        }
        out.flush().map_err(io_at(Path::new("standard output")))?;
    }
    watching.await.map_err(|e| AgentError::Usage(format!("the watch failed: {e}")))?.map_err(|e| local_error(AgentError::Local(e)))?;
    Ok(Exit::Failed)
}

/// A path in the synced folder as the local API wants it: relative, `/`-separated. An absolute one (or, on Windows, one
/// with a root but no drive) must lie in the folder.
pub(crate) fn relative_to_folder(path: &Path, folder: Option<&Path>) -> Result<String, AgentError> {
    let relative = if path.is_absolute() || path.has_root() {
        let folder = folder.ok_or(AgentError::NotSignedIn)?;
        let (path, folder) = (resolved(path), resolved(folder));
        path.strip_prefix(&folder)
            .map(Path::to_owned)
            .map_err(|_| AgentError::Usage(format!("{} isn't inside the synced folder {}", path.display(), folder.display())))?
    } else {
        path.to_owned()
    };
    let names: Vec<String> = relative.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Ok(names.join("/"))
}

fn print_recents(out: &mut impl Write, app: &str, entries: &[craft_cloud_app::RecentEntry]) -> Result<(), AgentError> {
    if entries.is_empty() {
        return say!(out, "no recent files for {app}");
    }
    say!(out, "recent files for {app}, newest first:")?;
    for entry in entries {
        let saved = entry.saved_ms.map(|ms| format!(", saved {}", show_time(ms))).unwrap_or_default();
        let title = if entry.path.ends_with(&entry.title) { String::new() } else { format!(" \"{}\"", entry.title) };
        say!(out, "  {}  {}{title} (on {}{saved})", show_time(entry.opened_ms), entry.path, entry.machine)?;
    }
    Ok(())
}

/// Sends one message to `stop` for every Ctrl-C (and, on Unix, SIGTERM) until the loop is gone. If the signals can't be
/// listened for, the loop runs until the process is killed (its checkpoints keep that safe).
async fn forward_stop_signals(stop: tokio::sync::mpsc::UnboundedSender<()>) {
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let mut ctrl_c_works = true;
    loop {
        #[cfg(unix)]
        let terminated = async {
            // A stream that ended (no signal driver any more) never fires again.
            let got = match term.as_mut() {
                Some(term) => term.recv().await,
                None => None,
            };
            if got.is_none() {
                std::future::pending::<()>().await;
            }
        };
        #[cfg(not(unix))]
        let terminated = std::future::pending::<()>();
        tokio::select! {
            ctrl_c = tokio::signal::ctrl_c(), if ctrl_c_works => {
                // Ctrl-C can't be listened for: SIGTERM alone (or nothing) stops the loop.
                if ctrl_c.is_err() {
                    ctrl_c_works = false;
                    continue;
                }
            }
            _ = terminated => {}
        }
        if stop.send(()).is_err() {
            return;
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

pub(crate) fn now_ms() -> i64 {
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

/// Takes the lock at `path` for as long as the result lives, noting a warning (on standard error, so it never mixes
/// into the output a script reads, or in `run.log`) if the filesystem can't lock.
fn hold(path: &Path) -> Result<Lock, AgentError> {
    let (held, warning) = lock::acquire(path)?;
    if let Some(warning) = warning {
        crate::runlog::note(format_args!("{warning}"));
    }
    Ok(held)
}

/// Takes the data folder's lock (creating the data folder).
pub(crate) fn hold_data(data: &DataDir) -> Result<Lock, AgentError> {
    // Every writer of the token files (`login`, `run`) takes this lock first.
    if cfg!(windows) {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        check_private_place(data, local.as_deref())?;
    }
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
    let (a, b) = (folded(&resolved(a)), folded(&resolved(b)));
    a.starts_with(&b) || b.starts_with(&a)
}

/// A path's names, ignoring case on Windows and macOS, whose filesystems usually do.
fn folded(p: &Path) -> Vec<String> {
    p.components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy();
            if cfg!(any(windows, target_vendor = "apple")) { s.to_lowercase() } else { s.into_owned() }
        })
        .collect()
}

/// Whether `path` lies inside `base` (or is it), as the OS resolves both.
fn lies_inside(path: &Path, base: &Path) -> bool {
    folded(&resolved(path)).starts_with(&folded(&resolved(base)))
}

/// Windows: the data folder holds the device token (in `account.json`) and the local API's token, and gets the
/// permissions of the folder it is in. Inside the user's `%LOCALAPPDATA%` only the user (and administrators) can read
/// it; anywhere else other users may, so it is refused. (Setting an owner-only ACL ourselves would need `unsafe`
/// calls or a crate that wraps them unchecked here.) Elsewhere the data folder itself is made owner-only (0700).
fn check_private_place(data: &DataDir, local_app_data: Option<&Path>) -> Result<(), AgentError> {
    match local_app_data {
        Some(base) if base.is_absolute() && lies_inside(data.path(), base) => Ok(()),
        _ => Err(AgentError::Usage(format!(
            "the agent's data folder {} must be inside your %LOCALAPPDATA% on Windows: elsewhere other users of this computer \
             may be able to read its sign-in token. Use the default, or a --data-dir (or CRAFT_CLOUD_AGENT_DIR) under %LOCALAPPDATA%",
            data.path().display()
        ))),
    }
}

/// Refuses a synced folder and a data folder that lie inside one another: the data folder holds the device token,
/// which would then be synced (or the synced folder would sync the agent's own state).
fn check_apart(data: &DataDir, folder: &Path) -> Result<(), AgentError> {
    if nested(data.path(), folder) {
        return Err(AgentError::DataInFolder { data: resolved(data.path()), folder: resolved(folder) });
    }
    Ok(())
}

pub(crate) fn already_signed_in(account: &Account) -> AgentError {
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
    let account = login_locked(data, args, password).await?;
    say!(out, "signed in as {} on {} (device \"{}\")", account.username, account.server, account.device_name)?;
    say!(out, "syncing {}; run `craft-cloud-agent sync` to sync it", account.folder.display())?;
    Ok(account)
}

/// [`login`] for a caller that holds the data folder's lock already: the `run` loop, signing in for the local API.
/// Prints nothing.
pub(crate) async fn login_locked(data: &DataDir, args: &LoginArgs, password: &str) -> Result<Account, AgentError> {
    if let Some(account) = data.load_account()? {
        return Err(already_signed_in(&account));
    }
    let server = ServerUrl::parse(&args.url).map_err(|e| AgentError::Usage(e.to_string()))?;
    let prepared = prepare(data, args.folder.as_deref(), args.device_name.as_deref(), args.adopt)?;
    ask_server(&server, |_, message| AgentError::Offline(message)).await?;
    let signed_in = match http::login(&server, &args.user, password, &prepared.device_name).await {
        Err(TransportError::Unauthorized) => return Err(AgentError::WrongPassword),
        other => other?,
    };
    set_up(data, &server, signed_in, prepared).await
}

/// Joins with a join string (an invite or an add-a-computer code), creates a device and sets up the folder, as
/// [`login`] does. Everything that can be checked here is checked before the code is spent (the folder, the device
/// name, what the server speaks); a failure after it (the folder is set up for another space) removes the new device
/// again, and says the code is used up. `password`: the new account's, for an invite that needs one.
pub async fn join(data: &DataDir, args: &JoinArgs, password: Option<&str>, out: &mut impl Write) -> Result<Account, AgentError> {
    let _data_lock = hold_data(data)?;
    let account = join_locked(data, args, password).await?;
    say!(out, "joined as {} on {} (device \"{}\")", account.username, account.server, account.device_name)?;
    say!(out, "syncing {}; run `craft-cloud-agent run` to keep it in sync", account.folder.display())?;
    Ok(account)
}

/// [`join`] for a caller that holds the data folder's lock already: the `run` loop, joining for the local API.
/// Prints nothing.
pub(crate) async fn join_locked(data: &DataDir, args: &JoinArgs, password: Option<&str>) -> Result<Account, AgentError> {
    if let Some(account) = data.load_account()? {
        return Err(already_signed_in(&account));
    }
    let join = JoinString::parse(&args.join).map_err(|e| AgentError::Usage(e.to_string()))?;
    let server = ServerUrl::parse(&join.server).map_err(|e| AgentError::Usage(e.to_string()))?;
    let prepared = prepare(data, args.folder.as_deref(), args.device_name.as_deref(), args.adopt)?;
    let unreachable = |server: &ServerUrl, message| AgentError::Unreachable { server: server.to_string(), message };
    ask_server(&server, unreachable).await?;
    let signed_in = match http::join(&server, join.code.as_str(), &prepared.device_name, password).await {
        Ok(signed_in) => signed_in,
        Err(JoinError::InvalidCode) => return Err(AgentError::InvalidCode),
        Err(JoinError::PasswordRequired) => return Err(AgentError::PasswordRequired),
        Err(JoinError::WeakPassword(message)) => return Err(AgentError::WeakPassword(message)),
        Err(JoinError::RateLimited(message)) => return Err(AgentError::TooManyTries(message)),
        Err(JoinError::Transport(TransportError::Unavailable(message))) => return Err(unreachable(&server, message)),
        Err(JoinError::Transport(e)) => return Err(e.into()),
    };
    set_up(data, &server, signed_in, prepared).await.map_err(|e| AgentError::JoinSetup(Box::new(e)))
}

/// What a sign-in or join checked before anything is created on the server.
struct Prepared {
    /// The folder to sync, absolute.
    folder: PathBuf,
    /// The root marker already in it, if any.
    existing_marker: Option<RootMarker>,
    device_name: String,
}

/// The checks of a sign-in or join that need no server: the folder (not inside the data folder, a folder if it exists,
/// with files only with `adopt`), its marker if it has one, and the device name.
fn prepare(data: &DataDir, folder: Option<&Path>, device_name: Option<&str>, adopt: bool) -> Result<Prepared, AgentError> {
    let folder = match folder {
        Some(folder) => std::path::absolute(folder).map_err(io_at(folder))?,
        None => default_folder()?,
    };
    check_apart(data, &folder)?;
    if folder.exists() {
        if !folder.is_dir() {
            return Err(AgentError::Usage(format!("{} is not a folder", folder.display())));
        }
        if !adopt && has_files(&folder)? {
            return Err(AgentError::FolderHasFiles(folder));
        }
    }
    let existing_marker = root::read(&folder)?;
    let device_name = device_name.map_or_else(default_device_name, str::to_owned);
    if !valid_device_name(&device_name) {
        return Err(AgentError::Usage("device names are 1-128 bytes without control characters".into()));
    }
    Ok(Prepared { folder, existing_marker, device_name })
}

/// Asks the server what it speaks and checks this agent speaks it too. A certificate this computer doesn't trust is
/// [`AgentError::UntrustedCertificate`]; a server that can't be reached is what `unreachable` makes of the message.
async fn ask_server(server: &ServerUrl, unreachable: impl FnOnce(&ServerUrl, String) -> AgentError) -> Result<(), AgentError> {
    match http::reach(server).await {
        Ok(info) => Ok(http::check_compatible(&info)?),
        Err(ReachError::UntrustedCertificate) => Err(AgentError::UntrustedCertificate { host: server.host() }),
        Err(ReachError::Transport(TransportError::Unavailable(message))) => Err(unreachable(server, message)),
        Err(ReachError::Transport(e)) => Err(e.into()),
    }
}

/// Sets up the folder for the device the server just created (its marker, the saved account). Any failure revokes
/// the device again (best effort) and leaves no account behind.
async fn set_up(data: &DataDir, server: &ServerUrl, signed_in: LoginResponse, prepared: Prepared) -> Result<Account, AgentError> {
    let Prepared { folder, existing_marker, device_name } = prepared;
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
    match setup.await {
        Ok(account) => Ok(account),
        Err(e) => {
            let _ = transport.revoke(signed_in.device).await;
            Err(e)
        }
    }
}

/// What `sync --approve-deletes` approves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approve {
    /// Nothing (no flag).
    Nothing,
    /// The bare flag: the set the last pass listed (while a loop runs: if `status` showed it).
    Listed,
    /// `--approve-deletes=<set>`: that set, if it is the one waiting.
    Set(String),
}

/// The value of `--approve-deletes=<set>`. An empty one is a usage error, not the bare flag: a script running
/// `--approve-deletes=$SET` with `$SET` unset must not approve whatever `status` showed.
fn set_name(value: &str) -> Result<String, String> {
    let set = value.trim();
    if set.is_empty() {
        return Err("the set is empty: give the set the list was printed with, or leave out `=` to approve the list `status` showed".into());
    }
    Ok(set.to_ascii_lowercase())
}

impl Approve {
    /// From the command line's `--approve-deletes[=<set>]`: absent, the bare flag, or a set.
    pub fn from_flag(flag: Option<Option<String>>) -> Self {
        match flag {
            None => Self::Nothing,
            Some(None) => Self::Listed,
            Some(Some(set)) => Self::Set(set.trim().to_ascii_lowercase()),
        }
    }

    fn any(&self) -> bool {
        *self != Self::Nothing
    }
}

impl From<bool> for Approve {
    fn from(approve: bool) -> Self {
        if approve { Self::Listed } else { Self::Nothing }
    }
}

/// `--approve-deletes=<set>` named a set that isn't the one waiting: the waiting one is shown (and noted as shown),
/// and nothing is approved (exit code 2).
fn wrong_set(data: &DataDir, named: &str, waiting: &PassSummary, out: &mut impl Write) -> Result<(Exit, PassSummary), AgentError> {
    print_waiting(out, waiting)?;
    if let Some(set) = waiting.pending_set() {
        data.save_shown(&set, now_ms())?;
    }
    say!(out, "nothing approved: set {named} isn't a set waiting now; check the list above, then approve its set")?;
    Ok((if waiting.needs_approval() { Exit::NeedsApproval } else { Exit::Done }, waiting.clone()))
}

/// Runs one sync pass and saves the state, whether the pass finished or not. Returns the exit code and what
/// the pass did.
///
/// While a `run` loop holds the locks, `--approve-deletes` hands the approval to it instead (see
/// [`approve_for_loop`]); without the flag the pass stops with exit code 3, as with any other agent running.
/// `--approve-deletes=<set>` approves only if `set` is the set waiting; otherwise nothing runs, the waiting set is
/// shown and the exit code is 2.
pub async fn sync(data: &DataDir, approve: impl Into<Approve>, out: &mut impl Write) -> Result<(Exit, PassSummary), AgentError> {
    let approve_deletes: Approve = approve.into();
    let _data_lock = match hold_data(data) {
        Err(AgentError::Lock(LockError::Busy(path))) if approve_deletes.any() => {
            return approve_for_loop(data, &approve_deletes, out)?.ok_or(AgentError::Lock(LockError::Busy(path)));
        }
        other => other?,
    };
    let mut session = Session::open(data, None, true)?;
    let previous = session.last_summary();
    let approve = match &approve_deletes {
        Approve::Nothing => Vec::new(),
        // Exactly the deletions the user saw listed; new ones wait and are listed again.
        Approve::Listed => {
            let shown = previous.as_ref().map(|s| s.pending_nodes.clone()).unwrap_or_default();
            let held = previous.as_ref().and_then(PassSummary::held_set);
            if let Some(set) = &held {
                say!(out, "{}", held_not_released(set))?;
            }
            if shown.is_empty() && held.is_none() {
                say!(out, "nothing was waiting for approval")?;
            }
            shown
        }
        Approve::Set(named) => {
            let waiting = previous.as_ref().map(PassSummary::pending_only).unwrap_or_default();
            // The set of deletes held behind a link: named, they go.
            if waiting.held_set().as_ref() == Some(named) {
                say!(out, "releasing the deletes held behind a link (set {named})")?;
                waiting.held_nodes
            } else if waiting.pending_set().as_ref() == Some(named) {
                waiting.pending_nodes
            } else if waiting.approvable() {
                return wrong_set(data, named, &waiting, out);
            } else {
                say!(out, "nothing was waiting for approval")?;
                return Ok((Exit::Done, waiting));
            }
        }
    };
    let summary = session.pass(approve).await?;
    print_summary(out, &summary)?;
    Ok((if summary.needs_approval() { Exit::NeedsApproval } else { Exit::Done }, summary))
}

/// `sync --approve-deletes` while a `run` loop holds the locks. The set of deletions the loop's last pass listed
/// (from `state.json`) is approved only if it is exactly the set `status` last showed (`shown.json`, by its
/// identifier): then it goes to `approval.json`, naming that set, and the loop releases exactly those deletions if
/// they are still the ones that wait. If the set changed since `status` showed it (a pass found more, say), or
/// `status` never showed it, nothing is approved: the new list is shown (and remembered as shown) and the exit code
/// is 2, so the user checks it and runs the command again. The rule of the one-off approval: what was shown,
/// nothing else. With `--approve-deletes=<set>` the user names the set (as the loop printed it, say), so it is
/// approved if it is the set waiting, whatever `status` showed; another set is refused the same way. Naming the set
/// of deletes held behind a link ([`PassSummary::held_set`]) hands those to the loop the same way; nothing else
/// releases a hold. `None` when no running loop is recorded (another one-off `sync` holds the locks).
fn approve_for_loop(data: &DataDir, approve: &Approve, out: &mut impl Write) -> Result<Option<(Exit, PassSummary)>, AgentError> {
    if !data.load_status()?.is_some_and(|s| s.running(now_ms())) {
        return Ok(None);
    }
    let waiting = data.load_state()?.last_pass.and_then(|l| l.summary).map(|s| s.pending_only()).unwrap_or_default();
    // The set of deletes held behind a link goes only when named.
    if let Approve::Set(named) = approve
        && waiting.held_set().as_ref() == Some(named)
    {
        data.give_approval(named, &waiting.held_nodes, now_ms())?;
        say!(
            out,
            "the running agent will delete these on the server, held behind a link until now (set {named}; it looks for \
             this within half a minute):"
        )?;
        for (path, links) in &waiting.held_deletes {
            say!(out, "  delete on the server: {} (held while any of {} is a link)", path, links.join(", "))?;
        }
        return Ok(Some((Exit::Done, waiting)));
    }
    // The bare flag never releases a hold: say how to, rather than "nothing was waiting".
    let held = waiting.held_set().filter(|_| *approve == Approve::Listed);
    if let Some(set) = &held {
        say!(out, "{}", held_not_released(set))?;
    }
    let Some(set) = waiting.pending_set() else {
        if let Approve::Set(named) = approve
            && waiting.approvable()
        {
            return wrong_set(data, named, &waiting, out).map(Some);
        }
        if held.is_none() {
            say!(out, "nothing was waiting for approval")?;
        }
        return Ok(Some((Exit::Done, waiting)));
    };
    if let Approve::Set(named) = approve {
        if *named != set {
            return wrong_set(data, named, &waiting, out).map(Some);
        }
    } else if data.load_shown()?.is_none_or(|shown| shown.set != set) {
        print_waiting(out, &waiting)?;
        data.save_shown(&set, now_ms())?;
        say!(out, "nothing approved: these deletions changed since `status` listed them (or it didn't); check them, then run this again")?;
        return Ok(Some((Exit::NeedsApproval, waiting)));
    }
    data.give_approval(&set, &waiting.pending_nodes, now_ms())?;
    say!(out, "the running agent will carry out these deletions (set {set}):")?;
    for path in &waiting.pending_deletes {
        say!(out, "  delete on the server: {path}")?;
    }
    for path in &waiting.pending_local_deletes {
        say!(out, "  delete here: {path}")?;
    }
    Ok(Some((Exit::Done, waiting)))
}

/// A signed-in folder ready for passes: its account, the engine on the real disk and the HTTP API, and the folder's
/// lock. The caller holds the data folder's lock for as long as the session lives. One-off `sync` opens one for one
/// pass; the `run` loop keeps one for many (so the stability window's looks carry over between passes).
pub struct Session {
    data: DataDir,
    account: Account,
    engine: Engine<DiskFs, HttpTransport>,
    /// The last pass's outcome, also written with every checkpoint (so it is shared with the checkpoint closure).
    last_pass: std::sync::Arc<std::sync::Mutex<Option<LastPass>>>,
    _folder_lock: Lock,
    /// The stability window's looks, shared with the `run` loop (`None`: a one-off `sync` trusts every file).
    stability: Option<Stability>,
    /// When the earliest look at a file still changing is due, after the last pass (if it finished).
    recheck_in: Option<std::time::Duration>,
}

impl Session {
    /// Opens the folder the account syncs: cleans up interrupted writes, checks the account, the folders and the
    /// marker, takes the folder's lock and loads the state. `stability`: the window a changed file must settle for
    /// before it syncs, and its looks (`None`: trusted at once). `clean_data`: clean up the data folder's interrupted
    /// writes first.
    pub fn open(data: &DataDir, stability: Option<Stability>, clean_data: bool) -> Result<Self, AgentError> {
        // Temporary files a crash left while one of the data folder's own files was written. The `run` loop cleans
        // them once, at its start (`clean_data` false here), never while it may be writing its own.
        if clean_data {
            data.clean_tmp()?;
        }
        let account = data.load_account()?.ok_or(AgentError::NotSignedIn)?;
        check_apart(data, &account.folder)?;
        root::check(&account.folder, &RootMarker { space: account.space, root: account.root })?;
        let folder_lock = hold_folder(&account.folder)?;
        let mut saved = data.load_state()?;
        if saved.state.space.is_some_and(|s| s != account.space) {
            return Err(AgentError::OtherAccountState(data.path().join(crate::account::STATE_FILE)));
        }
        for node in drop_holds_without_links(&mut saved.state) {
            crate::runlog::note(format_args!("a held delete saved without any link was dropped (node {node}; state.json was edited or damaged)"));
        }
        let mut fs = DiskFs::new(&account.folder).with_device_name(account.device_name.clone());
        if let Some(stability) = stability.clone() {
            fs = fs.with_stability(stability);
        }
        fs.clean_tmp().map_err(io_at(&fs.tmp_dir()))?;
        let transport = HttpTransport::new(account.server.clone(), &account.token)?;
        let config = Config::new(account.device_name.clone(), account.device);
        let mut engine = Engine::new(fs, transport, std::mem::take(&mut saved.state), config, conflict_stamp)?;
        // What Craft apps saved from live documents (their local API's `saved`): see `live::saves`.
        engine.set_save_records(std::sync::Arc::new(crate::live::saves::SaveBook::new(data.path(), account.space)));
        // `state.json` is also written at the pass's checkpoints, so a crash or a kill loses at most what the pass did
        // since the last one (the last pass's outcome stays as it was until this one ends).
        let last_pass = std::sync::Arc::new(std::sync::Mutex::new(saved.last_pass));
        let (checkpoint_data, checkpoint_last) = (data.clone(), last_pass.clone());
        engine.on_checkpoint(move |state| {
            let last_pass = checkpoint_last.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            let saved = SavedState { schema: STATE_SCHEMA, state: state.clone(), last_pass, paused: false };
            checkpoint_data.save_state(&saved).map_err(|e| std::io::Error::other(e.to_string()))
        });
        Ok(Self { data: data.clone(), account, engine, last_pass, _folder_lock: folder_lock, stability, recheck_in: None })
    }

    pub fn account(&self) -> &Account {
        &self.account
    }

    /// The engine's state (its `seq` is where the change feed was read to).
    pub fn state(&self) -> &craft_cloud_sync::State {
        self.engine.state()
    }

    /// The last pass's outcome, as saved in `state.json`.
    pub fn last_pass(&self) -> Option<LastPass> {
        self.last_pass.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// After a finished pass with a stability window: how long until the earliest look at a file still changing.
    pub fn recheck_in(&self) -> Option<std::time::Duration> {
        self.recheck_in
    }

    fn last_summary(&self) -> Option<PassSummary> {
        self.last_pass().and_then(|l| l.summary)
    }

    /// One pass, with `approve` released from the mass-delete guard (deletions a user saw listed), then the state is
    /// saved, whether the pass finished or not. The folder and its marker are checked first: the folder may have
    /// gone since the session opened (a drive unmounted while the loop runs).
    pub async fn pass(&mut self, approve: Vec<NodeId>) -> Result<PassSummary, AgentError> {
        check_apart(&self.data, &self.account.folder)?;
        root::check(&self.account.folder, &RootMarker { space: self.account.space, root: self.account.root })?;
        if !approve.is_empty() {
            self.engine.approve_deletes_of(approve);
        }
        let previous = self.last_summary();
        // The server is asked what it speaks before every pass (it may have been updated since the last one); a
        // mismatch fails the pass like any other server answer, so it is saved and shown by `status`.
        let compatible = match http::info(&self.account.server).await {
            Ok(info) => http::check_compatible(&info),
            Err(e) => Err(e),
        };
        let started = self.stability.as_ref().map(Stability::now);
        let result = match compatible {
            Ok(()) => self.engine.sync_once().await,
            Err(e) => Err(SyncError::Transport(e)),
        };
        // A finished pass asked about every changed file: when the earliest one still changing is looked at again.
        self.recheck_in = match (&result, &self.stability, started) {
            (Ok(_), Some(stability), Some(started)) => stability.pass_finished(started),
            _ => None,
        };
        let (outcome, summary) = match &result {
            Ok(report) => {
                let summary = PassSummary::from_report(report);
                (if summary.needs_approval() { Outcome::NeedsApproval } else { Outcome::Done }, Some(summary))
            }
            // A failed pass keeps the deletions the pass before it listed: the user was shown them, and a retry
            // after a network blip must still release exactly those.
            Err(e) => {
                (Outcome::Failed(failure_message(e)), previous.map(|s| s.pending_only()).filter(|s| s.needs_approval() || !s.held_deletes.is_empty()))
            }
        };
        let last = LastPass { finished_ms: now_ms(), outcome, summary: summary.clone() };
        *self.last_pass.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(last.clone());
        let saved = SavedState { schema: STATE_SCHEMA, state: self.engine.state().clone(), last_pass: Some(last), paused: false };
        self.data.save_state(&saved)?;
        result?;
        Ok(summary.unwrap_or_default())
    }
}

/// Drops the held deletes saved with no link (only a hand-edited or damaged `state.json` has them): such a hold
/// would never be listed, so the user couldn't see or release it. Returns their nodes, to be said.
fn drop_holds_without_links(state: &mut craft_cloud_sync::State) -> Vec<NodeId> {
    let empty: Vec<NodeId> = state.held_deletes.iter().filter(|(_, link)| link.paths.is_empty()).map(|(id, _)| *id).collect();
    for id in &empty {
        state.held_deletes.remove(id);
    }
    empty
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

pub(crate) fn print_summary(out: &mut impl Write, s: &PassSummary) -> Result<(), AgentError> {
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

/// What the bare `--approve-deletes` says while deletes are held behind a link: it doesn't release them.
fn held_not_released(set: &str) -> String {
    format!(
        "deletes held behind a link are released only by naming their set: check their links (`craft-cloud-agent \
         status`), then run `craft-cloud-agent sync --approve-deletes={set}`"
    )
}

/// One held delete and every one of its links: the user releases it only after checking all of them.
fn held_line(path: &str, links: &[String]) -> String {
    match links {
        [one] => format!("{path}: held while {one} is a link"),
        _ => format!("{path}: held while any of {} is a link", links.join(", ")),
    }
}

/// The deletions waiting for approval, and the ones held behind a link, if any.
fn print_waiting(out: &mut impl Write, s: &PassSummary) -> Result<(), AgentError> {
    if !s.held_deletes.is_empty() {
        say!(
            out,
            "held, not deleted on the server: it isn't here, and a link or special file that appeared with its name (or \
             a folder's on its path) may be it, moved with its folder (remove or replace the link and the next pass \
             decides again):"
        )?;
        for (path, links) in &s.held_deletes {
            say!(out, "  {}", held_line(path, links))?;
        }
        if let Some(set) = s.held_set() {
            say!(
                out,
                "if you did delete these, and none of their links is the file, release them with `craft-cloud-agent \
                 sync --approve-deletes={set}`"
            )?;
        }
    }
    if s.needs_approval() {
        let set = s.pending_set().unwrap_or_default();
        say!(out, "waiting for approval, set {set} (check the list, then run `craft-cloud-agent sync --approve-deletes={set}`):")?;
        for path in &s.pending_deletes {
            say!(out, "  delete on the server: {path}")?;
        }
        for path in &s.pending_local_deletes {
            say!(out, "  delete here: {path}")?;
        }
    }
    Ok(())
}

/// How a signed-out agent is signed in, as `status` and `run` say it.
pub const SIGN_IN_HINT: &str = "open the Craft Cloud app, or run `craft-cloud-agent join <join string>`";

/// Prints the account, the folder and the last pass. Never prints the token.
pub fn status(data: &DataDir, out: &mut impl Write) -> Result<(), AgentError> {
    let Some(account) = data.load_account()? else {
        say!(out, "not signed in")?;
        if let Ok(Some(loop_status)) = data.load_status()
            && loop_status.running(now_ms())
        {
            say!(out, "agent: running (pid {}), waiting for a sign-in: {SIGN_IN_HINT}", loop_status.pid)?;
        }
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
    // Whether `run` is running here, from what it last wrote (it holds the locks, so it can't be asked).
    match data.load_status() {
        Ok(Some(loop_status)) if loop_status.running(now_ms()) => {
            say!(out, "agent: running (pid {}), {}", loop_status.pid, describe_loop(&loop_status.state))?;
            if let Some(why) = &loop_status.not_watching {
                say!(out, "  not watching the folder ({why}): changes there are found by a full pass every minute")?;
            }
        }
        Ok(Some(loop_status)) => say!(out, "agent: not running (last seen {})", show_time(loop_status.heartbeat_ms))?,
        Ok(None) => say!(out, "agent: not running (`craft-cloud-agent run` keeps the folder in sync)")?,
        Err(e) => say!(out, "agent: can't tell whether it runs: {e}")?,
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
                // `sync --approve-deletes` approves exactly the set shown here, and nothing that came since.
                if let Some(set) = summary.pending_set()
                    && let Err(e) = data.save_shown(&set, now_ms())
                {
                    say!(out, "(can't note that this list was shown: {e}; `sync --approve-deletes` will show it again)")?;
                }
            }
        }
    }
    Ok(())
}

/// What the running loop is doing, in words.
fn describe_loop(state: &LoopState) -> String {
    match state {
        LoopState::Idle => "idle, waiting for changes".into(),
        LoopState::Syncing => "syncing".into(),
        LoopState::Retrying { next_try_ms, message } => format!("offline, retrying at {}: {message}", show_time(*next_try_ms)),
        LoopState::Paused { reason, next_check_ms: Some(at) } => format!("paused: {reason} (looked at again at {})", show_time(*at)),
        LoopState::Paused { reason, next_check_ms: None } => {
            format!("paused: {reason} (stop it with Ctrl-C, put this right, then start `craft-cloud-agent run` again)")
        }
        LoopState::Stopping => "stopping after the running pass".into(),
        LoopState::Stopped => "stopped".into(),
        LoopState::SignedOut => "not signed in, waiting for a sign-in".into(),
        LoopState::PausedByUser => "paused (`craft-cloud-agent resume` syncs again)".into(),
    }
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
        revoke_device(&account).await?;
    }
    forget_account(data)?;
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

/// Revokes this device on the server, to sign out. A device the server already revoked (or doesn't know) counts as
/// done; an unreachable server is [`AgentError::LogoutOffline`], and nothing is changed.
pub(crate) async fn revoke_device(account: &Account) -> Result<(), AgentError> {
    let transport = HttpTransport::new(account.server.clone(), &account.token)?;
    match transport.revoke(account.device).await {
        Ok(()) | Err(TransportError::Unauthorized | TransportError::NotFound) => Ok(()),
        Err(TransportError::Unavailable(message)) => Err(AgentError::LogoutOffline(message)),
        Err(e) => Err(e.into()),
    }
}

/// Forgets the account and the sync state here (the files stay). The caller holds the data folder's lock.
pub(crate) fn forget_account(data: &DataDir) -> Result<(), AgentError> {
    data.remove_account()?;
    data.remove_state()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_delete_is_shown_with_its_link_and_its_own_set() {
        let held = vec![("Footage/c0.mov".into(), vec!["Clips/c0.mov".into()])];
        let s = PassSummary { held_deletes: held.clone(), held_nodes: vec![NodeId::generate()], ..PassSummary::default() };
        let mut out = Vec::new();
        print_summary(&mut out, &s).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("held, not deleted on the server") && text.contains("  Footage/c0.mov: held while Clips/c0.mov is a link"), "{text}");
        let set = s.held_set().unwrap();
        assert!(text.contains(&format!("craft-cloud-agent sync --approve-deletes={set}")), "{text}");
        assert_eq!(s.pending_only(), s, "a failed pass keeps them in the status");
        assert!(s.pending_set().is_none() && !s.needs_approval() && s.approvable(), "the guard's set doesn't cover a held delete");
        // Another node, or a summary an older agent saved without the nodes: another set, or none.
        assert_ne!(PassSummary { held_nodes: vec![NodeId::generate()], ..s.clone() }.held_set(), Some(set));
        assert_eq!(PassSummary { held_deletes: held, ..PassSummary::default() }.held_set(), None);
    }

    /// Review 8, info: a hold saved with no link (`{"paths": []}`, from a damaged state) is dropped at load, never
    /// kept as a hold nobody can see.
    #[test]
    fn a_hold_saved_without_a_link_is_dropped() {
        let (empty, kept) = (NodeId::generate(), NodeId::generate());
        let mut state = craft_cloud_sync::State::default();
        state.held_deletes.insert(empty, serde_json::from_value(serde_json::json!({ "paths": [] })).unwrap());
        let link = craft_cloud_sync::HeldLink::at(serde_json::from_value(serde_json::json!(["Clips", "c0.mov"])).unwrap());
        state.held_deletes.insert(kept, link);
        assert_eq!(drop_holds_without_links(&mut state), vec![empty]);
        assert_eq!(state.held_deletes.keys().copied().collect::<Vec<_>>(), vec![kept]);
    }

    /// Review 8, finding 1: a hold with several links lists every one of them, and its set covers them all. A set
    /// computed over one of them (what the agent showed before) is another set, so it can't release the hold.
    #[test]
    fn a_hold_lists_every_link_and_its_set_covers_them_all() {
        let node = NodeId::generate();
        let both = PassSummary {
            held_deletes: vec![("Footage/c0.mov".into(), vec!["A/c0.mov".into(), "Clips/c0.mov".into()])],
            held_nodes: vec![node],
            ..PassSummary::default()
        };
        let mut out = Vec::new();
        print_summary(&mut out, &both).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("  Footage/c0.mov: held while any of A/c0.mov, Clips/c0.mov is a link"), "{text}");
        let one = PassSummary { held_deletes: vec![("Footage/c0.mov".into(), vec!["A/c0.mov".into()])], ..both.clone() };
        assert_ne!(one.held_set(), both.held_set(), "a set over one link isn't the hold's set");
        let swapped = PassSummary { held_deletes: vec![("Footage/c0.mov".into(), vec!["Clips/c0.mov".into(), "A/c0.mov".into()])], ..both.clone() };
        assert_eq!(swapped.held_set(), both.held_set(), "the order of the links doesn't matter");
        // A summary an older agent saved: one link per hold, as a string.
        let json = serde_json::json!({ "held_deletes": [["Footage/c0.mov", "A/c0.mov"]], "held_nodes": [node] });
        let old: PassSummary = serde_json::from_value(json).unwrap();
        assert_eq!(old, one);
        assert_eq!(serde_json::from_value::<PassSummary>(serde_json::to_value(&both).unwrap()).unwrap(), both, "round trip");
    }

    #[test]
    fn the_cli_parses() {
        let cli = Cli::try_parse_from(["craft-cloud-agent", "login", "https://c.example", "--user", "ana", "--adopt", "--data-dir", "d"]).unwrap();
        assert_eq!(cli.data_dir.as_deref(), Some(Path::new("d")));
        assert!(matches!(cli.command, Command::Login(LoginArgs { adopt: true, ref user, .. }) if user == "ana"));
        let cli = Cli::try_parse_from(["craft-cloud-agent", "sync", "--approve-deletes"]).unwrap();
        let Command::Sync { approve_deletes } = cli.command else { panic!("{:?}", cli.command) };
        assert_eq!(Approve::from_flag(approve_deletes), Approve::Listed);
        let cli = Cli::try_parse_from(["craft-cloud-agent", "sync", "--approve-deletes=3F9A0b1c2d3e"]).unwrap();
        let Command::Sync { approve_deletes } = cli.command else { panic!("{:?}", cli.command) };
        assert_eq!(Approve::from_flag(approve_deletes), Approve::Set("3f9a0b1c2d3e".into()));
        let cli = Cli::try_parse_from(["craft-cloud-agent", "sync"]).unwrap();
        let Command::Sync { approve_deletes } = cli.command else { panic!("{:?}", cli.command) };
        assert_eq!(Approve::from_flag(approve_deletes), Approve::Nothing);
        for empty in ["--approve-deletes=", "--approve-deletes= "] {
            let e = Cli::try_parse_from(["craft-cloud-agent", "sync", empty]).unwrap_err();
            assert_eq!(e.kind(), clap::error::ErrorKind::ValueValidation, "{empty:?} is a usage error: {e}");
        }
        assert!(Cli::try_parse_from(["craft-cloud-agent", "login", "https://c.example"]).is_err(), "--user is required");
    }

    #[test]
    fn the_join_and_computer_commands_parse() {
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("craft-cloud-agent").chain(args.iter().copied())).map(|c| c.command);
        let join = parse(&["join", "c.example/K7M4-Q9TX", "--folder", "f", "--name", "lptp1", "--adopt"]).unwrap();
        let Command::Join(args) = join else { panic!("{join:?}") };
        assert_eq!(args.join, "c.example/K7M4-Q9TX");
        assert_eq!((args.folder.as_deref(), args.device_name.as_deref(), args.adopt), (Some(Path::new("f")), Some("lptp1"), true));
        let Command::Join(args) = parse(&["join", "c.example/K7M4-Q9TX", "--device-name", "pc", "--autostart"]).unwrap() else { panic!() };
        assert_eq!((args.device_name.as_deref(), args.adopt, args.autostart), (Some("pc"), false, true));
        assert!(matches!(parse(&["autostart", "on"]).unwrap(), Command::Autostart { state: OnOff::On }));
        assert!(matches!(parse(&["autostart", "off"]).unwrap(), Command::Autostart { state: OnOff::Off }));
        assert!(parse(&["autostart"]).is_err() && parse(&["autostart", "maybe"]).is_err());
        assert!(parse(&["join"]).is_err(), "the join string is required");
        assert!(matches!(parse(&["add-computer"]).unwrap(), Command::AddComputer));
        assert!(matches!(parse(&["pause"]).unwrap(), Command::Pause));
        assert!(matches!(parse(&["resume"]).unwrap(), Command::Resume));
        assert!(matches!(parse(&["quit"]).unwrap(), Command::Quit));
        assert!(matches!(parse(&["devices"]).unwrap(), Command::Devices { action: None }));
        let id = craft_cloud_proto::DeviceId::generate();
        let removed = parse(&["devices", "remove", &id.to_string()]).unwrap();
        assert!(matches!(removed, Command::Devices { action: Some(DevicesAction::Remove { id: got }) } if got == id), "{removed:?}");
        assert!(parse(&["devices", "remove", "not-an-id"]).is_err());
    }

    /// A new account's password is asked twice; two that differ, or none, are refused.
    #[test]
    fn a_new_password_is_typed_twice() {
        let mut out = Vec::new();
        assert_eq!(new_password(&mut "a new password\na new password\n".as_bytes(), &mut out).unwrap(), "a new password");
        assert!(String::from_utf8(out).unwrap().contains("new account"));
        assert!(
            matches!(new_password(&mut "a new password\nanother one\n".as_bytes(), &mut Vec::new()), Err(AgentError::Usage(m)) if m.contains("differ"))
        );
        assert!(new_password(&mut "".as_bytes(), &mut Vec::new()).is_err());
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

    /// Every error a pass can end with, and how the agent ends for it: only damaged data and what a person must
    /// fix stop with "needs you"; a per-file problem never gets here (the engine skips that file and goes on).
    #[test]
    fn every_error_a_pass_ends_with_has_its_exit() {
        use craft_cloud_proto::ChunkId;
        let cases: Vec<(SyncError, Exit)> = vec![
            (SyncError::Transport(TransportError::Rejected(Vec::new())), Exit::Failed),
            // A manifest refused twice for missing chunks (the engine already sent them once): try again later.
            (SyncError::Transport(TransportError::MissingChunks(vec![ChunkId::from_bytes([1; 32])])), Exit::Failed),
            (SyncError::Transport(TransportError::ResyncRequired), Exit::Failed),
            (SyncError::Transport(TransportError::NotFound), Exit::Failed),
            (SyncError::Transport(TransportError::Unauthorized), Exit::NeedsYou),
            (SyncError::Transport(TransportError::Unavailable("offline".into())), Exit::Failed),
            (SyncError::Transport(TransportError::Protocol("a refusal naming no operation".into())), Exit::NeedsYou),
            // Only with no room left at all: a file that doesn't fit while others still may is skipped instead.
            (SyncError::Transport(TransportError::QuotaExceeded { used: 10, limit: 10 }), Exit::NeedsYou),
            (SyncError::Io { path: "an interrupted download".into(), source: std::io::Error::other("x") }, Exit::Failed),
            (SyncError::Corrupt("chunk x of a.txt is damaged".into()), Exit::NeedsYou),
            (SyncError::Checkpoint(std::io::Error::other("disk full")), Exit::Failed),
        ];
        for (error, exit) in cases {
            // Exhaustive on purpose: a new variant must be placed in the list above.
            match &error {
                SyncError::Transport(
                    TransportError::Rejected(_)
                    | TransportError::MissingChunks(_)
                    | TransportError::ResyncRequired
                    | TransportError::NotFound
                    | TransportError::Unauthorized
                    | TransportError::Unavailable(_)
                    | TransportError::Protocol(_)
                    | TransportError::QuotaExceeded { .. },
                )
                | SyncError::Io { .. }
                | SyncError::Corrupt(_)
                | SyncError::Checkpoint(_) => {}
            }
            let shown = error.to_string();
            assert_eq!(AgentError::from(error).exit(), exit, "{shown}");
        }
    }

    #[test]
    /// On Windows the data folder must lie inside `%LOCALAPPDATA%` (the rule itself is checked here on every
    /// platform; `hold_data` applies it on Windows only).
    fn a_data_folder_outside_the_private_place_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("AppData").join("Local");
        std::fs::create_dir_all(&local).unwrap();
        let inside = DataDir::new(local.join("craft-cloud-agent"));
        assert!(check_private_place(&inside, Some(&local)).is_ok());
        assert!(check_private_place(&DataDir::new(local.join("Temp/not-yet/made")), Some(&local)).is_ok(), "not made yet is fine");
        for outside in [dir.path().join("Shared/agent"), dir.path().join("AppData/LocalX/agent"), local.join("../Roaming/agent")] {
            let e = check_private_place(&DataDir::new(&outside), Some(&local)).unwrap_err();
            assert!(e.to_string().contains("%LOCALAPPDATA%"), "{}: {e}", outside.display());
        }
        assert!(check_private_place(&inside, None).is_err(), "no %LOCALAPPDATA%: refused");
        assert!(check_private_place(&inside, Some(Path::new("relative"))).is_err());
        #[cfg(unix)]
        {
            // Through a link: as the OS resolves it.
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(dir.path().join("Shared"), &link).unwrap();
            std::fs::create_dir_all(dir.path().join("Shared")).unwrap();
            assert!(check_private_place(&DataDir::new(link.join("agent")), Some(&local)).is_err());
        }
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
