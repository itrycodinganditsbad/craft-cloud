//! What the agent keeps between runs, in its data folder (one synced folder per data folder):
//!
//! - `account.json`: the server, user, device, device name, synced folder and the device token. Written
//!   owner-only: mode 0600 on Unix (the data folder itself is 0700). Windows has no mode bits; the default data
//!   folder is in the user's profile (`%LOCALAPPDATA%`), which only that user (and administrators) can read. A
//!   `--data-dir` outside the profile on Windows is refused, since it would get that folder's ACL, which may let
//!   others read the token. An OS keychain can come later.
//! - `state.json`: `{ "schema": 1, "state": <the engine's State>, "last_pass": … }`, written after every pass,
//!   failed or not. One document, not a table per node: the engine works on the whole state in memory anyway.
//! - `status.json`: what the `run` loop is doing ([`LoopStatus`]), for `status`; `shown.json`: the set of waiting
//!   deletions `status` last listed ([`Shown`]); `approval.json`: deletions
//!   `sync --approve-deletes` approved while the loop held the locks ([`Approval`]), taken by the loop.
//!
//! All are written atomically (temporary file, flush, rename), so a crash leaves the old file or the new one.
//! A crash loses at most one pass of bookkeeping, which the next pass redoes. A file written by a newer agent
//! (a higher `schema`) is refused, never half-read.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use craft_cloud_proto::{DeviceId, NodeId, SpaceId};
use craft_cloud_sync::{Report, State};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::http::{DeviceToken, ServerUrl};

pub const ACCOUNT_FILE: &str = "account.json";
pub const STATE_FILE: &str = "state.json";
/// What the `run` loop is doing ([`LoopStatus`]), next to `state.json`.
pub const STATUS_FILE: &str = "status.json";
/// An approval handed to the running loop ([`Approval`]).
pub const APPROVAL_FILE: &str = "approval.json";
/// The deletions `status` last listed for approval while the loop waited ([`Shown`]).
pub const SHOWN_FILE: &str = "shown.json";
/// The format of `shown.json` this agent writes and the newest it reads.
pub const SHOWN_SCHEMA: u32 = 1;
/// Temporary files of the data folder younger than this are left alone by [`DataDir::clean_tmp`]: another process
/// (`status`, `sync --approve-deletes`) may be writing one right now.
pub const TEMP_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(60);
/// The format of `status.json` this agent writes and the newest it reads. Format 2 added the `signed_out` state,
/// format 3 `paused_by_user`.
pub const STATUS_SCHEMA: u32 = 3;
/// The format of `approval.json` this agent writes and the newest it reads.
pub const APPROVAL_SCHEMA: u32 = 2;
/// The format of `account.json` this agent writes and the newest it reads.
pub const ACCOUNT_SCHEMA: u32 = 1;
/// The format of `state.json` this agent writes and the newest it reads.
pub const STATE_SCHEMA: u32 = 1;
/// The environment variable that overrides the data folder (the `--data-dir` flag wins over it).
pub use craft_cloud_proto::local::DATA_DIR_ENV;
/// The OS's per-user data folder plus `craft-cloud-agent`; shared with `craft-cloud-app`, which finds the running
/// agent the same way.
pub use craft_cloud_proto::local::default_data_dir;

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("can't find your user data folder; pass --data-dir or set {DATA_DIR_ENV}")]
    NoDataDir,
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is damaged ({message}); move it away to start over")]
    Damaged { path: PathBuf, message: String },
    #[error("the folder path {0} isn't valid UTF-8, so it can't be saved in the account; pick a folder whose path is plain text")]
    FolderNotUtf8(PathBuf),
    #[error("{path} was written by a newer Craft Cloud agent (format {found}; this one reads up to {supported}); update this agent")]
    Newer { path: PathBuf, found: u32, supported: u32 },
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> AccountError + '_ {
    move |source| AccountError::Io { path: path.to_owned(), source }
}

/// A signed-in device and the folder it syncs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub schema: u32,
    pub server: ServerUrl,
    pub username: String,
    pub device: DeviceId,
    pub device_name: String,
    /// The synced folder (absolute).
    pub folder: PathBuf,
    /// The space and root the folder's marker names.
    pub space: SpaceId,
    pub root: NodeId,
    pub token: DeviceToken,
}

/// How a pass ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "message")]
pub enum Outcome {
    Done,
    /// The mass-delete guard held deletions back.
    NeedsApproval,
    Failed(String),
}

/// What the last pass did, for `status`. Paths are display strings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PassSummary {
    pub downloaded: usize,
    pub uploaded: usize,
    pub removed_locally: usize,
    pub removed_remotely: usize,
    pub moved_locally: usize,
    pub moved_remotely: usize,
    pub created_dirs: usize,
    pub conflict_copies: Vec<String>,
    pub refused: usize,
    /// Entries that didn't sync, with the reason.
    pub skipped: Vec<(String, String)>,
    /// Deletions on the server waiting for approval.
    pub pending_deletes: Vec<String>,
    /// Deletions here waiting for approval.
    pub pending_local_deletes: Vec<String>,
    /// The nodes behind both pending lists: `sync --approve-deletes` releases exactly these.
    pub pending_nodes: Vec<NodeId>,
    /// Server deletes held behind a link: the node's path and the paths of all its links. Each is held until the
    /// user removes or replaces its links (see the engine's `State::held_deletes`), or approves the held set by
    /// naming it (`sync --approve-deletes=<set>`, [`PassSummary::held_set`]). A summary an older agent saved has one
    /// link per hold (a string) and loads as a one-element list.
    #[serde(deserialize_with = "held_deletes_any_form")]
    pub held_deletes: Vec<(String, Vec<String>)>,
    /// The nodes behind `held_deletes`, in the same order (none in a summary an older agent saved).
    pub held_nodes: Vec<NodeId>,
}

impl PassSummary {
    pub fn from_report(report: &Report) -> Self {
        let paths = |list: &[craft_cloud_sync::RelPath]| list.iter().map(ToString::to_string).collect();
        Self {
            downloaded: report.downloaded.len(),
            uploaded: report.uploaded.len(),
            removed_locally: report.removed_locally.len(),
            removed_remotely: report.removed_remotely.len(),
            moved_locally: report.moved_locally.len(),
            moved_remotely: report.moved_remotely.len(),
            created_dirs: report.created_dirs.len(),
            conflict_copies: paths(&report.conflict_copies),
            refused: report.refused.len(),
            skipped: report.skipped.clone(),
            pending_deletes: paths(&report.pending_deletes),
            pending_local_deletes: paths(&report.pending_local_deletes),
            pending_nodes: report.pending_nodes.clone(),
            held_deletes: report
                .held_deletes
                .iter()
                .map(|(path, links)| (path.to_string(), links.iter().map(ToString::to_string).collect()))
                .collect(),
            held_nodes: report.held_nodes.clone(),
        }
    }

    /// Only the deletions waiting for approval (paths and nodes) and the held ones: what a failed pass keeps from
    /// the pass before it, because the user was shown those lists and `--approve-deletes` still means exactly them
    /// (and a failed pass releases no hold).
    pub fn pending_only(&self) -> Self {
        Self {
            pending_deletes: self.pending_deletes.clone(),
            pending_local_deletes: self.pending_local_deletes.clone(),
            pending_nodes: self.pending_nodes.clone(),
            held_deletes: self.held_deletes.clone(),
            held_nodes: self.held_nodes.clone(),
            ..Self::default()
        }
    }

    pub fn needs_approval(&self) -> bool {
        !self.pending_deletes.is_empty() || !self.pending_local_deletes.is_empty()
    }

    /// Whether anything can be approved: deletions the mass-delete guard holds back, or deletes held behind a link
    /// (released only by naming their set).
    pub fn approvable(&self) -> bool {
        self.needs_approval() || self.held_set().is_some()
    }

    /// A short identifier of exactly the set of deletes held behind a link (their nodes, paths and every one of
    /// their links), apart from [`PassSummary::pending_set`]: `sync --approve-deletes=<set>` naming it lets those
    /// deletes go. A set computed over fewer links than a hold has is another set, so it can't release it. The bare
    /// `--approve-deletes` never does: a hold is released only when the user names it. `None` when nothing is held
    /// (or an older agent saved the summary without the nodes).
    pub fn held_set(&self) -> Option<String> {
        if self.held_nodes.is_empty() || self.held_nodes.len() != self.held_deletes.len() {
            return None;
        }
        let mut items: Vec<String> = self
            .held_nodes
            .iter()
            .zip(&self.held_deletes)
            .map(|(node, (path, links))| {
                let mut links = links.clone();
                links.sort();
                format!("{node}\0{path}\0{}", links.join("\0"))
            })
            .collect();
        items.sort();
        let mut hasher = blake3::Hasher::new();
        for item in items {
            hasher.update(b"held\0");
            hasher.update(item.as_bytes());
            hasher.update(&[0]);
        }
        Some(hasher.finalize().to_hex().get(..12).unwrap_or_default().to_owned())
    }

    /// A short identifier of exactly the set of deletions waiting for approval (its nodes and paths, in any order),
    /// so an approval can name the set the user saw. `None` when nothing waits.
    pub fn pending_set(&self) -> Option<String> {
        if !self.needs_approval() {
            return None;
        }
        let mut nodes: Vec<String> = self.pending_nodes.iter().map(ToString::to_string).collect();
        nodes.sort();
        let mut there = self.pending_deletes.clone();
        there.sort();
        let mut here = self.pending_local_deletes.clone();
        here.sort();
        let mut hasher = blake3::Hasher::new();
        for (kind, list) in [("node", &nodes), ("server", &there), ("here", &here)] {
            for item in list {
                hasher.update(kind.as_bytes());
                hasher.update(&[0]);
                hasher.update(item.as_bytes());
                hasher.update(&[0]);
            }
        }
        Some(hasher.finalize().to_hex().get(..12).unwrap_or_default().to_owned())
    }
}

/// `PassSummary::held_deletes` in either form: every link of a hold (now), or the one link an older agent saved.
fn held_deletes_any_form<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Vec<(String, Vec<String>)>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Links {
        All(Vec<String>),
        One(String),
    }
    let held: Vec<(String, Links)> = Deserialize::deserialize(de)?;
    Ok(held
        .into_iter()
        .map(|(path, links)| match links {
            Links::All(all) => (path, all),
            Links::One(one) => (path, vec![one]),
        })
        .collect())
}

/// The last pass: when it ended, how, and (if it finished) what it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastPass {
    /// Milliseconds since the Unix epoch.
    pub finished_ms: i64,
    pub outcome: Outcome,
    #[serde(default)]
    pub summary: Option<PassSummary>,
}

/// `state.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedState {
    pub schema: u32,
    pub state: State,
    #[serde(default)]
    pub last_pass: Option<LastPass>,
    /// Syncing is paused by the user (the local API's `pause`) until resumed, also after a restart. Written by the
    /// `run` loop only while no pass runs (a pass writes `false`: passes run only when not paused).
    #[serde(default)]
    pub paused: bool,
}

/// `status.json`: what the `run` loop is doing, written atomically on every change and as a heartbeat, for `status`
/// to read (the loop holds the locks, so `status` can't ask it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopStatus {
    pub schema: u32,
    /// The loop's process.
    pub pid: u32,
    /// When it started, milliseconds since the Unix epoch.
    pub started_ms: i64,
    /// When it last wrote this file.
    pub heartbeat_ms: i64,
    /// How often it writes this file while running, so a reader can tell a stale one (the loop was killed).
    pub heartbeat_every_ms: u64,
    pub state: LoopState,
    /// Deletions the mass-delete guard held back wait for `sync --approve-deletes` (the last pass listed them).
    /// Nothing else syncs any slower meanwhile.
    #[serde(default)]
    pub waiting_for_approval: bool,
    /// Deletes are held behind a link (the last pass listed them): released when their links go, or by
    /// `sync --approve-deletes=<set>` naming their set. A hold pauses nothing and may stand for days; it never sets
    /// `waiting_for_approval`.
    #[serde(default)]
    pub held: bool,
    /// The last pass the loop ran: when, how it ended and what it did.
    #[serde(default)]
    pub last_pass: Option<LastPass>,
    /// Why the folder isn't watched, while it isn't (the watcher couldn't start): changes there are then found by
    /// a full pass every minute.
    #[serde(default)]
    pub not_watching: Option<String>,
}

impl LoopStatus {
    /// Whether the loop that wrote this is running, as of `now_ms`: it hasn't said it stopped, and its heartbeat is
    /// fresh (at most three beats and a few seconds old; a killed loop stops writing it).
    pub fn running(&self, now_ms: i64) -> bool {
        let stale_after = i64::try_from(self.heartbeat_every_ms.saturating_mul(3)).unwrap_or(i64::MAX).saturating_add(5_000);
        self.state != LoopState::Stopped && now_ms.saturating_sub(self.heartbeat_ms) <= stale_after
    }
}

/// The `run` loop's state in `status.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LoopState {
    /// Waiting for something to sync.
    Idle,
    /// A pass is running.
    Syncing,
    /// The last pass failed for a reason that may pass by itself; the next try is at `next_try_ms`.
    Retrying { next_try_ms: i64, message: String },
    /// Stopped until the user does something; looked at again by itself at `next_check_ms`, if set.
    Paused { reason: String, next_check_ms: Option<i64> },
    /// Stopping after the running pass.
    Stopping,
    /// The loop ended.
    Stopped,
    /// Running, but not signed in: waiting for a sign-in through the local API (format 2).
    SignedOut,
    /// Signed in, with syncing paused by the user until resumed (format 3).
    PausedByUser,
}

/// `approval.json`: the waiting deletions `sync --approve-deletes` approved while the loop held the locks, as the set
/// `status` showed ([`PassSummary::pending_set`]). The loop takes it and releases exactly these, and only if they are
/// still the set that waits; otherwise it discards it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub schema: u32,
    /// The set's identifier (an approval from an older agent has none, and matches no set).
    #[serde(default)]
    pub set: String,
    pub nodes: Vec<NodeId>,
    pub given_ms: i64,
}

/// `shown.json`: the waiting deletions `status` last listed while the loop held the locks, so
/// `sync --approve-deletes` approves exactly what was shown (and refuses a set that changed since).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shown {
    pub schema: u32,
    pub set: String,
    pub shown_ms: i64,
}

/// Just the format number, read before the rest so a newer file is refused cleanly.
#[derive(Deserialize)]
struct Schema {
    schema: Option<u32>,
}

/// The agent's data folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir(PathBuf);

impl DataDir {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// `--data-dir` if given, else `$CRAFT_CLOUD_AGENT_DIR`, else the OS's per-user data folder.
    pub fn resolve(flag: Option<PathBuf>) -> Result<Self, AccountError> {
        flag.or_else(|| craft_cloud_proto::local::data_dir(|name| std::env::var_os(name))).map(Self).ok_or(AccountError::NoDataDir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Creates the data folder (owner-only on Unix) if it doesn't exist. Folders above it that are missing (a fresh
    /// `~/.local/share`, say) are made as any program would make them; only the data folder itself is owner-only.
    pub fn create(&self) -> Result<(), AccountError> {
        if self.0.is_dir() {
            return Ok(());
        }
        if let Some(parent) = self.0.parent() {
            fs::create_dir_all(parent).map_err(io_at(parent))?;
        }
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder
        };
        #[cfg(not(unix))]
        let builder = fs::DirBuilder::new();
        match builder.create(&self.0) {
            // Made meanwhile (by another agent starting at the same moment): there is nothing more to do.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && self.0.is_dir() => Ok(()),
            other => other.map_err(io_at(&self.0)),
        }
    }

    pub fn load_account(&self) -> Result<Option<Account>, AccountError> {
        load(&self.file(ACCOUNT_FILE), ACCOUNT_SCHEMA)
    }

    pub fn save_account(&self, account: &Account) -> Result<(), AccountError> {
        if account.folder.to_str().is_none() {
            return Err(AccountError::FolderNotUtf8(account.folder.clone()));
        }
        self.create()?;
        save(&self.file(ACCOUNT_FILE), account)
    }

    pub fn remove_account(&self) -> Result<(), AccountError> {
        remove(&self.file(ACCOUNT_FILE))
    }

    /// The saved state, or a fresh one (a first sync) if there is none.
    pub fn load_state(&self) -> Result<SavedState, AccountError> {
        Ok(load(&self.file(STATE_FILE), STATE_SCHEMA)?.unwrap_or(SavedState { schema: STATE_SCHEMA, ..SavedState::default() }))
    }

    pub fn save_state(&self, saved: &SavedState) -> Result<(), AccountError> {
        self.create()?;
        save(&self.file(STATE_FILE), saved)
    }

    pub fn remove_state(&self) -> Result<(), AccountError> {
        remove(&self.file(STATE_FILE))
    }

    /// Removes the temporary files a crash left while `account.json`, `state.json`, `status.json`, `approval.json` or
    /// `shown.json` was written (`.state.json.<random>.tmp`), and nothing else. Call it while holding the data
    /// folder's lock. Only those at least [`TEMP_MIN_AGE`] old go: `status` and `sync --approve-deletes` write files
    /// here without the lock (while a loop holds it), and one may be writing right now. Returns how many it removed.
    pub fn clean_tmp(&self) -> Result<usize, AccountError> {
        remove_atomic_temps(&self.0, |of| [ACCOUNT_FILE, STATE_FILE, STATUS_FILE, APPROVAL_FILE, SHOWN_FILE].contains(&of), TEMP_MIN_AGE)
            .map_err(io_at(&self.0))
    }

    /// What the `run` loop last wrote about itself, if it ever ran here.
    pub fn load_status(&self) -> Result<Option<LoopStatus>, AccountError> {
        load(&self.file(STATUS_FILE), STATUS_SCHEMA)
    }

    /// Writes the `run` loop's status (atomically, owner-only).
    pub fn save_status(&self, status: &LoopStatus) -> Result<(), AccountError> {
        save(&self.file(STATUS_FILE), status)
    }

    /// Hands an approval of the waiting set `set` (its deletions `nodes`) to the running loop, in place of any it
    /// hasn't taken yet: an approval names one set, the one shown.
    pub fn give_approval(&self, set: &str, nodes: &[NodeId], now_ms: i64) -> Result<(), AccountError> {
        save(&self.file(APPROVAL_FILE), &Approval { schema: APPROVAL_SCHEMA, set: set.to_owned(), nodes: nodes.to_vec(), given_ms: now_ms })
    }

    /// Remembers that `status` listed the waiting set `set`.
    pub fn save_shown(&self, set: &str, now_ms: i64) -> Result<(), AccountError> {
        save(&self.file(SHOWN_FILE), &Shown { schema: SHOWN_SCHEMA, set: set.to_owned(), shown_ms: now_ms })
    }

    /// The waiting set `status` last listed, if any.
    pub fn load_shown(&self) -> Result<Option<Shown>, AccountError> {
        load(&self.file(SHOWN_FILE), SHOWN_SCHEMA)
    }

    /// Takes the approval `sync --approve-deletes` left for the running loop, if any. It is first renamed to a
    /// temporary name, so an approval given while this one is read lands as a new file and is taken next time; a
    /// damaged one is removed and reported.
    pub fn take_approval(&self) -> Result<Option<Approval>, AccountError> {
        let path = self.file(APPROVAL_FILE);
        let taken = self.file(&format!(".{APPROVAL_FILE}.{}.tmp", random_hex(8).map_err(io_at(&self.0))?));
        match fs::rename(&path, &taken) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_at(&path)(e)),
        }
        let approval = load::<Approval>(&taken, APPROVAL_SCHEMA);
        // (A name of its own already: deleted as it is.)
        match fs::remove_file(&taken) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(io_at(&taken)(e)),
            _ => {}
        }
        approval
    }
}

fn load<T: DeserializeOwned>(path: &Path, supported: u32) -> Result<Option<T>, AccountError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_at(path)(e)),
    };
    let damaged = |e: serde_json::Error| AccountError::Damaged { path: path.to_owned(), message: e.to_string() };
    let schema: Schema = serde_json::from_slice(&bytes).map_err(damaged)?;
    match schema.schema {
        Some(found) if found > supported => Err(AccountError::Newer { path: path.to_owned(), found, supported }),
        Some(_) => serde_json::from_slice(&bytes).map(Some).map_err(damaged),
        None => Err(AccountError::Damaged { path: path.to_owned(), message: "no format number".into() }),
    }
}

fn save<T: Serialize>(path: &Path, value: &T) -> Result<(), AccountError> {
    let json = serde_json::to_vec_pretty(value).map_err(|e| AccountError::Damaged { path: path.to_owned(), message: e.to_string() })?;
    write_atomic(path, &json, true).map_err(io_at(path))
}

/// Deletes a file of the data folder (absent is fine), freeing its name at once ([`retire`]): `state.json` is written
/// again soon after a sign-in removes it.
fn remove(path: &Path) -> Result<(), AccountError> {
    retire(path).map(drop).map_err(io_at(path))
}

/// The file a temporary name of [`write_atomic`] is for: `Some("state.json")` for exactly
/// `.state.json.<16 lowercase hex digits>.tmp`, `None` for any other name.
pub(crate) fn atomic_temp_of(name: &str) -> Option<&str> {
    let (of, hex) = name.strip_prefix('.')?.strip_suffix(".tmp")?.rsplit_once('.')?;
    let random = hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (random && !of.is_empty()).then_some(of)
}

/// Removes the temporary files a [`write_atomic`] cut short by a crash left in `dir` (regular files only, and only for
/// files `ours` accepts by name). Returns how many it removed; a missing `dir` has none.
pub(crate) fn remove_atomic_temps(dir: &Path, ours: impl Fn(&str) -> bool, min_age: std::time::Duration) -> io::Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let stray = entry.file_name().to_str().and_then(atomic_temp_of).is_some_and(&ours);
        // A young one may be a write under way (another process writing it now), unless any age will do.
        let old_enough =
            || min_age.is_zero() || entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age >= min_age);
        if stray && entry.file_type().is_ok_and(|t| t.is_file()) && old_enough() {
            fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// `n` random bytes as lowercase hex: the random part of temporary names (here and on the synced disk).
pub(crate) fn random_hex(n: usize) -> io::Result<String> {
    let mut random = vec![0u8; n];
    getrandom::fill(&mut random).map_err(|e| io::Error::other(format!("the system random source failed: {e}")))?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

/// Writes `bytes` to `path` atomically: a temporary file next to it, flushed, then renamed over it. `private`
/// makes the file owner-only on Unix (mode 0600 from creation, so it is never readable by others).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    write_atomic_as(path, bytes, private, true)
}

/// [`write_atomic`], except that another program holding `path` refuses it at once ([`is_busy`]) rather than being
/// waited for: for a write that can as well be made later.
pub(crate) fn write_atomic_unless_busy(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    write_atomic_as(path, bytes, private, false)
}

fn write_atomic_as(path: &Path, bytes: &[u8], private: bool, wait: bool) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent folder"))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = dir.join(format!(".{name}.{}.tmp", random_hex(8)?));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if private { 0o600 } else { 0o666 });
    }
    #[cfg(not(unix))]
    let _ = private;
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if wait { replace(&temp, path) } else { fs::rename(&temp, path) }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Renames `temp` over `path` ([`retry_busy`]: on Windows the rename is refused while another program holds `path`
/// open without delete sharing).
fn replace(temp: &Path, path: &Path) -> io::Result<()> {
    retry_busy(|| fs::rename(temp, path))
}

/// Whether `e` is Windows refusing a file operation because another program holds the file (ERROR_ACCESS_DENIED or
/// ERROR_SHARING_VIOLATION; [`retry_busy`] waits those out), or a store saying so (`ResourceBusy`).
pub(crate) fn is_busy(e: &io::Error) -> bool {
    (cfg!(windows) && e.raw_os_error().is_some_and(|code| BUSY.contains(&code))) || e.kind() == io::ErrorKind::ResourceBusy
}

/// ERROR_ACCESS_DENIED and ERROR_SHARING_VIOLATION.
const BUSY: [i32; 2] = [5, 32];

/// Runs the file operation `op`. On Windows another program (a virus scanner, the search indexer, a backup tool)
/// holding the file open without sharing it for what `op` does makes it fail with ERROR_ACCESS_DENIED or
/// ERROR_SHARING_VIOLATION, usually for milliseconds: `op` is tried again for about a second and a half before
/// giving up. Elsewhere it runs once.
pub(crate) fn retry_busy<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    #[cfg(windows)]
    {
        let mut wait = std::time::Duration::from_millis(5);
        for _ in 0..12 {
            match op() {
                Err(e) if is_busy(&e) => {
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(std::time::Duration::from_millis(200));
                }
                other => return other,
            }
        }
    }
    op()
}

/// Deletes the file at `path`; `Ok(false)` if there was none. It is first renamed to a temporary name of its own
/// beside it (the one [`write_atomic`] uses, so the clean-ups of interrupted writes remove one a crash leaves), then
/// that is deleted: its own name is free at once, so a file written there next never meets a deleted one that
/// another program still holds open (which Windows may keep, "delete pending", refusing a new file of that name
/// with ERROR_ACCESS_DENIED until the last handle closes). A holder that doesn't share the file for deletion is
/// waited for ([`retry_busy`]). The caller makes the change durable (the folder's fsync) if it must be.
pub(crate) fn retire(path: &Path) -> io::Result<bool> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent folder"))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let gone = dir.join(format!(".{name}.{}.tmp", random_hex(8)?));
    match retry_busy(|| fs::rename(path, &gone)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    }
    // The name is free now; a renamed file that can't be deleted yet is only a temporary file left behind.
    let _ = retry_busy(|| fs::remove_file(&gone));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Another program holding the file for a moment (without delete sharing, as virus scanners do) delays the write;
    /// it doesn't fail it.
    #[cfg(windows)]
    #[test]
    fn a_file_held_open_for_a_moment_is_still_replaced() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        fs::write(&path, b"old").unwrap();
        // FILE_SHARE_READ only: renaming over the file is refused while this handle is open.
        let held = OpenOptions::new().read(true).share_mode(1).open(&path).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(held);
        });
        write_atomic(&path, b"new", false).unwrap();
        release.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    /// A file held open for good still fails the write, after the retries, and leaves no temporary file behind.
    #[cfg(windows)]
    #[test]
    fn a_file_held_open_for_good_fails_the_write_cleanly() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        fs::write(&path, b"old").unwrap();
        let _held = OpenOptions::new().read(true).share_mode(1).open(&path).unwrap();
        assert!(write_atomic(&path, b"new", false).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "the temporary file is removed");
    }

    /// Signing in again forgets the old sync state and writes a new one soon after: a scanner holding `state.json` for
    /// a moment (without delete sharing) delays the removal, it doesn't fail the sign-in, and the name is free for the
    /// new state at once.
    #[cfg(windows)]
    #[test]
    fn a_state_held_for_a_moment_is_still_removed_and_written_again() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path().join("data"));
        data.save_state(&SavedState { schema: STATE_SCHEMA, ..SavedState::default() }).unwrap();
        let path = data.file(STATE_FILE);
        // FILE_SHARE_READ | FILE_SHARE_WRITE: neither deleting nor renaming it works while this is open.
        let held = OpenOptions::new().read(true).share_mode(1 | 2).open(&path).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(held);
        });
        data.remove_state().unwrap();
        release.join().unwrap();
        assert!(!path.exists());
        data.save_state(&SavedState { schema: STATE_SCHEMA, ..SavedState::default() }).unwrap();
        assert!(path.exists());
    }

    fn account(folder: &Path) -> Account {
        Account {
            schema: ACCOUNT_SCHEMA,
            server: ServerUrl::parse("https://cloud.example.com").unwrap(),
            username: "ana".into(),
            device: DeviceId::generate(),
            device_name: "laptop".into(),
            folder: folder.to_owned(),
            space: SpaceId::generate(),
            root: NodeId::generate(),
            token: DeviceToken::new("tok".into()),
        }
    }

    #[test]
    fn accounts_round_trip_and_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path().join("data"));
        assert_eq!(data.load_account().unwrap(), None);
        let acc = account(dir.path());
        data.save_account(&acc).unwrap();
        assert_eq!(data.load_account().unwrap(), Some(acc));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.path().join("data/account.json")), 0o600);
            assert_eq!(mode(&dir.path().join("data")), 0o700);
        }
        data.remove_account().unwrap();
        data.remove_account().unwrap();
        assert_eq!(data.load_account().unwrap(), None);
        // No temporary files left behind.
        assert_eq!(fs::read_dir(data.path()).unwrap().count(), 0);
    }

    /// Only the data folder is owner-only: missing folders above it are made as any program makes them.
    #[cfg(unix)]
    #[test]
    fn only_the_data_folder_itself_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path().join("share/craft-cloud"));
        data.create().unwrap();
        data.create().unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("share/craft-cloud")), 0o700);
        // As any program makes a folder (whatever the user's umask says).
        fs::create_dir(dir.path().join("plain")).unwrap();
        assert_eq!(mode(&dir.path().join("share")), mode(&dir.path().join("plain")), "a missing parent isn't made owner-only");
    }

    /// Only the exact temporary names `write_atomic` uses, for the data folder's own two files, are cleaned up.
    #[test]
    fn interrupted_atomic_writes_are_cleaned_and_nothing_else() {
        assert_eq!(atomic_temp_of(".state.json.0123456789abcdef.tmp"), Some("state.json"));
        for other in [
            "state.json.0123456789abcdef.tmp",
            ".state.json.0123456789abcde.tmp",
            ".state.json.0123456789ABCDEF.tmp",
            ".state.json.tmp",
            "..0123456789abcdef.tmp",
            ".x.0123456789abcdef.part",
        ] {
            assert_eq!(atomic_temp_of(other), None, "{other}");
        }
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path().join("data"));
        assert_eq!(data.clean_tmp().unwrap(), 0, "no data folder yet");
        data.save_state(&SavedState { schema: STATE_SCHEMA, ..SavedState::default() }).unwrap();
        // A write under way right now (`status` or `sync --approve-deletes` from another terminal) is young: kept.
        let keep = [".notes.json.0123456789abcdef.tmp", ".state.json.tmp", "mine.tmp", ".approval.json.00000000000000ff.tmp"];
        let a_while_ago = std::time::SystemTime::now() - TEMP_MIN_AGE - std::time::Duration::from_secs(1);
        for name in [
            ".state.json.0123456789abcdef.tmp",
            ".account.json.fedcba9876543210.tmp",
            ".status.json.0123456789abcdef.tmp",
            ".approval.json.0123456789abcdef.tmp",
            ".shown.json.0123456789abcdef.tmp",
        ]
        .iter()
        .chain(&keep)
        {
            let path = data.path().join(name);
            fs::write(&path, b"").unwrap();
            if !name.contains("00000000000000ff") {
                fs::File::options().write(true).open(&path).unwrap().set_modified(a_while_ago).unwrap();
            }
        }
        assert_eq!(data.clean_tmp().unwrap(), 5);
        let mut left: Vec<String> = fs::read_dir(data.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        let mut want: Vec<String> = keep.iter().map(|s| (*s).to_owned()).chain([STATE_FILE.to_owned()]).collect();
        want.sort();
        assert_eq!(left, want);
    }

    #[test]
    fn the_loop_status_round_trips_and_goes_stale() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        assert_eq!(data.load_status().unwrap(), None);
        let status = LoopStatus {
            schema: STATUS_SCHEMA,
            pid: 7,
            started_ms: 1_000,
            heartbeat_ms: 100_000,
            heartbeat_every_ms: 15_000,
            state: LoopState::Retrying { next_try_ms: 104_000, message: "offline".into() },
            waiting_for_approval: true,
            held: true,
            last_pass: Some(LastPass { finished_ms: 99_000, outcome: Outcome::Failed("offline".into()), summary: None }),
            not_watching: Some("too many folders to watch".into()),
        };
        data.save_status(&status).unwrap();
        assert_eq!(data.load_status().unwrap(), Some(status.clone()));
        assert!(status.running(100_000 + 50_000), "three beats and five seconds");
        assert!(!status.running(100_000 + 50_001), "a killed loop's heartbeat goes stale");
        assert!(!LoopStatus { state: LoopState::Stopped, ..status }.running(100_000), "a loop that said it stopped");
    }

    #[test]
    fn an_approval_names_one_set_and_is_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        assert_eq!(data.take_approval().unwrap(), None);
        let (a, b) = (NodeId::generate(), NodeId::generate());
        data.give_approval("set-1", &[a], 1).unwrap();
        data.give_approval("set-2", &[a, b], 2).unwrap();
        let taken = data.take_approval().unwrap().unwrap();
        assert_eq!((taken.set.as_str(), taken.nodes, taken.given_ms), ("set-2", vec![a, b], 2), "the last one given");
        assert_eq!(data.take_approval().unwrap(), None, "taken once");
        assert_eq!(fs::read_dir(data.path()).unwrap().count(), 0, "nothing left behind");
        fs::write(dir.path().join(APPROVAL_FILE), "{damaged").unwrap();
        assert!(matches!(data.take_approval(), Err(AccountError::Damaged { .. })));
        assert_eq!(data.take_approval().unwrap(), None, "a damaged one is removed");
    }

    #[test]
    fn a_pending_set_is_named_by_exactly_its_deletions() {
        let (a, b) = (NodeId::generate(), NodeId::generate());
        let summary = |nodes: Vec<NodeId>, paths: &[&str]| PassSummary {
            pending_deletes: paths.iter().map(|p| (*p).to_owned()).collect(),
            pending_nodes: nodes,
            ..PassSummary::default()
        };
        assert_eq!(PassSummary::default().pending_set(), None);
        let one = summary(vec![a, b], &["x.txt", "y.txt"]).pending_set().unwrap();
        assert_eq!(summary(vec![b, a], &["y.txt", "x.txt"]).pending_set().unwrap(), one, "order doesn't matter");
        assert_ne!(summary(vec![a], &["x.txt"]).pending_set().unwrap(), one);
        assert_ne!(summary(vec![a, b, NodeId::generate()], &["x.txt", "y.txt", "z.txt"]).pending_set().unwrap(), one);
        let here = PassSummary { pending_local_deletes: vec!["x.txt".into(), "y.txt".into()], pending_nodes: vec![a, b], ..PassSummary::default() };
        assert_ne!(here.pending_set().unwrap(), one, "a delete here isn't one on the server");
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        assert_eq!(data.load_shown().unwrap(), None);
        data.save_shown(&one, 5).unwrap();
        assert_eq!(data.load_shown().unwrap().map(|s| s.set), Some(one));
    }

    #[test]
    fn state_round_trips_and_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        let fresh = data.load_state().unwrap();
        assert_eq!(fresh.schema, STATE_SCHEMA);
        assert_eq!(fresh.state, State::default());
        let mut saved = fresh;
        saved.state.seq = 42;
        saved.state.space = Some(SpaceId::generate());
        saved.last_pass = Some(LastPass { finished_ms: 7, outcome: Outcome::Failed("x".into()), summary: None });
        data.save_state(&saved).unwrap();
        assert_eq!(data.load_state().unwrap(), saved);
    }

    /// Held deletes an older agent saved are still read, as the link's path, and keep holding: fix round 3 wrote the
    /// bare path, fix rounds 4 and 5 a map with the link's folder ID and name as well (and `fresh_dirs`, no longer
    /// kept). The current record (every candidate link) and the links the last finished push saw round-trip.
    #[test]
    fn a_held_delete_saved_by_an_older_agent_is_kept() {
        use craft_cloud_sync::{HeldLink, RelPath};
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        let (bare, mapped) = (NodeId::generate(), NodeId::generate());
        let name = |s: &str| craft_cloud_proto::Name::new(s).unwrap();
        let link = RelPath::from_names(vec![name("Clips"), name("c0.mov")]);
        let other = RelPath::from_names(vec![name("Clips"), name("c1.mov")]);
        let mut state = serde_json::to_value(State::default()).unwrap();
        state["held_deletes"] = serde_json::json!({
            bare.to_string(): ["Clips", "c0.mov"],
            mapped.to_string(): { "dir": 7, "name": "c1.mov", "path": ["Clips", "c1.mov"] },
        });
        state["fresh_dirs"] = serde_json::json!([NodeId::generate().to_string()]);
        fs::write(dir.path().join(STATE_FILE), serde_json::json!({ "schema": 1, "state": state }).to_string()).unwrap();
        let mut saved = data.load_state().unwrap();
        assert_eq!(saved.state.held_deletes.get(&bare), Some(&HeldLink::at(link.clone())));
        assert_eq!(saved.state.held_deletes.get(&mapped), Some(&HeldLink::at(other)));
        assert!(saved.state.known_links.is_empty(), "an older agent recorded none: every link counts as new once");
        saved.state.known_links.insert(link.clone());
        let both = HeldLink { paths: [link, RelPath::from_names(vec![name("A"), name("c0.mov")])].into() };
        saved.state.held_deletes.insert(NodeId::generate(), both);
        data.save_state(&saved).unwrap();
        assert_eq!(data.load_state().unwrap(), saved);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_path_that_is_not_utf8_gets_a_clear_error() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        let mut account = account(dir.path());
        account.folder = PathBuf::from(std::ffi::OsStr::from_bytes(b"/bad\xff"));
        let err = data.save_account(&account).unwrap_err();
        assert!(matches!(err, AccountError::FolderNotUtf8(_)));
        assert!(err.to_string().contains("valid UTF-8"), "{err}");
        assert!(data.load_account().unwrap().is_none(), "nothing was written");
    }

    #[test]
    fn newer_or_damaged_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        fs::write(dir.path().join(STATE_FILE), r#"{"schema": 2, "state": "something new"}"#).unwrap();
        assert!(matches!(data.load_state(), Err(AccountError::Newer { found: 2, supported: 1, .. })));
        fs::write(dir.path().join(STATE_FILE), "{not json").unwrap();
        assert!(matches!(data.load_state(), Err(AccountError::Damaged { .. })));
        fs::write(dir.path().join(STATE_FILE), r#"{"state": {}}"#).unwrap();
        assert!(matches!(data.load_state(), Err(AccountError::Damaged { .. })));
        // Fields this agent doesn't know (from a minor addition) are ignored; missing optional ones default.
        let state = serde_json::to_value(State::default()).unwrap();
        fs::write(dir.path().join(STATE_FILE), serde_json::json!({ "schema": 1, "state": state, "extra": true }).to_string()).unwrap();
        assert_eq!(data.load_state().unwrap().last_pass, None);
    }

    #[test]
    fn the_token_is_saved_but_never_in_debug_output() {
        let dir = tempfile::tempdir().unwrap();
        let acc = Account { token: DeviceToken::new("very-secret".into()), ..account(dir.path()) };
        assert!(!format!("{acc:?}").contains("very-secret"));
        assert!(serde_json::to_string(&acc).unwrap().contains("very-secret"));
    }

    #[test]
    fn the_default_data_dir_follows_the_platform() {
        use std::ffi::OsString;
        let env = |name: &str| match name {
            "HOME" => Some(OsString::from("/home/ana")),
            "XDG_DATA_HOME" => Some(OsString::from("relative/ignored")),
            "LOCALAPPDATA" => Some(OsString::from(r"C:\Users\ana\AppData\Local")),
            _ => None,
        };
        let dir = default_data_dir(env).unwrap();
        assert!(dir.ends_with("craft-cloud-agent"));
        if cfg!(windows) {
            assert!(dir.starts_with(r"C:\Users\ana\AppData\Local"));
        } else if cfg!(target_vendor = "apple") {
            assert_eq!(dir, Path::new("/home/ana/Library/Application Support/craft-cloud-agent"));
        } else {
            assert_eq!(dir, Path::new("/home/ana/.local/share/craft-cloud-agent"));
        }
        assert_eq!(default_data_dir(|_| None), None);
    }
}
