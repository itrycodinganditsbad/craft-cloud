//! What the agent keeps between runs, in its data folder (one synced folder per data folder):
//!
//! - `account.json`: the server, user, device, device name, synced folder and the device token. Written
//!   owner-only: mode 0600 on Unix (the data folder itself is 0700). Windows has no mode bits; the default data
//!   folder is in the user's profile (`%LOCALAPPDATA%`), which only that user (and administrators) can read. A
//!   `--data-dir` outside the profile on Windows gets that folder's ACL, which may let others read the token:
//!   keep it in the profile. An OS keychain can come later.
//! - `state.json`: `{ "schema": 1, "state": <the engine's State>, "last_pass": … }`, written after every pass,
//!   failed or not. One document, not a table per node: the engine works on the whole state in memory anyway.
//!
//! Both are written atomically (temporary file, flush, rename), so a crash leaves the old file or the new one.
//! A crash loses at most one pass of bookkeeping, which the next pass redoes. A file written by a newer agent
//! (a higher `schema`) is refused, never half-read.

use std::ffi::OsString;
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
/// The format of `account.json` this agent writes and the newest it reads.
pub const ACCOUNT_SCHEMA: u32 = 1;
/// The format of `state.json` this agent writes and the newest it reads.
pub const STATE_SCHEMA: u32 = 1;
/// The environment variable that overrides the data folder (the `--data-dir` flag wins over it).
pub const DATA_DIR_ENV: &str = "CRAFT_CLOUD_AGENT_DIR";
/// The data folder's name inside the OS's per-user data folder.
const APP_DIR: &str = "craft-cloud-agent";

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("can't find your user data folder; pass --data-dir or set {DATA_DIR_ENV}")]
    NoDataDir,
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is damaged ({message}); move it away to start over")]
    Damaged { path: PathBuf, message: String },
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
        }
    }

    pub fn needs_approval(&self) -> bool {
        !self.pending_deletes.is_empty() || !self.pending_local_deletes.is_empty()
    }
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
        flag.or_else(|| std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()).map(PathBuf::from))
            .or_else(|| default_data_dir(|name| std::env::var_os(name)))
            .map(Self)
            .ok_or(AccountError::NoDataDir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Creates the data folder (owner-only on Unix) if it doesn't exist.
    pub fn create(&self) -> Result<(), AccountError> {
        if self.0.is_dir() {
            return Ok(());
        }
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.0).map_err(io_at(&self.0))
    }

    pub fn load_account(&self) -> Result<Option<Account>, AccountError> {
        load(&self.file(ACCOUNT_FILE), ACCOUNT_SCHEMA)
    }

    pub fn save_account(&self, account: &Account) -> Result<(), AccountError> {
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
}

/// The OS's per-user data folder plus `craft-cloud-agent`: `%LOCALAPPDATA%` on Windows (machine-local: the
/// state describes this machine's disk, so it must not roam), `~/Library/Application Support` on macOS,
/// `$XDG_DATA_HOME` or `~/.local/share` elsewhere. `env` reads an environment variable. Written out rather than
/// taken from a crate: the usual ones depend on an MPL-licensed helper.
pub fn default_data_dir(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let absolute = |name: &str| env(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let base = if cfg!(windows) {
        absolute("LOCALAPPDATA")
    } else if cfg!(target_vendor = "apple") {
        absolute("HOME").map(|home| home.join("Library").join("Application Support"))
    } else {
        absolute("XDG_DATA_HOME").or_else(|| absolute("HOME").map(|home| home.join(".local").join("share")))
    };
    base.map(|b| b.join(APP_DIR))
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

fn remove(path: &Path) -> Result<(), AccountError> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_at(path)(e)),
        _ => Ok(()),
    }
}

/// Writes `bytes` to `path` atomically: a temporary file next to it, flushed, then renamed over it. `private`
/// makes the file owner-only on Unix (mode 0600 from creation, so it is never readable by others).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent folder"))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|e| io::Error::other(format!("the system random source failed: {e}")))?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let temp = dir.join(format!(".{name}.{suffix}.tmp"));
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
        fs::rename(&temp, path)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let env = |name: &str| match name {
            "HOME" => Some(OsString::from("/home/ana")),
            "XDG_DATA_HOME" => Some(OsString::from("relative/ignored")),
            "LOCALAPPDATA" => Some(OsString::from(r"C:\Users\ana\AppData\Local")),
            _ => None,
        };
        let dir = default_data_dir(env).unwrap();
        assert!(dir.ends_with(APP_DIR));
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
