//! "Make a team space from this folder" (`POST /v1/spaces/from-folder`): a folder of the personal folder becomes a new team
//! space. The files are copied, never moved, and the old folder goes only when the team space is known to hold every file.
//!
//! The order is the safety argument. Each step either changes nothing that matters or can be repeated, and the old folder is
//! touched by the last step only:
//!
//! 1. **Checking** (in the call): the folder is inside the personal folder and is not it, is no link, holds no team folder, no
//!    `Craft Cloud Shared` folder, no other synced folder, no link or special file and no name the sync cannot carry.
//!    Nothing is written.
//! 2. **Creating** (in the call): the team space is made on the server, an administrator's call with the account as the first
//!    owner. A refusal (a wrong password, not an administrator, a name taken, a server without team spaces) is the call's
//!    error, and nothing exists. From here on the space exists; if the work stops it is reported, never deleted.
//! 3. **SettingUp**: the team folder `Craft Cloud Shared/<name>` is set up, its sync is turned on, and its passes are allowed to
//!    claim chunks from the personal space (`POST /chunks/claim`), so the bytes of the files are not sent a second time.
//!    A claim the server refuses (an older server, no read access) falls back to the ordinary upload.
//! 4. **Copying**: every file is read from the old folder once, chunked, and written into the team folder through a temporary
//!    file and a rename, so the team pass never sees half a file. A file that changes while it is read is read again.
//! 5. **Syncing**: the team space's tree is read from the server until it holds every copied file with the manifest the copy
//!    was chunked into (and every folder). That is the commit having succeeded, checked against the server and not against the
//!    pass's own report. A paused space, lost access or a stall ends the step with the reason.
//! 6. **Verifying**: the old folder is read once more and must still be exactly what was copied (same manifests, no new file).
//!    If it is not, the work goes back to copying, up to three rounds.
//! 7. **Trashing**: the old folder's files are deleted here one by one, each only if it is still as it was verified; what changed
//!    stays (and is counted in `kept`). The personal sync sees an ordinary deletion of a folder: it goes to the server's trash,
//!    where it can be restored, and the mass-delete guard sees it like any other.
//!
//! A journal (`from-folder.json` in the data folder) records the step and the space after every change of step. When the agent
//! stops in the middle, the next start reports the work as interrupted; asking again for the same folder and name goes on with the
//! same space and needs no confirmation. Every step is safe to repeat: the copy skips what is in place, the team folder and the
//! space are found again, and claims are idempotent.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use craft_cloud_chunker::{Chunker, FASTCDC_V1, chunk_reader};
use craft_cloud_proto::api::{AdminCreateSpace, AdminSpaceInfo};
use craft_cloud_proto::local::{FromFolderRequest, FromFolderStatus, FromFolderStep, SpaceRun, SpaceSyncRequest};
use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, FileHash, Manifest, ManifestId, Name, NodeId, SpaceId, SpaceName};
use craft_cloud_sync::Transport;
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::account::{Account, DataDir, random_hex, write_atomic};
use crate::cli::AgentError;
use crate::disk::AGENT_DIR;
use crate::http::{CallError, HttpTransport};
use crate::local_api::{Control, LocalError};
use crate::spaces::{SHARED_FOLDER, SpacesHandle};

/// The journal in the data folder.
pub const JOURNAL_FILE: &str = "from-folder.json";
const JOURNAL_SCHEMA: u32 = 1;
/// How many times the folder is copied again because it changed under the copy.
const ROUNDS: u32 = 3;
/// How long the team space may make no progress before the work stops and says so.
const STALL: Duration = Duration::from_secs(600);
/// How long the team space may take to show up in the agent's list.
const LISTED_WITHIN: Duration = Duration::from_secs(60);
/// The extension of the copy's temporary files in the team folder's `.craft-cloud/tmp/` (the engine's cleaning leaves it be).
const COPY_EXT: &str = "cc-copy";
/// How a problem is named in the list of entries that stop the work: at most this many.
const LISTED_PROBLEMS: usize = 5;
/// Waiting for a file that another program holds while it is put in place.
const RENAME_TRIES: u32 = 8;

#[derive(Serialize, Deserialize)]
struct Journal {
    schema: u32,
    /// The account's personal space: a journal of another account is not ours.
    personal: SpaceId,
    status: FromFolderStatus,
}

fn journal_path(data: &DataDir) -> PathBuf {
    data.path().join(JOURNAL_FILE)
}

fn read_journal(data: &DataDir, personal: SpaceId) -> Option<FromFolderStatus> {
    let bytes = std::fs::read(journal_path(data)).ok()?;
    let journal: Journal = serde_json::from_slice(&bytes).ok()?;
    (journal.schema <= JOURNAL_SCHEMA && journal.personal == personal).then_some(journal.status)
}

fn write_journal(data: &DataDir, personal: SpaceId, status: &FromFolderStatus) {
    let journal = Journal { schema: JOURNAL_SCHEMA, personal, status: status.clone() };
    let result = serde_json::to_vec_pretty(&journal).map_err(io::Error::other).and_then(|json| write_atomic(&journal_path(data), &json, true));
    if let Err(e) = result {
        crate::runlog::note(format_args!("can't write the journal of making a team space from a folder: {e}"));
    }
}

/// Test hooks: stop at a step, or fail at it, for the agent whose data folder is named. Not used outside tests.
#[doc(hidden)]
pub mod hook {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Hook {
        /// Wait before the step until the hook is cleared (or the agent stops).
        Pause(FromFolderStep),
        /// Fail at the step.
        Fail(FromFolderStep),
    }

    static HOOKS: Mutex<Vec<(PathBuf, Hook)>> = Mutex::new(Vec::new());

    pub fn set(data: &Path, hook: Option<Hook>) {
        let mut hooks = HOOKS.lock().unwrap_or_else(PoisonError::into_inner);
        hooks.retain(|(path, _)| path != data);
        if let Some(hook) = hook {
            hooks.push((data.to_owned(), hook));
        }
    }

    pub(super) fn get(data: &Path) -> Option<Hook> {
        HOOKS.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(path, _)| path == data).map(|(_, hook)| *hook)
    }
}

/// Reads the journal at the start of an agent that is signed in: work that was under way is now interrupted. A journal of
/// another account is removed.
pub fn restore(control: &Control, data: &DataDir, account: &Account) {
    let path = journal_path(data);
    let Some(mut status) = read_journal(data, account.space) else {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        return;
    };
    if status.running() {
        status.failed_at = Some(status.step);
        status.message = Some(match status.step {
            FromFolderStep::Trashing => {
                "The agent stopped while the old folder was being deleted. Every file is in the team space; ask again to finish, and the \
                files still in the old folder are checked once more before they are deleted."
                    .to_owned()
            }
            _ => {
                "The agent stopped before this was finished. Your old folder was not changed; ask again to go on with the same team space.".to_owned()
            }
        });
        status.step = FromFolderStep::Interrupted;
        write_journal(data, account.space, &status);
    }
    control.set_from_folder(Some(status));
}

// ---------------------------------------------------------------------------------------------------------------------
// The call

fn usage(message: impl Into<String>) -> LocalError {
    AgentError::Usage(message.into()).into()
}

/// What the call needs of the world, from the local API.
pub(crate) struct Start {
    pub control: Control,
    pub data: DataDir,
}

/// `POST /v1/spaces/from-folder`: checks the folder, makes the team space and starts the rest of the work. The answer is the
/// status then.
pub(crate) async fn start(start: Start, request: FromFolderRequest) -> Result<FromFolderStatus, LocalError> {
    let Start { control, data } = start;
    let account = control.account().ok_or(AgentError::NotSignedIn)?;
    let spaces = control.spaces_handle().ok_or(AgentError::NotSignedIn)?;
    if control.from_folder().is_some_and(|s| s.running()) {
        return Err(LocalError::new(
            axum::http::StatusCode::CONFLICT,
            craft_cloud_proto::api::codes::CONFLICT,
            "another team space is being made from a folder; wait until it is done",
        ));
    }
    let name = SpaceName::new(&request.name).map_err(|e| usage(format!("that is not a name for a team space: {e}")))?;
    let folder = PathBuf::from(&request.folder);
    if !folder.is_absolute() {
        return Err(usage("the folder must be an absolute path"));
    }
    // Checked off the async threads: it reads the whole folder.
    let (checked_account, checked_folder) = (account.clone(), folder.clone());
    let scan = tokio::task::spawn_blocking(move || check_folder(&checked_account, &checked_folder))
        .await
        .map_err(|e| LocalError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, craft_cloud_proto::api::codes::INTERNAL, e.to_string()))??;
    let canonical = scan.root.clone();

    let before = read_journal(&data, account.space);
    let same = |s: &FromFolderStatus| s.folder == canonical.display().to_string() && s.name == name.as_str();
    // Asked again for the same folder and name: goes on with the space that was made, when there is one.
    let resumed = before.as_ref().filter(|s| same(s) && !s.running() && s.step != FromFolderStep::Done);
    let crashed_creating = resumed.is_some_and(|s| s.failed_at == Some(FromFolderStep::Creating) && s.space.is_none());
    let mut status = FromFolderStatus {
        folder: canonical.display().to_string(),
        name: name.as_str().to_owned(),
        step: FromFolderStep::Creating,
        space: resumed.and_then(|s| s.space),
        team_folder: resumed.and_then(|s| s.team_folder.clone()),
        done: 0,
        total: scan.files.len() as u64,
        failed_at: None,
        message: None,
        kept: 0,
    };
    if status.space.is_none() {
        let transport = HttpTransport::new(account.server.clone(), &account.token).map_err(AgentError::from)?;
        if request.confirm.password.is_none() {
            return Err(AgentError::PasswordRequired.into());
        }
        if !control.begin_from_folder(status.clone()) {
            return Err(LocalError::new(
                axum::http::StatusCode::CONFLICT,
                craft_cloud_proto::api::codes::CONFLICT,
                "another team space is being made from a folder",
            ));
        }
        write_journal(&data, account.space, &status);
        match create_space(&transport, &name, &request).await {
            Ok(created) => status.space = Some(created.id),
            // A crash between the server's answer and the journal left a space of this name: it is ours if it was just made
            // for this (it is the account's, owned by it, and empty).
            Err(e) if crashed_creating && is_name_taken(&e) => match find_empty_own_space(&transport, &name).await {
                Some(space) => status.space = Some(space),
                None => {
                    give_back(&control, &data, account.space, before);
                    return Err(e.into());
                }
            },
            Err(e) => {
                // Nothing exists: the status (and the journal) of an earlier try is as it was.
                give_back(&control, &data, account.space, before);
                return Err(e.into());
            }
        }
    } else if !control.begin_from_folder(status.clone()) {
        return Err(LocalError::new(
            axum::http::StatusCode::CONFLICT,
            craft_cloud_proto::api::codes::CONFLICT,
            "another team space is being made from a folder",
        ));
    }
    status.step = FromFolderStep::SettingUp;
    write_journal(&data, account.space, &status);
    control.set_from_folder(Some(status.clone()));
    let job = Job { control, data, account, spaces, status: status.clone(), scan };
    tokio::spawn(job.run());
    Ok(status)
}

/// Puts the status and the journal back to what an earlier try left (`None`: nothing).
fn give_back(control: &Control, data: &DataDir, personal: SpaceId, before: Option<FromFolderStatus>) {
    match &before {
        Some(status) => write_journal(data, personal, status),
        None => {
            let _ = std::fs::remove_file(journal_path(data));
        }
    }
    control.set_from_folder(before);
}

async fn create_space(transport: &HttpTransport, name: &SpaceName, request: &FromFolderRequest) -> Result<AdminSpaceInfo, CallError> {
    let me = transport.account_info().await?.user_id;
    let body = AdminCreateSpace { name: name.as_str().to_owned(), owner: me, quota_bytes: request.quota_bytes, confirm: request.confirm.clone() };
    transport.admin_json(Method::POST, "admin/spaces", &body).await
}

fn is_name_taken(e: &CallError) -> bool {
    matches!(e, CallError::Refused(refusal) if refusal.status == 409)
}

/// The team space called `name` that the account owns and that holds nothing yet: the one a crashed call made.
async fn find_empty_own_space(transport: &HttpTransport, name: &SpaceName) -> Option<SpaceId> {
    let list = transport.spaces().await.ok()?;
    let fold = name.fold_key();
    let item = list.spaces.into_iter().find(|i| {
        i.kind == "team"
            && i.state == "active"
            && i.role == craft_cloud_proto::api::SpaceRole::Owner
            && i.name.as_deref().and_then(|n| SpaceName::new(n).ok()).is_some_and(|n| n.fold_key() == fold)
    })?;
    // Only a space with nothing in it is adopted.
    let usage: craft_cloud_proto::api::SpaceUsage = transport.admin_get(&format!("spaces/{}/usage", item.id), &[]).await.ok()?;
    (usage.stored_bytes == 0).then_some(item.id)
}

// ---------------------------------------------------------------------------------------------------------------------
// Looking at the folder

/// A file of the old folder, as it was when it was looked at.
#[derive(Clone)]
struct FileEntry {
    /// The names as the folder has them (to find the file) and as the sync names them (NFC, to compare with the server).
    os: Vec<std::ffi::OsString>,
    names: Vec<String>,
    size: u64,
    modified: Option<SystemTime>,
}

struct Scan {
    root: PathBuf,
    dirs: Vec<(Vec<std::ffi::OsString>, Vec<String>)>,
    files: Vec<FileEntry>,
}

impl Scan {
    fn path_of(&self, os: &[std::ffi::OsString]) -> PathBuf {
        os.iter().fold(self.root.clone(), |p, n| p.join(n))
    }
}

/// Looks at the folder and refuses it for any reason that would make moving it wrong. Reads names and sizes only.
fn check_folder(account: &Account, folder: &Path) -> Result<Scan, AgentError> {
    let link = std::fs::symlink_metadata(folder).map_err(|_| AgentError::Usage(format!("{} doesn't exist", folder.display())))?;
    if link.file_type().is_symlink() || !link.is_dir() {
        return Err(AgentError::Usage(format!("{} is not a folder (a link is not followed)", folder.display())));
    }
    let root = std::fs::canonicalize(folder).map_err(|e| AgentError::Usage(format!("{} can't be opened: {e}", folder.display())))?;
    let personal = std::fs::canonicalize(&account.folder).map_err(|e| AgentError::Usage(format!("the personal folder can't be opened: {e}")))?;
    if root == personal {
        return Err(AgentError::Usage("this is the personal folder itself; choose a folder inside it".into()));
    }
    if !root.starts_with(&personal) {
        return Err(AgentError::Usage(format!("{} is not inside the personal folder {}", root.display(), personal.display())));
    }
    for setting in &account.spaces {
        if crate::cli::nested(&setting.folder, &root) {
            return Err(AgentError::Usage(format!(
                "{} is, or holds, the folder of a team space ({}); a folder that already is a team space can't be made into another",
                root.display(),
                setting.folder.display()
            )));
        }
    }
    let mut scan = Scan { root: root.clone(), dirs: Vec::new(), files: Vec::new() };
    let mut problems: Vec<String> = Vec::new();
    let mut problem = |what: String| {
        if problems.len() < LISTED_PROBLEMS {
            problems.push(what);
        }
        problems.len()
    };
    let mut stack: Vec<(Vec<std::ffi::OsString>, Vec<String>)> = vec![(Vec::new(), Vec::new())];
    let mut refused = false;
    while let Some((os, names)) = stack.pop() {
        let dir = scan.path_of(&os);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                problem(format!("{} can't be read: {e}", dir.display()));
                refused = true;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                problem(format!("{} can't be listed completely", dir.display()));
                refused = true;
                continue;
            };
            let os_name = entry.file_name();
            let shown = dir.join(&os_name);
            let Some(text) = os_name.to_str() else {
                problem(format!("{} has a name that is not text", shown.display()));
                refused = true;
                continue;
            };
            if text == AGENT_DIR {
                problem(format!("{} holds another synced folder (it has its own {AGENT_DIR} folder)", dir.display()));
                refused = true;
                continue;
            }
            // A name the sync never carries (a temporary file, an open document's lock file): not copied, and not deleted either.
            if ignored(text) {
                continue;
            }
            let Ok(name) = Name::new(text) else {
                problem(format!("{} has a name the sync can't carry", shown.display()));
                refused = true;
                continue;
            };
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) => {
                    problem(format!("{} can't be read: {e}", shown.display()));
                    refused = true;
                    continue;
                }
            };
            let mut child_os = os.clone();
            child_os.push(os_name.clone());
            let mut child_names = names.clone();
            child_names.push(name.as_str().to_owned());
            let kind = meta.file_type();
            if kind.is_symlink() {
                problem(format!("{} is a link; links are not copied", shown.display()));
                refused = true;
            } else if kind.is_dir() {
                if name.as_str().to_lowercase() == SHARED_FOLDER.to_lowercase() {
                    problem(format!("{} is a `{SHARED_FOLDER}` folder; team spaces can't be inside a team space", shown.display()));
                    refused = true;
                    continue;
                }
                scan.dirs.push((child_os.clone(), child_names.clone()));
                stack.push((child_os, child_names));
            } else if kind.is_file() {
                scan.files.push(FileEntry { os: child_os, names: child_names, size: meta.len(), modified: meta.modified().ok() });
            } else {
                problem(format!("{} is not an ordinary file or folder", shown.display()));
                refused = true;
            }
        }
    }
    if refused {
        return Err(AgentError::Usage(format!("this folder can't be made into a team space: {}", problems.join("; "))));
    }
    if scan.files.is_empty() && scan.dirs.is_empty() {
        return Err(AgentError::Usage("this folder is empty; there is nothing to move".into()));
    }
    scan.dirs.sort();
    scan.files.sort_by(|a, b| a.os.cmp(&b.os));
    Ok(scan)
}

/// Whether the sync leaves `name` alone (the engine's default ignore patterns, ignoring ASCII case).
fn ignored(name: &str) -> bool {
    craft_cloud_sync::DEFAULT_IGNORES.iter().any(|pattern| {
        if let Some(prefix) = pattern.strip_suffix('*') {
            name.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        } else if let Some(suffix) = pattern.strip_prefix('*') {
            name.len().checked_sub(suffix.len()).and_then(|start| name.get(start..)).is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
        } else {
            name.eq_ignore_ascii_case(pattern)
        }
    })
}

/// The files the OS drops into folders by itself: deleted with the folder, as the sync does.
const OS_METADATA: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

// ---------------------------------------------------------------------------------------------------------------------
// The work

/// What the copy knows of a file once it is in the team folder: the manifest its content was chunked into.
#[derive(Clone)]
struct Copied {
    entry: FileEntry,
    manifest: ManifestId,
}

struct Job {
    control: Control,
    data: DataDir,
    account: Account,
    spaces: SpacesHandle,
    status: FromFolderStatus,
    scan: Scan,
}

/// Why the work stopped.
enum Stop {
    /// The agent is stopping: the journal says what was under way.
    Interrupted,
    Failed(String),
}

impl From<AgentError> for Stop {
    fn from(e: AgentError) -> Self {
        Stop::Failed(e.to_string())
    }
}

fn failed(e: impl std::fmt::Display) -> Stop {
    Stop::Failed(e.to_string())
}

impl Job {
    async fn run(mut self) {
        let result = self.work().await;
        match result {
            Ok(()) => {}
            Err(Stop::Interrupted) => {
                // The agent is stopping; the journal keeps the step, and the next start reports it.
            }
            Err(Stop::Failed(message)) => {
                let at = self.status.step;
                self.status.failed_at = Some(at);
                self.status.message = Some(message);
                self.status.step = FromFolderStep::Failed;
                self.publish(true);
                crate::runlog::note(format_args!("making a team space from a folder failed at {at:?}; the old folder was not deleted"));
            }
        }
        if let Some(space) = self.status.space {
            self.spaces.claim_from(space, None);
        }
    }

    fn publish(&self, journal: bool) {
        if journal {
            write_journal(&self.data, self.account.space, &self.status);
        }
        self.control.set_from_folder(Some(self.status.clone()));
    }

    fn step(&mut self, step: FromFolderStep) {
        self.status.step = step;
        self.status.done = 0;
        self.status.total = match step {
            FromFolderStep::Copying | FromFolderStep::Syncing | FromFolderStep::Verifying => self.scan.files.len() as u64,
            _ => 0,
        };
        self.publish(true);
    }

    fn stopping(&self) -> bool {
        self.control.spaces_handle().is_none()
    }

    /// The test hooks, and the agent stopping, at the start of a step.
    async fn gate(&self) -> Result<(), Stop> {
        loop {
            if self.stopping() {
                return Err(Stop::Interrupted);
            }
            match hook::get(self.data.path()) {
                Some(hook::Hook::Fail(step)) if step == self.status.step => return Err(failed("failed on purpose (test)")),
                Some(hook::Hook::Pause(step)) if step == self.status.step => tokio::time::sleep(Duration::from_millis(50)).await,
                _ => return Ok(()),
            }
        }
    }

    async fn work(&mut self) -> Result<(), Stop> {
        let space = self.status.space.ok_or_else(|| failed("the team space is not known"))?;
        // The personal pass first, so that the personal space has what is on disk and the claims find it.
        self.control.request_pass();
        let team_folder = self.set_up(space).await?;
        let mut last: Vec<Copied> = Vec::new();
        for round in 1..=ROUNDS {
            self.step(FromFolderStep::Copying);
            self.gate().await?;
            let copied = self.copy(&team_folder).await?;
            self.step(FromFolderStep::Syncing);
            self.gate().await?;
            self.wait_for_server(space, &copied).await?;
            self.step(FromFolderStep::Verifying);
            self.gate().await?;
            if self.verify(&copied).await? {
                last = copied;
                break;
            }
            if round == ROUNDS {
                return Err(failed("the folder keeps changing while it is copied; nothing was deleted. Try again when it is quiet"));
            }
            // The folder changed meanwhile: copy it again (a rescan finds new files; copied ones are in place).
            self.rescan().await?;
        }
        self.step(FromFolderStep::Trashing);
        self.gate().await?;
        self.trash(last).await?;
        Ok(())
    }

    /// Step 3: the team folder and its sync, with claims allowed from the personal space.
    async fn set_up(&mut self, space: SpaceId) -> Result<PathBuf, Stop> {
        self.step(FromFolderStep::SettingUp);
        self.gate().await?;
        self.spaces.claim_from(space, Some(self.account.space));
        let deadline = Instant::now() + LISTED_WITHIN;
        // The space is made; the agent's list has it after a look at the server.
        loop {
            self.spaces.refresh().await;
            if self.control.space_info(space).is_some_and(|i| i.state == "active") {
                break;
            }
            if Instant::now() > deadline {
                return Err(failed("the new team space does not show in the agent's list yet; ask again to go on"));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let info = self.spaces.sync(space, SpaceSyncRequest { on: true, folder: None, adopt: false }).await.map_err(failed)?;
        let folder = info.folder.map(PathBuf::from).ok_or_else(|| failed("the agent did not say where the team space syncs"))?;
        self.status.team_folder = Some(folder.display().to_string());
        self.publish(true);
        Ok(folder)
    }

    async fn rescan(&mut self) -> Result<(), Stop> {
        let (account, folder) = (self.account.clone(), self.scan.root.clone());
        let scan = tokio::task::spawn_blocking(move || check_folder(&account, &folder)).await.map_err(failed)??;
        self.scan = scan;
        Ok(())
    }

    /// Step 4: every file into the team folder. Returns what was copied.
    async fn copy(&mut self, team_folder: &Path) -> Result<Vec<Copied>, Stop> {
        let (scan_dirs, files, root) = (self.scan.dirs.clone(), self.scan.files.clone(), self.scan.root.clone());
        let (team, control) = (team_folder.to_owned(), self.control.clone());
        let done = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let progress = done.clone();
        let task = tokio::task::spawn_blocking(move || copy_all(&root, &team, &scan_dirs, &files, &progress, &|| control.spaces_handle().is_none()));
        let result = poll_blocking(task, || {
            self.status.done = done.load(std::sync::atomic::Ordering::Relaxed);
            self.control.set_from_folder(Some(self.status.clone()));
        })
        .await;
        match result {
            Ok(Ok(copied)) => Ok(copied),
            Ok(Err(CopyStop::Interrupted)) => Err(Stop::Interrupted),
            Ok(Err(CopyStop::Failed(message))) => Err(Stop::Failed(message)),
            Err(e) => Err(failed(e)),
        }
    }

    /// Step 5: the server's tree for the team space holds every copied file.
    async fn wait_for_server(&mut self, space: SpaceId, copied: &[Copied]) -> Result<(), Stop> {
        let transport = HttpTransport::new(self.account.server.clone(), &self.account.token).and_then(|t| t.bound_to(space)).map_err(failed)?;
        let total = copied.len() as u64;
        let mut best = 0u64;
        let mut progressed = Instant::now();
        loop {
            if self.stopping() {
                return Err(Stop::Interrupted);
            }
            match tree_matches(&transport, copied, &self.scan.dirs).await {
                Ok(matched) => {
                    self.status.done = matched.files;
                    self.control.set_from_folder(Some(self.status.clone()));
                    if matched.files == total && matched.dirs_missing == 0 {
                        return Ok(());
                    }
                    if matched.files > best {
                        best = matched.files;
                        progressed = Instant::now();
                    }
                }
                // A failed look is tried again; a long failure is the stall below.
                Err(e) => crate::runlog::note(format_args!("looking at the new team space failed: {e}")),
            }
            if let Some(run) = self.control.space_info(space).and_then(|i| i.run_state) {
                match run {
                    SpaceRun::Paused { reason, .. } => return Err(failed(format!("the team space stopped syncing: {reason}"))),
                    SpaceRun::NoAccess { .. } => return Err(failed("this account has lost access to the new team space")),
                    SpaceRun::Unavailable { message } => return Err(failed(message)),
                    _ => {}
                }
            }
            if let Some(why) = self.skipped_by_team_pass(space) {
                return Err(failed(format!("the team space can't take every file: {why}")));
            }
            if progressed.elapsed() > STALL {
                return Err(failed(format!("the team space has taken {best} of {total} files and no more for {} minutes", STALL.as_secs() / 60)));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    /// A file the team pass skipped with a reason (too big, can't be read), once its pass is idle and the files are still missing.
    fn skipped_by_team_pass(&self, space: SpaceId) -> Option<String> {
        let folder = self.status.team_folder.as_deref().map(PathBuf::from)?;
        if !matches!(self.control.space_info(space).and_then(|i| i.run_state), Some(SpaceRun::Idle)) {
            return None;
        }
        let (saved, _) = self.data.space_dir(space).load_state_for(space, &folder, None).ok()?;
        // A file that was just put there and has not settled yet (or waits for a Craft save) is tried again by the pass itself.
        let waits = |why: &str| {
            why.starts_with(craft_cloud_sync::STILL_CHANGING)
                || why.starts_with(craft_cloud_sync::SAVE_ORDER_WAITS)
                || why.starts_with(craft_cloud_sync::SAVE_RECORD_WAITS)
        };
        saved.last_pass?.summary?.skipped.into_iter().map(|(_, why)| why).find(|why| !waits(why))
    }

    /// Step 6: the old folder is still exactly what was copied. False when it changed (copy again).
    async fn verify(&mut self, copied: &[Copied]) -> Result<bool, Stop> {
        let (account, folder) = (self.account.clone(), self.scan.root.clone());
        let scan = tokio::task::spawn_blocking(move || check_folder(&account, &folder)).await.map_err(failed)??;
        if scan.files.len() != copied.len() || scan.dirs.len() != self.scan.dirs.len() {
            self.scan = scan;
            return Ok(false);
        }
        let (root, copied_list) = (self.scan.root.clone(), copied.to_vec());
        let control = self.control.clone();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let progress = done.clone();
        let task = tokio::task::spawn_blocking(move || verify_all(&root, &copied_list, &progress, &|| control.spaces_handle().is_none()));
        let result = poll_blocking(task, || {
            self.status.done = done.load(std::sync::atomic::Ordering::Relaxed);
            self.control.set_from_folder(Some(self.status.clone()));
        })
        .await;
        match result {
            Ok(Ok(same)) => Ok(same),
            Ok(Err(CopyStop::Interrupted)) => Err(Stop::Interrupted),
            Ok(Err(CopyStop::Failed(message))) => Err(Stop::Failed(message)),
            Err(e) => Err(failed(e)),
        }
    }

    /// Step 7: deletes the old folder's files here, each only while it is as it was verified.
    async fn trash(&mut self, copied: Vec<Copied>) -> Result<(), Stop> {
        let root = self.scan.root.clone();
        let dirs = self.scan.dirs.clone();
        let kept = tokio::task::spawn_blocking(move || delete_old(&root, &copied, &dirs)).await.map_err(failed)?;
        // The personal sync sees a deleted folder: soon, and as any deletion.
        self.control.request_pass();
        self.status.step = FromFolderStep::Done;
        self.status.kept = kept;
        self.status.done = 0;
        self.status.total = 0;
        self.status.failed_at = None;
        self.status.message = (kept > 0).then(|| {
            format!(
                "{kept} file{} stayed in the old folder: changed after the copy, or of a kind that does not sync (such as .tmp files). {} not in the team space.",
                if kept == 1 { "" } else { "s" },
                if kept == 1 { "It is" } else { "They are" }
            )
        });
        self.publish(true);
        Ok(())
    }
}

/// Waits for a blocking task, calling `tick` about every 200 ms meanwhile.
async fn poll_blocking<T>(mut task: tokio::task::JoinHandle<T>, mut tick: impl FnMut()) -> Result<T, tokio::task::JoinError> {
    loop {
        tokio::select! {
            result = &mut task => return result,
            () = tokio::time::sleep(Duration::from_millis(200)) => tick(),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Copying (blocking)

enum CopyStop {
    Interrupted,
    Failed(String),
}

fn io_failed(path: &Path, e: &io::Error) -> CopyStop {
    CopyStop::Failed(format!("{}: {e}", path.display()))
}

fn join(root: &Path, os: &[std::ffi::OsString]) -> PathBuf {
    os.iter().fold(root.to_owned(), |p, n| p.join(n))
}

/// The manifest of a file's content, the way the engine makes it, while `sink` receives every chunk's bytes.
fn chunk_file(path: &Path, mut sink: impl FnMut(&[u8]) -> io::Result<()>) -> Result<(ManifestId, u64), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let chunker = Chunker::new(FASTCDC_V1).map_err(|e| e.to_string())?;
    let summary = chunk_reader::<_, io::Error, _>(&chunker, io::BufReader::new(file), |chunk| sink(chunk.data))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let refs = summary.chunks.iter().map(|c| ChunkRef { id: ChunkId::from_bytes(c.hash), len: c.len }).collect();
    let params = ChunkerParamsId::new(FASTCDC_V1.name).map_err(|e| e.to_string())?;
    let manifest =
        Manifest::new(params, summary.size, FileHash::from_bytes(summary.file_hash), refs).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((manifest.id(), summary.size))
}

fn same_stat(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

fn copy_all(
    root: &Path,
    team: &Path,
    dirs: &[(Vec<std::ffi::OsString>, Vec<String>)],
    files: &[FileEntry],
    done: &std::sync::atomic::AtomicU64,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Copied>, CopyStop> {
    let tmp = team.join(AGENT_DIR).join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| io_failed(&tmp, &e))?;
    // Leftovers of an earlier try.
    if let Ok(entries) = std::fs::read_dir(&tmp) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == COPY_EXT) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    for (os, _) in dirs {
        let dir = join(team, os);
        std::fs::create_dir_all(&dir).map_err(|e| io_failed(&dir, &e))?;
    }
    let mut copied = Vec::with_capacity(files.len());
    for entry in files {
        if cancelled() {
            return Err(CopyStop::Interrupted);
        }
        let source = join(root, &entry.os);
        let dest = join(team, &entry.os);
        let mut attempt = 0;
        let manifest = loop {
            attempt += 1;
            match copy_file(&source, &dest, &tmp) {
                Ok(Some(manifest)) => break manifest,
                Ok(None) if attempt < 4 => std::thread::sleep(Duration::from_millis(300)),
                Ok(None) => return Err(CopyStop::Failed(format!("{} keeps changing while it is copied; nothing was deleted", source.display()))),
                Err(e) => return Err(CopyStop::Failed(e)),
            }
        };
        copied.push(Copied { entry: entry.clone(), manifest });
        done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(copied)
}

/// Copies one file into place unless it already is (same size and time as the source). `None`: the source changed while it was
/// read; the copy was thrown away.
fn copy_file(source: &Path, dest: &Path, tmp: &Path) -> Result<Option<ManifestId>, String> {
    let before = std::fs::metadata(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let in_place = std::fs::metadata(dest).is_ok_and(|d| d.is_file() && same_stat(&before, &d));
    if in_place {
        // Same size and time as the source: put there by an earlier try. Its content is still read from the source for the
        // manifest, which is what the server is compared with.
        let (manifest, _) = chunk_file(source, |_| Ok(()))?;
        let after = std::fs::metadata(source).map_err(|e| format!("{}: {e}", source.display()))?;
        return Ok(same_stat(&before, &after).then_some(manifest));
    }
    let temp = tmp.join(format!("{}.{COPY_EXT}", random_hex(12).map_err(|e| e.to_string())?));
    let result = (|| -> Result<Option<ManifestId>, String> {
        let mut out = std::fs::File::options().write(true).create_new(true).open(&temp).map_err(|e| format!("{}: {e}", temp.display()))?;
        let (manifest, size) = chunk_file(source, |data| out.write_all(data)).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| format!("{}: {e}", temp.display()))?;
        if out.metadata().map(|m| m.len()).ok() != Some(size) {
            return Err(format!("{}: the copy has the wrong size", temp.display()));
        }
        if let Ok(modified) = before.modified() {
            out.set_modified(modified).map_err(|e| format!("{}: {e}", temp.display()))?;
        }
        drop(out);
        // The permissions (the executable bit) go with the file.
        std::fs::set_permissions(&temp, before.permissions()).map_err(|e| format!("{}: {e}", temp.display()))?;
        let after = std::fs::metadata(source).map_err(|e| format!("{}: {e}", source.display()))?;
        if !same_stat(&before, &after) {
            return Ok(None);
        }
        put_in_place(&temp, dest)?;
        Ok(Some(manifest))
    })();
    if !matches!(result, Ok(Some(_))) {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Renames the finished copy over its place; another program that holds the destination for a moment (the team pass reading
/// an older copy) is waited for.
fn put_in_place(temp: &Path, dest: &Path) -> Result<(), String> {
    let mut last = None;
    for _ in 0..RENAME_TRIES {
        match std::fs::rename(temp, dest) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound && !temp.exists() => {
                return Err(format!("{}: the copy was removed under the work", temp.display()));
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
    Err(format!("{}: {}", dest.display(), last.map_or_else(|| "can't be put in place".to_owned(), |e| e.to_string())))
}

/// Whether every copied file of the old folder is as it was copied: same manifest of its content now. `Ok(false)` when not.
fn verify_all(root: &Path, copied: &[Copied], done: &std::sync::atomic::AtomicU64, cancelled: &dyn Fn() -> bool) -> Result<bool, CopyStop> {
    for item in copied {
        if cancelled() {
            return Err(CopyStop::Interrupted);
        }
        let path = join(root, &item.entry.os);
        let Ok((manifest, _)) = chunk_file(&path, |_| Ok(())) else { return Ok(false) };
        if manifest != item.manifest {
            return Ok(false);
        }
        done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(true)
}

/// Deletes the verified files of the old folder, each only if it is still as it was when it was verified, then the folders that
/// are empty. Returns how many files stayed.
fn delete_old(root: &Path, copied: &[Copied], dirs: &[(Vec<std::ffi::OsString>, Vec<String>)]) -> u64 {
    for item in copied {
        let path = join(root, &item.entry.os);
        let unchanged =
            std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == item.entry.size && m.modified().ok() == item.entry.modified);
        if unchanged {
            // A manifest of the content once more, right before: a same-size same-time edit is caught too.
            if chunk_file(&path, |_| Ok(())).is_ok_and(|(m, _)| m == item.manifest) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    // What the OS leaves in folders goes with them.
    let mut places: Vec<PathBuf> = dirs.iter().map(|(os, _)| join(root, os)).collect();
    places.push(root.to_owned());
    for place in places {
        for name in OS_METADATA {
            let path = place.join(name);
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    // Deepest first; a folder that still holds something stays.
    let mut sorted: Vec<&Vec<std::ffi::OsString>> = dirs.iter().map(|(os, _)| os).collect();
    sorted.sort_by_key(|os| std::cmp::Reverse(os.len()));
    for os in sorted {
        let _ = std::fs::remove_dir(join(root, os));
    }
    let remaining = count_files(root);
    if remaining == 0 {
        let _ = std::fs::remove_dir(root);
    }
    remaining
}

fn count_files(root: &Path) -> u64 {
    let mut count = 0;
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(_) => count += 1,
                Err(_) => count += 1,
            }
        }
    }
    count
}

// ---------------------------------------------------------------------------------------------------------------------
// The server's tree

struct Matched {
    files: u64,
    dirs_missing: u64,
}

/// How much of the copy the team space's tree on the server holds: files with the manifest they were chunked into, and folders.
async fn tree_matches(transport: &HttpTransport, copied: &[Copied], dirs: &[(Vec<std::ffi::OsString>, Vec<String>)]) -> Result<Matched, String> {
    use craft_cloud_proto::{NodeKind, NodeState};
    let mut nodes: std::collections::HashMap<NodeId, NodeState> = std::collections::HashMap::new();
    let mut after = None;
    loop {
        let page = transport.tree(after).await.map_err(|e| e.to_string())?;
        for node in page.nodes {
            if !node.deleted {
                nodes.insert(node.id, node);
            }
        }
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    // Paths of the nodes, from their parents (the root has no name).
    let mut paths: std::collections::HashMap<Vec<String>, &NodeState> = std::collections::HashMap::with_capacity(nodes.len());
    for node in nodes.values() {
        let mut names: Vec<String> = Vec::new();
        let mut at = Some(node);
        let mut depth = 0;
        while let Some(current) = at {
            let Some(name) = current.name.as_ref() else { break };
            names.push(name.as_str().to_owned());
            at = current.parent.and_then(|p| nodes.get(&p));
            depth += 1;
            if depth > craft_cloud_proto::limits::MAX_TREE_DEPTH + 1 {
                names.clear();
                break;
            }
        }
        if names.is_empty() {
            continue;
        }
        names.reverse();
        paths.insert(names, node);
    }
    let files = copied
        .iter()
        .filter(|c| matches!(paths.get(&c.entry.names), Some(NodeState { kind: NodeKind::File(file), .. }) if file.manifest == c.manifest && file.size == c.entry.size))
        .count() as u64;
    let dirs_missing = dirs.iter().filter(|(_, names)| !matches!(paths.get(names), Some(NodeState { kind: NodeKind::Dir, .. }))).count() as u64;
    Ok(Matched { files, dirs_missing })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_the_sync_never_carries_are_left_alone() {
        for name in [".DS_Store", "Thumbs.db", "desktop.ini", "~$report.docx", "render.TMP", "x.part", ".~lock.report.odt#"] {
            assert!(ignored(name), "{name}");
        }
        for name in ["report.docx", "tmp", "part.txt", "a~$b", "photo.tmpl"] {
            assert!(!ignored(name), "{name}");
        }
    }

    fn status(step: FromFolderStep) -> FromFolderStatus {
        FromFolderStatus {
            folder: "/p/Project".into(),
            name: "Team".into(),
            step,
            space: Some(SpaceId::generate()),
            team_folder: None,
            done: 0,
            total: 0,
            failed_at: None,
            message: None,
            kept: 0,
        }
    }

    #[test]
    fn work_that_was_under_way_when_the_agent_stopped_is_reported_as_interrupted_and_a_foreign_journal_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        let control = Control::new(tokio::sync::mpsc::channel(1).0);
        let personal = SpaceId::generate();
        let account = Account {
            schema: crate::account::ACCOUNT_SCHEMA,
            server: crate::http::ServerUrl::parse("https://cloud.example.com").unwrap(),
            username: "ana".into(),
            device: craft_cloud_proto::DeviceId::generate(),
            device_name: "laptop".into(),
            folder: dir.path().join("Craft Cloud"),
            space: personal,
            root: NodeId::generate(),
            token: crate::http::DeviceToken::new("tok".into()),
            spaces: Vec::new(),
        };
        for step in [FromFolderStep::Creating, FromFolderStep::Copying, FromFolderStep::Trashing] {
            write_journal(&data, personal, &status(step));
            restore(&control, &data, &account);
            let shown = control.from_folder().unwrap();
            assert_eq!((shown.step, shown.failed_at), (FromFolderStep::Interrupted, Some(step)), "{step:?}");
            assert!(shown.message.is_some());
            // The journal says it too, so the next start reports the same.
            assert_eq!(read_journal(&data, personal).map(|s| s.step), Some(FromFolderStep::Interrupted));
        }
        // Finished and failed work is shown as it was.
        write_journal(&data, personal, &status(FromFolderStep::Done));
        restore(&control, &data, &account);
        assert_eq!(control.from_folder().map(|s| s.step), Some(FromFolderStep::Done));
        // Another account's journal is not ours: removed.
        write_journal(&data, SpaceId::generate(), &status(FromFolderStep::Copying));
        control.set_from_folder(None);
        restore(&control, &data, &account);
        assert_eq!(control.from_folder(), None);
        assert!(!journal_path(&data).exists());
    }
}
