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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
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
    /// When the call that makes the space started (milliseconds since the Unix epoch): a space a crashed call is taken to have made
    /// is only one made after this.
    #[serde(default)]
    started_ms: Option<i64>,
}

fn journal_path(data: &DataDir) -> PathBuf {
    data.path().join(JOURNAL_FILE)
}

fn read_saved(data: &DataDir, personal: SpaceId) -> Option<Journal> {
    let bytes = std::fs::read(journal_path(data)).ok()?;
    let journal: Journal = serde_json::from_slice(&bytes).ok()?;
    (journal.schema <= JOURNAL_SCHEMA && journal.personal == personal).then_some(journal)
}

fn read_journal(data: &DataDir, personal: SpaceId) -> Option<FromFolderStatus> {
    read_saved(data, personal).map(|j| j.status)
}

/// Writes the journal, keeping the start time it has.
fn write_journal(data: &DataDir, personal: SpaceId, status: &FromFolderStatus) {
    let started_ms = read_saved(data, personal).and_then(|j| j.started_ms);
    write_journal_started(data, personal, status, started_ms);
}

fn write_journal_started(data: &DataDir, personal: SpaceId, status: &FromFolderStatus, started_ms: Option<i64>) {
    let journal = Journal { schema: JOURNAL_SCHEMA, personal, status: status.clone(), started_ms };
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

    /// What is called with a file taken out of the old folder.
    pub type Callback = fn(&Path);

    static HOOKS: Mutex<Vec<(PathBuf, Hook)>> = Mutex::new(Vec::new());
    static STALLS: Mutex<Vec<(PathBuf, Duration)>> = Mutex::new(Vec::new());
    static AFTER: Mutex<Vec<(PathBuf, Callback)>> = Mutex::new(Vec::new());
    static BETWEEN: Mutex<Vec<(PathBuf, Callback)>> = Mutex::new(Vec::new());

    /// Calls `f` with the path of each file after its quarantine place was made and before the file is moved into it (the moment a
    /// program could save a new file at the old path).
    pub fn set_between_steps(data: &Path, f: Option<Callback>) {
        let mut hooks = BETWEEN.lock().unwrap_or_else(PoisonError::into_inner);
        hooks.retain(|(path, _)| path != data);
        if let Some(f) = f {
            hooks.push((data.to_owned(), f));
        }
    }

    pub(super) fn between_steps(data: &Path) -> Option<Callback> {
        BETWEEN.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(path, _)| path == data).map(|(_, f)| *f)
    }

    static BATCHES: Mutex<Vec<(PathBuf, usize)>> = Mutex::new(Vec::new());
    static DELAYS: Mutex<Vec<(PathBuf, Duration)>> = Mutex::new(Vec::new());

    /// How many files one batch of deletions holds (default: 1,000), so a test can see what happens between batches.
    pub fn set_trash_batch(data: &Path, files: Option<usize>) {
        let mut batches = BATCHES.lock().unwrap_or_else(PoisonError::into_inner);
        batches.retain(|(path, _)| path != data);
        if let Some(files) = files {
            batches.push((data.to_owned(), files));
        }
    }

    pub(super) fn trash_batch(data: &Path) -> Option<usize> {
        BATCHES.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(path, _)| path == data).map(|(_, n)| *n)
    }

    /// A pause after every chunk of the copy (default: none), to make a copy slow.
    pub fn set_copy_delay(data: &Path, delay: Option<Duration>) {
        let mut delays = DELAYS.lock().unwrap_or_else(PoisonError::into_inner);
        delays.retain(|(path, _)| path != data);
        if let Some(delay) = delay {
            delays.push((data.to_owned(), delay));
        }
    }

    pub(super) fn copy_delay(data: &Path) -> Option<Duration> {
        DELAYS.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(path, _)| path == data).map(|(_, d)| *d)
    }

    /// How long the team space may store nothing before the wait for it fails (default: ten minutes).
    pub fn set_stall(data: &Path, stall: Option<Duration>) {
        let mut stalls = STALLS.lock().unwrap_or_else(PoisonError::into_inner);
        stalls.retain(|(path, _)| path != data);
        if let Some(stall) = stall {
            stalls.push((data.to_owned(), stall));
        }
    }

    pub(super) fn stall(data: &Path) -> Option<Duration> {
        STALLS.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(path, _)| path == data).map(|(_, d)| *d)
    }

    /// Calls `f` with the path of each file right after it was taken out of the old folder and before it is read again.
    pub fn set_after_quarantine(data: &Path, f: Option<fn(&Path)>) {
        let mut after = AFTER.lock().unwrap_or_else(PoisonError::into_inner);
        after.retain(|(path, _)| path != data);
        if let Some(f) = f {
            after.push((data.to_owned(), f));
        }
    }

    pub(super) fn after_quarantine(data: &Path) -> Option<fn(&Path)> {
        AFTER.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(path, _)| path == data).map(|(_, f)| *f)
    }

    static REPLACES: Mutex<Vec<(PathBuf, Callback)>> = Mutex::new(Vec::new());
    static SAVES: Mutex<Vec<(PathBuf, Duration)>> = Mutex::new(Vec::new());

    /// Calls `f` with the path of a team file the copy is about to replace by a newer version of its own, after the new version was
    /// read in full and before the old one is touched (the moment a team edit could be made).
    pub fn set_before_replace(data: &Path, f: Option<Callback>) {
        let mut hooks = REPLACES.lock().unwrap_or_else(PoisonError::into_inner);
        hooks.retain(|(path, _)| path != data);
        if let Some(f) = f {
            hooks.push((data.to_owned(), f));
        }
    }

    pub(super) fn before_replace(data: &Path) -> Option<Callback> {
        REPLACES.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(path, _)| path == data).map(|(_, f)| *f)
    }

    /// How often the copy saves what it wrote (default: every second); `Duration::ZERO` saves after every file.
    pub fn set_save_every(data: &Path, every: Option<Duration>) {
        let mut saves = SAVES.lock().unwrap_or_else(PoisonError::into_inner);
        saves.retain(|(path, _)| path != data);
        if let Some(every) = every {
            saves.push((data.to_owned(), every));
        }
    }

    pub(super) fn save_every(data: &Path) -> Option<Duration> {
        SAVES.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(path, _)| path == data).map(|(_, d)| *d)
    }

    static LOSES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Makes the copy of this agent not save what it wrote when it ends, as a kill would not: only what it saved as it went is kept.
    pub fn set_lose_unsaved(data: &Path, on: bool) {
        let mut loses = LOSES.lock().unwrap_or_else(PoisonError::into_inner);
        loses.retain(|path| path != data);
        if on {
            loses.push(data.to_owned());
        }
    }

    pub(super) fn lose_unsaved(data: &Path) -> bool {
        LOSES.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|path| path == data)
    }

    static LINKS: Mutex<Vec<(PathBuf, io::ErrorKind)>> = Mutex::new(Vec::new());

    /// Makes every hard link made below `under` fail with an error of this kind (`Unsupported` is what a volume without hard links
    /// says; any other kind is a failure of its own), or ends it with `None`.
    pub fn set_hard_link_error(under: &Path, kind: Option<io::ErrorKind>) {
        let mut links = LINKS.lock().unwrap_or_else(PoisonError::into_inner);
        let canonical = std::fs::canonicalize(under).ok();
        links.retain(|(path, _)| path != under && Some(path) != canonical.as_ref());
        if let Some(kind) = kind {
            links.push((under.to_owned(), kind));
            if let Some(canonical) = canonical {
                links.push((canonical, kind));
            }
        }
    }

    pub(super) fn hard_link_error(to: &Path) -> Option<io::ErrorKind> {
        LINKS.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(path, _)| to.starts_with(path)).map(|(_, kind)| *kind)
    }

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

/// Whether `path` is gone: only "not found" says so (a drive that is not mounted yet, or a permission error, does not).
fn is_gone(path: &str) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// Reads the journal at the start of an agent that is signed in: work that was under way is now interrupted. A journal of
/// another account is removed. Whatever a crashed deletion left in the quarantine of this personal folder goes back first,
/// whichever journal (or none) it belongs to; if some of it cannot be put back the work is not done, and the status says where
/// the files are.
pub fn restore(control: &Control, data: &DataDir, account: &Account) {
    let stranded = recover_quarantine_all(account);
    let path = journal_path(data);
    let Some(mut status) = read_journal(data, account.space) else {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        if !stranded.is_empty() {
            let kept = FromFolderStatus {
                folder: String::new(),
                name: String::new(),
                step: FromFolderStep::Failed,
                space: None,
                team_folder: None,
                done: 0,
                total: 0,
                failed_at: Some(FromFolderStep::Trashing),
                message: Some(stranded_words(&stranded)),
                kept: 0,
            };
            control.set_from_folder(Some(kept));
        }
        return;
    };
    if !stranded.is_empty() {
        status.step = FromFolderStep::Failed;
        status.failed_at = Some(FromFolderStep::Trashing);
        status.message = Some(stranded_words(&stranded));
        write_journal(data, account.space, &status);
        control.set_from_folder(Some(status));
        return;
    }
    // What could not be put back before has been put back now: the old message no longer holds.
    if status.step == FromFolderStep::Failed && status.message.as_deref().is_some_and(|m| m.starts_with(STRANDED_LEAD)) {
        status.step = FromFolderStep::Interrupted;
        status.message = Some(
            "The files that could not be put back were put back. Ask again to finish; the files still in the old folder are checked once more \
             before they are deleted."
                .to_owned(),
        );
        write_journal(data, account.space, &status);
    }
    let trashing = status.step == FromFolderStep::Trashing || status.failed_at == Some(FromFolderStep::Trashing);
    // Stopped after the old folder was removed, before "done" was written: there is nothing left to finish.
    if trashing && status.step != FromFolderStep::Done && is_gone(&status.folder) {
        status.step = FromFolderStep::Done;
        status.failed_at = None;
        status.message = None;
        status.done = 0;
        status.total = 0;
        write_journal(data, account.space, &status);
        control.set_from_folder(Some(status));
        return;
    }
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

/// The session ends (sign-out, another account, the agent stopping): the work of this session is told so and waited for, so it has
/// written nothing more when the next session starts and reads the journal. A job that does not end within the bound is cut off
/// (its blocking work looks at the flag for every chunk of a file). A call that is still under way (checking the folder, making the
/// space) sees the session number change and writes nothing.
pub async fn end_session(control: &Control) {
    control.bump_session();
    let Some(mut job) = control.take_from_folder_job() else { return };
    job.cancel.store(true, Ordering::SeqCst);
    if tokio::time::timeout(Duration::from_secs(15), &mut job.task).await.is_err() {
        job.task.abort();
    }
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

/// Waits while a test hook holds the call at `step` (and while the session it belongs to goes on).
async fn hold_at(data: &DataDir, step: FromFolderStep, alive: &(dyn Fn() -> bool + Sync)) {
    while matches!(hook::get(data.path()), Some(hook::Hook::Pause(s)) if s == step) && alive() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `POST /v1/spaces/from-folder`: checks the folder, makes the team space and starts the rest of the work. The answer is the
/// status then. The call belongs to the session it started in: after every wait it looks whether that session is still the
/// agent's, and if it is not it writes nothing (no journal, no status, no job) and answers that the agent is not signed in.
pub(crate) async fn start(start: Start, request: FromFolderRequest) -> Result<FromFolderStatus, LocalError> {
    let Start { control, data } = start;
    // The session number first: a session that ends after it makes every later look say so.
    let session = control.session_id();
    let account = control.account().ok_or(AgentError::NotSignedIn)?;
    let spaces = control.spaces_handle().ok_or(AgentError::NotSignedIn)?;
    let alive = || control.session_id() == session && control.account().is_some_and(|a| a.space == account.space && a.device == account.device);
    let ended = || LocalError::from(AgentError::NotSignedIn);
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
    let scan = tokio::task::spawn_blocking(move || {
        // Files an earlier try's deletion had taken out of an old folder and not put back come back first.
        let _ = recover_quarantine_all(&checked_account);
        check_folder(&checked_account, &checked_folder)
    })
    .await
    .map_err(|e| LocalError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, craft_cloud_proto::api::codes::INTERNAL, e.to_string()))?;
    hold_at(&data, FromFolderStep::Checking, &alive).await;
    if !alive() {
        return Err(ended());
    }
    let scan = scan?;
    let canonical = scan.root.clone();

    let saved = read_saved(&data, account.space);
    let before = saved.as_ref().map(|j| j.status.clone());
    let started_before = saved.and_then(|j| j.started_ms);
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
    let busy = || {
        LocalError::new(axum::http::StatusCode::CONFLICT, craft_cloud_proto::api::codes::CONFLICT, "another team space is being made from a folder")
    };
    if status.space.is_none() {
        let transport = HttpTransport::new(account.server.clone(), &account.token).map_err(AgentError::from)?;
        if request.confirm.password.is_none() {
            return Err(AgentError::PasswordRequired.into());
        }
        // The server's clock just before the call, kept in the journal: a space is taken to be the one a crashed call made only if
        // the server made it after this. Once a call was sent the time is never written again.
        let fresh = transport.server_clock_ms().await;
        if !alive() {
            return Err(ended());
        }
        let started = started_time(crashed_creating, started_before, fresh);
        if !control.begin_from_folder(status.clone()) {
            return Err(busy());
        }
        write_journal_started(&data, account.space, &status, started);
        let created = create_space(&transport, &name, &request).await;
        hold_at(&data, FromFolderStep::Creating, &alive).await;
        // The session ended while the server was asked: nothing of this call is written under the next one. (A space the
        // server made stays, and a journal of this account is found again, or dropped, by the next start.)
        if !alive() {
            return Err(ended());
        }
        match created {
            Ok(created) => status.space = Some(created.id),
            // A crash between the server's answer and the journal left a space of this name: it is ours only if the server made it
            // after that call started, has this account as its only member (as its owner) and holds nothing.
            Err(e) if crashed_creating && is_name_taken(&e) => match find_empty_own_space(&transport, &name, started_before).await {
                Some(space) => status.space = Some(space),
                None => {
                    if !alive() {
                        return Err(ended());
                    }
                    give_back(&control, &data, account.space, before, started_before);
                    return Err(e.into());
                }
            },
            Err(e) => {
                // Nothing exists: the status (and the journal) of an earlier try is as it was.
                give_back(&control, &data, account.space, before, started_before);
                return Err(e.into());
            }
        }
        if !alive() {
            return Err(ended());
        }
    } else if !control.begin_from_folder(status.clone()) {
        return Err(busy());
    }
    status.step = FromFolderStep::SettingUp;
    write_journal(&data, account.space, &status);
    control.set_from_folder(Some(status.clone()));
    let cancel = Arc::new(AtomicBool::new(false));
    let job = Job { control: control.clone(), data, account, spaces, status: status.clone(), scan, cancel: cancel.clone() };
    let task = tokio::spawn(job.run());
    // Registered only if the session is still the one the call began in (checked under the lock `end_session` takes the job with).
    if !control.register_from_folder_job(session, crate::local_api::FromFolderJob { cancel: cancel.clone(), task }) {
        cancel.store(true, Ordering::SeqCst);
        return Err(ended());
    }
    Ok(status)
}

/// The server's time the journal keeps for a creation: a crashed creation tried again keeps the one it has (a retry never rewrites it
/// once a call was sent); otherwise the server's clock just read. No server time (no `Date` header) is no time: nothing is adopted
/// later (the agent's own clock is never used).
fn started_time(crashed_creating: bool, before: Option<i64>, fresh: Option<i64>) -> Option<i64> {
    if crashed_creating { before.or(fresh) } else { fresh }
}

/// Puts the status and the journal back to what an earlier try left (`None`: nothing).
fn give_back(control: &Control, data: &DataDir, personal: SpaceId, before: Option<FromFolderStatus>, started_ms: Option<i64>) {
    match &before {
        Some(status) => write_journal_started(data, personal, status, started_ms),
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

/// How much earlier than the server's clock reading before the call a space may have been made and still be taken for the call's.
const CLOCK_SLACK_MS: i64 = 30 * 1000;

/// The team space called `name` that the account owns, alone, that the server made after `started_ms` (its own clock, read just
/// before the call) and that holds nothing yet: the one a crashed call made, and no one else's. A journal without that time
/// (`None`) adopts nothing.
async fn find_empty_own_space(transport: &HttpTransport, name: &SpaceName, started_ms: Option<i64>) -> Option<SpaceId> {
    let started_ms = started_ms?;
    let list = transport.spaces().await.ok()?;
    let fold = name.fold_key();
    let item = list.spaces.into_iter().find(|i| {
        i.kind == "team"
            && i.state == "active"
            && i.role == craft_cloud_proto::api::SpaceRole::Owner
            && i.members == 1
            && i.created_ms.saturating_add(CLOCK_SLACK_MS) >= started_ms
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
    /// The old folder and the folders above it up to the personal folder, as they were (see [`chain_intact`]).
    chain: Chain,
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
    let mut scan = Scan { root: root.clone(), chain: chain_of(&root, &personal), dirs: Vec::new(), files: Vec::new() };
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
    /// The name of the conflict copy this file's version was put under, beside a file that was edited in the team folder: this
    /// file is not in the team space as it is, and is not deleted from the old folder.
    team_edit: Option<String>,
}

struct Job {
    control: Control,
    data: DataDir,
    account: Account,
    spaces: SpacesHandle,
    status: FromFolderStatus,
    scan: Scan,
    /// Set when the session this job belongs to ends (sign-out, account switch, the agent stopping): the job stops at its next
    /// look, inside the copy loops and the sleeps too, and writes nothing more.
    cancel: Arc<AtomicBool>,
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

impl From<CopyStop> for Stop {
    fn from(e: CopyStop) -> Self {
        match e {
            CopyStop::Interrupted => Stop::Interrupted,
            CopyStop::Failed(message) => Stop::Failed(message),
        }
    }
}

fn failed(e: impl std::fmt::Display) -> Stop {
    Stop::Failed(e.to_string())
}

/// How many files one batch of deletions holds at most, and how long it runs at most, before the server is asked again whether the
/// space still holds what is about to be deleted.
const TRASH_BATCH: usize = 1000;
const TRASH_RECHECK: Duration = Duration::from_secs(60);
/// How long the full tree check of the team space is good for during the deletion.
const TRASH_FULL_CHECK: Duration = Duration::from_secs(600);
/// The roughest time the tree is looked at while a step waits, and how fast it is looked at first.
const POLL_FIRST: Duration = Duration::from_millis(400);
const POLL_SLOWEST: Duration = Duration::from_secs(5);

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
                crate::runlog::note(format_args!("making a team space from a folder failed at {at:?}"));
            }
        }
        if let Some(space) = self.status.space {
            self.spaces.claim_from(space, None);
        }
    }

    /// Whether the job must stop: its session ended, or the agent is stopping.
    fn stopping(&self) -> bool {
        self.cancel.load(Ordering::SeqCst) || self.control.spaces_handle().is_none()
    }

    /// Writes the status (and the journal). A job whose session has ended writes nothing: the status of the account that signed out
    /// is never shown under another one.
    fn publish(&self, journal: bool) {
        if self.cancel.load(Ordering::SeqCst) {
            return;
        }
        if journal {
            write_journal(&self.data, self.account.space, &self.status);
        }
        self.control.set_from_folder(Some(self.status.clone()));
    }

    fn step(&mut self, step: FromFolderStep) {
        self.status.step = step;
        self.status.done = 0;
        self.status.total = match step {
            FromFolderStep::Copying | FromFolderStep::Syncing | FromFolderStep::Verifying | FromFolderStep::Trashing => self.scan.files.len() as u64,
            _ => 0,
        };
        self.publish(true);
    }

    /// The test hooks, and the session ending, at the start of a step.
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

    /// A sleep that the end of the session cuts short (and reports).
    async fn sleep(&self, how_long: Duration) -> Result<(), Stop> {
        let until = Instant::now() + how_long;
        while Instant::now() < until {
            if self.stopping() {
                return Err(Stop::Interrupted);
            }
            tokio::time::sleep(Duration::from_millis(100).min(how_long)).await;
        }
        Ok(())
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
        self.trash(space, last).await?;
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
            self.sleep(Duration::from_millis(500)).await?;
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

    /// Runs `work` off the async threads, with the progress counter it fills shown in the status meanwhile.
    async fn blocking<T: Send + 'static>(
        &mut self,
        work: impl FnOnce(Arc<AtomicU64>, Arc<AtomicBool>, Control) -> Result<T, CopyStop> + Send + 'static,
    ) -> Result<T, Stop> {
        let done = Arc::new(AtomicU64::new(0));
        let (progress, cancel, control) = (done.clone(), self.cancel.clone(), self.control.clone());
        let task = tokio::task::spawn_blocking(move || work(progress, cancel, control));
        let result = poll_blocking(task, || {
            self.status.done = done.load(Ordering::Relaxed);
            self.publish(false);
        })
        .await;
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(stop)) => Err(stop.into()),
            Err(e) => Err(failed(e)),
        }
    }

    /// Step 4: every file into the team folder. Returns what was copied.
    async fn copy(&mut self, team_folder: &Path) -> Result<Vec<Copied>, Stop> {
        let (scan_dirs, files, root, team) = (self.scan.dirs.clone(), self.scan.files.clone(), self.scan.root.clone(), team_folder.to_owned());
        let hooks = CopyHooks {
            delay: hook::copy_delay(self.data.path()),
            before_replace: hook::before_replace(self.data.path()),
            save_every: hook::save_every(self.data.path()).unwrap_or(WRITTEN_SAVE_EVERY),
        };
        let device = self.account.device_name.clone();
        let record = self.status.space.map(|space| self.data.space_dir(space).path().join(WRITTEN_FILE));
        let written = Arc::new(Mutex::new(record.as_deref().map(load_written).unwrap_or_default()));
        let shared = written.clone();
        let saved_to = record.clone();
        let result = self
            .blocking(move |done, cancel, control| {
                copy_all(&root, &team, &scan_dirs, &files, &done, hooks, &device, &shared, saved_to.as_deref(), &|| {
                    cancel.load(Ordering::SeqCst) || control.spaces_handle().is_none()
                })
            })
            .await;
        // What was written is kept whether the copy came to an end or not.
        if let Some(record) = record
            && !hook::lose_unsaved(self.data.path())
        {
            save_written(&record, &written.lock().unwrap_or_else(PoisonError::into_inner));
        }
        result
    }

    /// Step 5: the server's tree for the team space holds every copied file. Progress is anything the team space took: a file
    /// committed, or bytes stored (a chunk uploaded or claimed), so one big file that takes long to go up is no stall.
    async fn wait_for_server(&mut self, space: SpaceId, all: &[Copied]) -> Result<(), Stop> {
        // A file that was edited in the team folder is there as the edit, not as the old folder's version: not waited for.
        let copied: Vec<Copied> = all.iter().filter(|c| c.team_edit.is_none()).cloned().collect();
        let copied = copied.as_slice();
        let transport = HttpTransport::new(self.account.server.clone(), &self.account.token).map_err(failed)?;
        let bound = transport.clone().bound_to(space).map_err(failed)?;
        let stall = hook::stall(self.data.path()).unwrap_or(STALL);
        let total = copied.len() as u64;
        let (mut best_files, mut best_bytes) = (0u64, 0u64);
        let mut progressed = Instant::now();
        let (mut interval, mut next_look, mut last_pass) = (POLL_FIRST, Instant::now(), None);
        loop {
            if self.stopping() {
                return Err(Stop::Interrupted);
            }
            let info = self.control.space_info(space);
            if let Some(run) = info.as_ref().and_then(|i| i.run_state.clone()) {
                match run {
                    SpaceRun::Paused { reason, .. } => return Err(failed(format!("the team space stopped syncing: {reason}"))),
                    SpaceRun::NoAccess { .. } => return Err(failed("this account has lost access to the new team space")),
                    SpaceRun::Unavailable { message } => return Err(failed(message)),
                    _ => {}
                }
            }
            // The tree is read when a pass of the team space has ended (its answer is new) and otherwise less and less often.
            let pass_ended = info.as_ref().map(|i| i.last_sync_ms) != last_pass;
            if pass_ended || Instant::now() >= next_look {
                last_pass = info.as_ref().map(|i| i.last_sync_ms);
                let mut grew = false;
                match tree_matches(&bound, copied, &self.scan.dirs).await {
                    Ok(matched) => {
                        self.status.done = matched.files;
                        self.publish(false);
                        if matched.files == total && matched.dirs_missing == 0 {
                            return Ok(());
                        }
                        if matched.files > best_files {
                            best_files = matched.files;
                            grew = true;
                        }
                    }
                    // A failed look is tried again; a long failure is the stall below.
                    Err(e) => crate::runlog::note(format_args!("looking at the new team space failed: {e}")),
                }
                if let Ok(usage) = transport.admin_get::<craft_cloud_proto::api::SpaceUsage>(&format!("spaces/{space}/usage"), &[]).await
                    && usage.stored_bytes > best_bytes
                {
                    best_bytes = usage.stored_bytes;
                    grew = true;
                }
                if grew {
                    progressed = Instant::now();
                    interval = POLL_FIRST;
                } else {
                    interval = (interval * 2).min(POLL_SLOWEST);
                }
                next_look = Instant::now() + interval;
            }
            if let Some(why) = self.skipped_by_team_pass(space) {
                return Err(failed(format!("the team space can't take every file: {why}")));
            }
            if progressed.elapsed() > stall {
                return Err(failed(format!(
                    "the team space has taken {best_files} of {total} files and stored nothing more for {} minutes",
                    (stall.as_secs() / 60).max(1)
                )));
            }
            self.sleep(Duration::from_millis(200)).await?;
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
        self.blocking(move |done, cancel, control| {
            verify_all(&root, &copied_list, &done, &|| cancel.load(Ordering::SeqCst) || control.spaces_handle().is_none())
        })
        .await
    }

    /// Whether the team space still is what step 5 saw. Asked of the server directly (never of the agent's own list, which may be
    /// old): the space is listed, active, not archived and the account may change it; an answer that does not come is a refusal.
    /// With `files` the server's tree must also hold every one of them with the manifest it was verified as. Anything else stops
    /// the deletion; what was deleted already is in the team space.
    async fn recheck(&self, space: SpaceId, files: Option<&[Copied]>) -> Result<(), Stop> {
        let transport = HttpTransport::new(self.account.server.clone(), &self.account.token).map_err(failed)?;
        let list = transport.spaces().await.map_err(|e| failed(format!("the team space could not be checked ({e}); nothing more was deleted")))?;
        let Some(item) = list.spaces.iter().find(|i| i.id == space) else {
            return Err(failed("the team space is not listed any more; nothing more was deleted"));
        };
        if item.state != "active" || item.archived || item.role == craft_cloud_proto::api::SpaceRole::Viewer {
            return Err(failed("the team space is archived, or this account can no longer change it; nothing more was deleted"));
        }
        let Some(files) = files else { return Ok(()) };
        let bound = transport.bound_to(space).map_err(failed)?;
        match tree_matches(&bound, files, &self.scan.dirs).await {
            Ok(m) if m.files == files.len() as u64 && m.dirs_missing == 0 => Ok(()),
            Ok(m) => Err(failed(format!(
                "the team space no longer holds {} of the {} files still to be deleted; nothing more was deleted",
                files.len() as u64 - m.files,
                files.len()
            ))),
            Err(e) => Err(failed(format!("the team space could not be checked ({e}); nothing more was deleted"))),
        }
    }

    /// Whether the old folder, and every folder above it up to the personal folder, still is the one that was scanned.
    async fn folder_intact(&self) -> bool {
        let chain = self.scan.chain.clone();
        tokio::task::spawn_blocking(move || chain_intact(&chain)).await.unwrap_or(false)
    }

    /// Step 7: deletes the old folder's files here, each only while it is as it was verified. Before the first batch the team space
    /// is checked in full; before every batch (at most [`TRASH_BATCH`] files or [`TRASH_RECHECK`]) the old folder is checked and the
    /// team space is asked whether it is still there to take what is deleted, and every [`TRASH_FULL_CHECK`] the tree is read again.
    async fn trash(&mut self, space: SpaceId, copied: Vec<Copied>) -> Result<(), Stop> {
        let root = self.scan.root.clone();
        let quarantine = quarantine_dir(&self.account, &root);
        let hooks = Hooks { after: hook::after_quarantine(self.data.path()), between: hook::between_steps(self.data.path()) };
        let batch_size = hook::trash_batch(self.data.path()).unwrap_or(TRASH_BATCH).max(1);
        // A file edited in the team folder stays here: only its version is beside the edit there.
        let (edited, copied): (Vec<Copied>, Vec<Copied>) = copied.into_iter().partition(|c| c.team_edit.is_some());
        let mut stats = TrashStats { edited: edited.len() as u64, ..TrashStats::default() };
        let mut at = 0usize;
        let mut last_full: Option<Instant> = None;
        while at < copied.len() {
            if self.stopping() {
                return Err(Stop::Interrupted);
            }
            if !self.folder_intact().await {
                return Err(failed("the old folder was moved or replaced since it was checked; nothing more was deleted"));
            }
            let remaining = copied.get(at..).unwrap_or_default();
            let full = last_full.is_none_or(|t| t.elapsed() >= TRASH_FULL_CHECK);
            self.recheck(space, full.then_some(remaining)).await?;
            if full {
                last_full = Some(Instant::now());
            }
            let end = (at + batch_size).min(copied.len());
            let items: Vec<Copied> = copied.get(at..end).unwrap_or_default().to_vec();
            let (root_b, quarantine_b, chain) = (root.clone(), quarantine.clone(), self.scan.chain.clone());
            let batch = self
                .blocking(move |done, cancel, control| {
                    Ok(delete_batch(
                        &root_b,
                        &quarantine_b,
                        &chain,
                        &items,
                        &done,
                        &|| cancel.load(Ordering::SeqCst) || control.spaces_handle().is_none(),
                        hooks,
                    ))
                })
                .await?;
            stats.add(&batch.stats);
            if batch.replaced {
                return Err(failed("the old folder was moved or replaced since it was checked; nothing more was deleted"));
            }
            if batch.processed == 0 {
                return Err(Stop::Interrupted);
            }
            at += batch.processed;
            self.status.done = at as u64;
            self.publish(false);
        }
        if stats.stranded > 0 {
            // Files that could not be put back stay in the quarantine, and the work is not done.
            return Err(failed(stranded_words(&[quarantine])));
        }
        let (root_f, dirs, quarantine_f) = (root.clone(), self.scan.dirs.clone(), quarantine.clone());
        let others = tokio::task::spawn_blocking(move || finish_old(&root_f, &dirs, &quarantine_f, stats.kept())).await.map_err(failed)?;
        // The personal sync sees a deleted folder: soon, and as any deletion.
        self.control.request_pass();
        self.status.step = FromFolderStep::Done;
        self.status.kept = stats.kept();
        self.status.done = 0;
        self.status.total = 0;
        self.status.failed_at = None;
        self.status.message = stats.words(others);
        self.publish(true);
        Ok(())
    }
}

/// What the deletion left, by reason.
#[derive(Debug, Default, Clone, Copy)]
struct TrashStats {
    /// Changed (or moved, or its folder swapped for a link) after the check: not in the team space as it is now.
    changed: u64,
    /// Could not be taken out of the folder or deleted: it is where it was, and in the team space.
    locked: u64,
    /// Could not be put back out of the quarantine.
    stranded: u64,
    /// Edited in the team folder after it was copied: the old folder's version is beside the edit there, and the old file stays here.
    edited: u64,
}

impl TrashStats {
    fn add(&mut self, other: &TrashStats) {
        self.changed += other.changed;
        self.locked += other.locked;
        self.stranded += other.stranded;
        self.edited += other.edited;
    }

    /// The files that stayed because of the deletion (not those the sync never carries).
    fn kept(&self) -> u64 {
        self.changed + self.locked + self.edited
    }

    /// What to tell the person about what stayed; `others` is how many other files the old folder still holds (kinds the sync does
    /// not carry, such as `.tmp` files, or files made meanwhile).
    fn words(&self, others: u64) -> Option<String> {
        let plural = |n: u64, one: &str, many: &str| if n == 1 { one.to_owned() } else { many.to_owned() };
        let mut parts: Vec<String> = Vec::new();
        if self.changed > 0 {
            parts.push(format!(
                "{} {} changed after the copy and {} not in the team space as {} now",
                self.changed,
                plural(self.changed, "file", "files"),
                plural(self.changed, "is", "are"),
                plural(self.changed, "it is", "they are")
            ));
        }
        if self.edited > 0 {
            parts.push(format!(
                "{} {} edited in the team space after {} copied: the edit stays there, your version of {} is beside it as a conflict copy, and {} here stays as it is",
                self.edited,
                plural(self.edited, "file was", "files were"),
                plural(self.edited, "it was", "they were"),
                plural(self.edited, "it", "each"),
                plural(self.edited, "the old file", "each old file")
            ));
        }
        if self.locked > 0 {
            parts.push(format!(
                "{} {} could not be deleted (a program holds {}, or {} could not be moved aside): {} in the team space, and you can delete {} yourself",
                self.locked,
                plural(self.locked, "file", "files"),
                plural(self.locked, "it", "them"),
                plural(self.locked, "it", "they"),
                plural(self.locked, "it is", "they are"),
                plural(self.locked, "it", "them")
            ));
        }
        if others > 0 {
            parts.push(format!(
                "{others} other {} stayed: of a kind that does not sync (such as .tmp files), or made meanwhile",
                plural(others, "file", "files")
            ));
        }
        (!parts.is_empty()).then(|| format!("{}.", parts.join("; ")))
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

/// The manifest of a file's content, the way the engine makes it, while `sink` receives every chunk's bytes. `cancelled` is looked
/// at for every chunk, so a big file does not hold the end of a session up.
fn chunk_file(path: &Path, cancelled: &dyn Fn() -> bool, mut sink: impl FnMut(&[u8]) -> io::Result<()>) -> Result<(ManifestId, u64), CopyStop> {
    let file = std::fs::File::open(path).map_err(|e| io_failed(path, &e))?;
    let chunker = Chunker::new(FASTCDC_V1).map_err(|e| CopyStop::Failed(e.to_string()))?;
    let summary = chunk_reader::<_, io::Error, _>(&chunker, io::BufReader::new(file), |chunk| {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "the session ended"));
        }
        sink(chunk.data)
    })
    .map_err(|e| if cancelled() { CopyStop::Interrupted } else { io_failed(path, &e) })?;
    let refs = summary.chunks.iter().map(|c| ChunkRef { id: ChunkId::from_bytes(c.hash), len: c.len }).collect();
    let params = ChunkerParamsId::new(FASTCDC_V1.name).map_err(|e| CopyStop::Failed(e.to_string()))?;
    let manifest = Manifest::new(params, summary.size, FileHash::from_bytes(summary.file_hash), refs)
        .map_err(|e| CopyStop::Failed(format!("{}: {e}", path.display())))?;
    Ok((manifest.id(), summary.size))
}

fn same_stat(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

/// What came of copying one file.
enum Landing {
    /// The file is in the team folder as it is in the old folder.
    Placed(ManifestId),
    /// The team folder's file was edited there since this job wrote it: it stays as it is, and the old folder's version went
    /// beside it as the conflict copy with this name; the last is the manifest of the team file that was there (when it could be read).
    Beside(ManifestId, String, Option<ManifestId>),
}

/// How often the copy saves what it wrote while it goes on (a crash then loses at most this much of the record).
const WRITTEN_SAVE_EVERY: Duration = Duration::from_secs(1);

/// The record key under which the conflict copy made for `key`'s path is noted (a NUL cannot be in a path, so it is no file's key).
fn beside_key(key: &str) -> String {
    format!("\0beside\0{key}")
}

/// The conflict copy noted for a path: the team version it was made beside, the old version it holds, and its name.
fn beside_note(seen: &ManifestId, mine: &ManifestId, name: &str) -> String {
    format!("{seen}|{mine}|{name}")
}

/// What the copy's hooks and timing are (see [`hook`]).
#[derive(Clone, Copy)]
struct CopyHooks {
    delay: Option<Duration>,
    before_replace: Option<hook::Callback>,
    save_every: Duration,
}

/// The extension of a team file the copy moved aside, and of the note beside it that names where it belongs.
const ASIDE_EXT: &str = "cc-aside";

/// The file in the team space's own folder (in the data folder) that records, for each path, the manifest this job last wrote there:
/// what tells a team file of this job's own (which a newer old version may replace) from one somebody edited.
const WRITTEN_FILE: &str = "from-folder-written.json";

type Written = std::collections::BTreeMap<String, String>;

fn load_written(path: &Path) -> Written {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn save_written(path: &Path, written: &Written) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec(written)
        && let Err(e) = write_atomic(path, &json, true)
    {
        crate::runlog::note(format_args!("can't write what the copy wrote into the team folder: {e}"));
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_all(
    root: &Path,
    team: &Path,
    dirs: &[(Vec<std::ffi::OsString>, Vec<String>)],
    files: &[FileEntry],
    done: &AtomicU64,
    hooks: CopyHooks,
    device: &str,
    written: &Mutex<Written>,
    record: Option<&Path>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Copied>, CopyStop> {
    // The team folder as it is now: it and every folder written into must stay what they are (no link, the same folder), and
    // everything is written through this resolved path, never through the one the agent was given.
    std::fs::create_dir_all(team).map_err(|e| io_failed(team, &e))?;
    let team_real = std::fs::canonicalize(team).map_err(|e| io_failed(team, &e))?;
    let team_chain = chain_of(&team_real, &team_real);
    let tmp = team_real.join(AGENT_DIR).join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| io_failed(&tmp, &e))?;
    // Leftovers of an earlier try.
    if let Ok(entries) = std::fs::read_dir(&tmp) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == COPY_EXT) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    recover_asides(&tmp, &team_real, written, cancelled);
    for (os, _) in dirs {
        let dir = join(&team_real, os);
        if !chain_intact(&team_chain) || !ancestors_are_real(&team_real, os) {
            return Err(unsafe_destination());
        }
        std::fs::create_dir_all(&dir).map_err(|e| io_failed(&dir, &e))?;
        if !components_are_real(&team_real, os) {
            return Err(unsafe_destination());
        }
    }
    let mut copied = Vec::with_capacity(files.len());
    let mut last_save = Instant::now();
    for entry in files {
        if cancelled() {
            return Err(CopyStop::Interrupted);
        }
        let source = join(root, &entry.os);
        let dest = join(&team_real, &entry.os);
        let key = entry.names.join("/");
        let (ours, beside) = {
            let known = written.lock().unwrap_or_else(PoisonError::into_inner);
            (known.get(&key).cloned(), known.get(&beside_key(&key)).cloned())
        };
        let mut attempt = 0;
        let landing = loop {
            attempt += 1;
            let guard = || chain_intact(&team_chain) && ancestors_are_real(&team_real, &entry.os);
            let known = Known { ours: ours.as_deref(), beside: beside.as_deref(), key: &key };
            match copy_file(&source, &dest, &tmp, hooks, known, device, &guard, cancelled)? {
                Some(landing) => break landing,
                None if attempt < 4 => std::thread::sleep(Duration::from_millis(300)),
                None => return Err(CopyStop::Failed(format!("{} keeps changing while it is copied; nothing was deleted", source.display()))),
            }
        };
        {
            let mut known = written.lock().unwrap_or_else(PoisonError::into_inner);
            match landing {
                Landing::Placed(manifest) => {
                    known.insert(key, manifest.to_string());
                    copied.push(Copied { entry: entry.clone(), manifest, team_edit: None });
                }
                Landing::Beside(manifest, name, seen) => {
                    if let Some(seen) = seen {
                        known.insert(beside_key(&key), beside_note(&seen, &manifest, &name));
                    }
                    copied.push(Copied { entry: entry.clone(), manifest, team_edit: Some(name) });
                }
            }
        }
        // What was written is saved as it goes, so a crash does not make this job's own files look like team edits.
        if let Some(record) = record
            && last_save.elapsed() >= hooks.save_every
        {
            save_written(record, &written.lock().unwrap_or_else(PoisonError::into_inner));
            last_save = Instant::now();
        }
        done.fetch_add(1, Ordering::Relaxed);
    }
    Ok(copied)
}

/// Puts back the team files an earlier copy moved aside and did not finish with (a crash between moving the old file aside and
/// landing the new one): to their place if it is free; if something is there now, the aside file goes if it is this job's own old
/// content, and is kept next to it (under a `.cc-kept` name) if it is anything else.
fn recover_asides(tmp: &Path, team_real: &Path, written: &Mutex<Written>, cancelled: &dyn Fn() -> bool) {
    let Ok(entries) = std::fs::read_dir(tmp) else { return };
    for entry in entries.flatten() {
        let note = entry.path();
        let name = note.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(stem) = name.strip_suffix(&format!(".{ASIDE_EXT}.path")) else { continue };
        let aside = tmp.join(format!("{stem}.{ASIDE_EXT}"));
        let Ok(names) = std::fs::read_to_string(&note) else { continue };
        if !aside.exists() {
            let _ = std::fs::remove_file(&note);
            continue;
        }
        let os: Vec<std::ffi::OsString> = names.split('/').map(std::ffi::OsString::from).collect();
        let place = join(team_real, &os);
        if names.is_empty() || !ancestors_are_real(team_real, &os) {
            continue;
        }
        let ours = written.lock().unwrap_or_else(PoisonError::into_inner).get(&names).cloned();
        let free = matches!(std::fs::symlink_metadata(&place), Err(e) if e.kind() == io::ErrorKind::NotFound);
        let own_old = !free && matches!(chunk_file(&aside, cancelled, |_| Ok(())), Ok((m, _)) if Some(m.to_string()) == ours);
        let result = if own_old { std::fs::remove_file(&aside) } else { put_back(&aside, &place) };
        if result.is_ok() {
            let _ = std::fs::remove_file(&note);
        }
    }
}

/// The destination of a copy, as it is when the copy starts.
enum Dest {
    Absent,
    /// A file, with the manifest of its content (`None` if it could not be read).
    File(Option<ManifestId>),
}

/// What identifies a file's state without reading it: its file ID, size and modification time.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStat {
    id: Option<u64>,
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStat {
    fn of(path: &Path, meta: &std::fs::Metadata) -> Self {
        Self { id: identity(path, meta), len: meta.len(), modified: meta.modified().ok() }
    }
}

/// A team file as it was read at the start of its copy: the manifest of its content and its [`FileStat`].
#[derive(Clone, Copy)]
struct Read {
    manifest: ManifestId,
    stat: Option<FileStat>,
}

/// What the record knows of one path.
#[derive(Clone, Copy)]
struct Known<'a> {
    /// The manifest this job last wrote there.
    ours: Option<&'a str>,
    /// The conflict copy made beside an edit there ([`beside_note`]).
    beside: Option<&'a str>,
    /// The path, as the record names it.
    key: &'a str,
}

/// Copies one file into the team folder. A file that is not there is put there (never over a name that was taken meanwhile). A file
/// that already has exactly the old file's content stays. A file the job wrote itself (`known.ours` is the manifest it recorded) is
/// replaced by the newer old file, but never blindly: it is moved aside first, looked at, and only content that is still the job's
/// own is replaced. Any other file there was edited in the team folder: it stays as it is, and the old file's version is put beside
/// it as a conflict copy (once: a conflict copy already made for the same two versions is not made again). `None`: the old file
/// changed while it was read; the copy was thrown away.
#[allow(clippy::too_many_arguments)]
fn copy_file(
    source: &Path,
    dest: &Path,
    tmp: &Path,
    hooks: CopyHooks,
    known: Known<'_>,
    device: &str,
    guard: &dyn Fn() -> bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Landing>, CopyStop> {
    let delay = hooks.delay;
    if !guard() {
        return Err(unsafe_destination());
    }
    let failed_at = |path: &Path, e: &dyn std::fmt::Display| CopyStop::Failed(format!("{}: {e}", path.display()));
    let before = std::fs::metadata(source).map_err(|e| io_failed(source, &e))?;
    let mut dest_stat = None;
    let state = match std::fs::symlink_metadata(dest) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Dest::Absent,
        Ok(m) if m.is_file() && !is_link(&m) => {
            dest_stat = Some(FileStat::of(dest, &m));
            Dest::File(chunk_file(dest, cancelled, |_| Ok(())).ok().map(|(m, _)| m))
        }
        _ => return Err(CopyStop::Failed(format!("{}: something other than a file is there", dest.display()))),
    };
    let temp = tmp.join(format!("{}.{COPY_EXT}", random_hex(12).map_err(|e| CopyStop::Failed(e.to_string()))?));
    let result = (|| -> Result<Option<Landing>, CopyStop> {
        let mut out = std::fs::File::options().write(true).create_new(true).open(&temp).map_err(|e| failed_at(&temp, &e))?;
        let (manifest, size) = chunk_file(source, cancelled, |data| {
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
            out.write_all(data)
        })?;
        out.flush().map_err(|e| failed_at(&temp, &e))?;
        if out.metadata().map(|m| m.len()).ok() != Some(size) {
            return Err(CopyStop::Failed(format!("{}: the copy has the wrong size", temp.display())));
        }
        let after = std::fs::metadata(source).map_err(|e| io_failed(source, &e))?;
        if !same_stat(&before, &after) {
            return Ok(None);
        }
        if matches!(state, Dest::File(Some(have)) if have == manifest) {
            // The destination has this content already.
            return Ok(Some(Landing::Placed(manifest)));
        }
        if let Ok(modified) = before.modified() {
            out.set_modified(modified).map_err(|e| failed_at(&temp, &e))?;
        }
        drop(out);
        // The permissions (the executable bit) go with the file.
        std::fs::set_permissions(&temp, before.permissions()).map_err(|e| failed_at(&temp, &e))?;
        // Looked at once more right before the file lands.
        if !guard() {
            return Err(unsafe_destination());
        }
        match state {
            Dest::Absent => {
                land_noreplace(&temp, dest).map_err(|e| {
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        CopyStop::Failed(format!("{}: a file appeared there while it was copied; try again", dest.display()))
                    } else {
                        io_failed(dest, &e)
                    }
                })?;
                Ok(Some(Landing::Placed(manifest)))
            }
            Dest::File(Some(have)) if known.ours == Some(have.to_string().as_str()) => {
                // An older copy this job wrote itself: replaced, if it still is that.
                if let Some(hook) = hooks.before_replace {
                    hook(dest);
                }
                let read = Read { manifest: have, stat: dest_stat };
                replace_own(&temp, dest, tmp, &manifest, read, known, device, cancelled).map(Some)
            }
            Dest::File(seen) => {
                // Edited in the team folder (or not known to be this job's): it stays, and this version goes beside it.
                land_beside(&temp, dest, &manifest, seen, known, device).map(Some)
            }
        }
    })();
    // A copy that was put in place is gone from `temp`; anything else is cleaned up.
    let _ = std::fs::remove_file(&temp);
    result
}

/// Puts the finished copy `temp` beside the team file `dest` as a conflict copy (never over anything), unless one with the same
/// content was made beside the same team version already.
fn land_beside(
    temp: &Path,
    dest: &Path,
    manifest: &ManifestId,
    seen: Option<ManifestId>,
    known: Known<'_>,
    device: &str,
) -> Result<Landing, CopyStop> {
    if let (Some(seen), Some(note)) = (seen.as_ref(), known.beside) {
        let mut parts = note.splitn(3, '|');
        if let (Some(then), Some(mine), Some(name)) = (parts.next(), parts.next(), parts.next())
            && then == seen.to_string()
            && mine == manifest.to_string()
            && std::fs::symlink_metadata(dest.with_file_name(name)).is_ok_and(|m| m.is_file())
        {
            return Ok(Landing::Beside(*manifest, name.to_owned(), Some(*seen)));
        }
    }
    let original = dest.file_name().and_then(|n| n.to_str()).and_then(|n| Name::new(n).ok());
    let Some(original) = original else {
        return Err(CopyStop::Failed(format!("{}: the name can't be used for a conflict copy", dest.display())));
    };
    for counter in 1..=50 {
        let name = craft_cloud_proto::conflict_name(&original, false, device, crate::cli::conflict_stamp(), counter)
            .map_err(|e| CopyStop::Failed(e.to_string()))?;
        match land_noreplace(temp, &dest.with_file_name(name.as_str())) {
            Ok(()) => return Ok(Landing::Beside(*manifest, name.as_str().to_owned(), seen)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_failed(dest, &e)),
        }
    }
    Err(CopyStop::Failed(format!("{}: too many conflict copies are there", dest.display())))
}

/// Replaces the job's own older team file `dest` by the finished copy `temp` without ever overwriting what somebody wrote there
/// meanwhile: the old file is moved aside first (a rename, with a note beside it that says where it belongs, so a crash is put right
/// at the next start), the moved file is checked (read again unless it plainly is the file that was read at the start: the same
/// file ID, size and time), and only if it still is the content the job wrote does the new copy land (by a link that fails on a
/// taken name). Anything else was edited: it is put back (never over anything) and the new copy goes beside it.
#[allow(clippy::too_many_arguments)]
fn replace_own(
    temp: &Path,
    dest: &Path,
    tmp: &Path,
    manifest: &ManifestId,
    read: Read,
    known: Known<'_>,
    device: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<Landing, CopyStop> {
    let id = random_hex(12).map_err(|e| CopyStop::Failed(e.to_string()))?;
    let aside = tmp.join(format!("{id}.{ASIDE_EXT}"));
    let note = tmp.join(format!("{id}.{ASIDE_EXT}.path"));
    write_atomic(&note, known.key.as_bytes(), false).map_err(|e| io_failed(&note, &e))?;
    let mut moved = Err(io::Error::from(io::ErrorKind::NotFound));
    for _ in 0..RENAME_TRIES {
        moved = std::fs::rename(dest, &aside);
        match &moved {
            // Another program holds the file for a moment (the team pass reading it): waited for.
            Err(e) if e.kind() != io::ErrorKind::NotFound => std::thread::sleep(Duration::from_millis(250)),
            _ => break,
        }
    }
    match moved {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Gone meanwhile (deleted in the team space): there is nothing to protect, the new copy lands on the free name.
            let _ = std::fs::remove_file(&note);
            return match land_noreplace(temp, dest) {
                Ok(()) => Ok(Landing::Placed(*manifest)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => land_beside(temp, dest, manifest, None, known, device),
                Err(e) => Err(io_failed(dest, &e)),
            };
        }
        Err(e) => {
            let _ = std::fs::remove_file(&note);
            return Err(io_failed(dest, &e));
        }
    }
    // The team file is absent from its place from here until the new copy lands, and the team pass that reads it then fails: so
    // what was moved is read again only when it is not plainly the file that was read at the start (the same file ID, size and
    // modification time); then the content is the one that was read.
    let plainly_the_same = read.stat.is_some()
        && std::fs::symlink_metadata(&aside).is_ok_and(|m| m.is_file() && !is_link(&m) && Some(FileStat::of(&aside, &m)) == read.stat);
    let moved_content = if plainly_the_same { Ok(read.manifest) } else { chunk_file(&aside, cancelled, |_| Ok(())).map(|(m, _)| m) };
    let own = matches!(&moved_content, Ok(m) if Some(m.to_string().as_str()) == known.ours);
    let put_aside_back = || match put_back(&aside, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(&note);
            Ok(())
        }
        // It stays in the team folder's own hidden folder, with its note: the next start puts it back.
        Err(e) => Err(CopyStop::Failed(format!(
            "{}: the file that was moved aside could not be put back ({e}); it is kept in {}",
            dest.display(),
            aside.display()
        ))),
    };
    if own {
        match land_noreplace(temp, dest) {
            Ok(()) => {
                let _ = std::fs::remove_file(&aside);
                let _ = std::fs::remove_file(&note);
                Ok(Landing::Placed(*manifest))
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // A file took the name in this instant: the job's own old copy is dropped, the new version goes beside what is there.
                let _ = std::fs::remove_file(&aside);
                let _ = std::fs::remove_file(&note);
                land_beside(temp, dest, manifest, None, known, device)
            }
            Err(e) => {
                put_aside_back()?;
                Err(io_failed(dest, &e))
            }
        }
    } else {
        let seen = moved_content.as_ref().ok().copied();
        match &moved_content {
            Err(CopyStop::Interrupted) => {
                put_aside_back()?;
                return Err(CopyStop::Interrupted);
            }
            Err(CopyStop::Failed(why)) => {
                put_aside_back()?;
                return Err(CopyStop::Failed(why.clone()));
            }
            Ok(_) => {}
        }
        put_aside_back()?;
        land_beside(temp, dest, manifest, seen, known, device)
    }
}

/// What the copy says when the team folder, or a folder in it, is not the real folder it was.
fn unsafe_destination() -> CopyStop {
    CopyStop::Failed(
        "the team folder, or a folder in it, was moved or replaced by a link while the files were copied; nothing more was written there".into(),
    )
}

/// Whether every folder of `os` below `root` (the last one too) is a real folder now (no link of any kind).
fn components_are_real(root: &Path, os: &[std::ffi::OsString]) -> bool {
    let mut at = root.to_owned();
    for name in os {
        at.push(name);
        if !std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir() && !is_link(&m)) {
            return false;
        }
    }
    true
}

/// Whether the error of a hard link says that this volume has none (FAT, exFAT and many network shares), as opposed to a failure of
/// this one file (access denied, too many links, a path error).
fn hard_links_unsupported(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    // Windows: ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, ERROR_CALL_NOT_IMPLEMENTED. Linux: EPERM (what FAT gives) and
    // EOPNOTSUPP. Other Unix: EPERM and ENOTSUP.
    let codes: &[i32] = if cfg!(windows) {
        &[1, 50, 120]
    } else if cfg!(any(target_os = "linux", target_os = "android")) {
        &[1, 95]
    } else if cfg!(unix) {
        &[1, 45]
    } else {
        &[]
    };
    e.raw_os_error().is_some_and(|code| codes.contains(&code))
}

/// A hard link (`hook::set_hard_link_error` can make it fail in tests).
fn link_file(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(kind) = hook::hard_link_error(to) {
        return Err(io::Error::from(kind));
    }
    std::fs::hard_link(from, to)
}

/// The move for a volume without hard links: only a name that is absent (looked at just before) is renamed to, so a name that is
/// taken is an `AlreadyExists` error. Another program that takes the name between the look and the rename is the narrow window
/// this fallback has; the hard link has none.
fn rename_if_absent(from: &Path, to: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(from, to)
}

/// Puts the finished copy in a place that is free: a hard link (which fails when the name is taken) and then the temporary name goes.
/// A name that is taken is an `AlreadyExists` error, never an overwrite. Only a volume that has no hard links falls back to
/// [`rename_if_absent`]; any other error of the link is the failure it is.
fn land_noreplace(temp: &Path, dest: &Path) -> io::Result<()> {
    match link_file(temp, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(temp);
            Ok(())
        }
        Err(e) if hard_links_unsupported(&e) => rename_if_absent(temp, dest),
        Err(e) => Err(e),
    }
}

/// Whether every copied file of the old folder is as it was copied: same manifest of its content now. `Ok(false)` when not.
fn verify_all(root: &Path, copied: &[Copied], done: &AtomicU64, cancelled: &dyn Fn() -> bool) -> Result<bool, CopyStop> {
    for item in copied {
        if cancelled() {
            return Err(CopyStop::Interrupted);
        }
        let path = join(root, &item.entry.os);
        let manifest = match chunk_file(&path, cancelled, |_| Ok(())) {
            Ok((manifest, _)) => manifest,
            Err(CopyStop::Interrupted) => return Err(CopyStop::Interrupted),
            Err(CopyStop::Failed(_)) => return Ok(false),
        };
        if manifest != item.manifest {
            return Ok(false);
        }
        done.fetch_add(1, Ordering::Relaxed);
    }
    Ok(true)
}

// ---------------------------------------------------------------------------------------------------------------------
// Deleting the old folder (blocking)

/// Where a file waits between being taken out of the old folder and being deleted: inside the personal folder's own `.craft-cloud`
/// (the sync ignores it, and it is on the same volume), one folder for each old folder, with a marker that says which.
fn quarantine_dir(account: &Account, root: &Path) -> PathBuf {
    let key: String = blake3::hash(root.to_string_lossy().as_bytes()).to_hex().chars().take(16).collect();
    quarantine_base(account).join(key)
}

fn quarantine_base(account: &Account) -> PathBuf {
    account.folder.join(AGENT_DIR).join("from-folder")
}

/// The marker beside a quarantine folder (`<key>.root`, outside the folder, which mirrors the old folder's tree and so can hold any
/// file name) that names the old folder its files came from.
fn marker_of(quarantine: &Path) -> PathBuf {
    let mut name = quarantine.file_name().map(std::ffi::OsString::from).unwrap_or_default();
    name.push(".root");
    quarantine.with_file_name(name)
}

/// Makes sure the marker of `quarantine` names `root`, written atomically. False when it cannot be had: nothing is then moved into
/// the quarantine, because nothing could be put back from it.
fn marker_ready(quarantine: &Path, root: &Path) -> bool {
    let text = root.to_string_lossy().into_owned();
    // A root that is not text would be written changed: it is refused.
    if root.to_str().is_none() {
        return false;
    }
    let marker = marker_of(quarantine);
    if std::fs::create_dir_all(quarantine).is_err() {
        return false;
    }
    if std::fs::read_to_string(&marker).is_ok_and(|have| have == text) {
        return true;
    }
    write_atomic(&marker, text.as_bytes(), true).is_ok() && std::fs::read_to_string(&marker).is_ok_and(|have| have == text)
}

/// The old folder a quarantine folder's marker names, if the marker is sound: an absolute path without `.` or `..`, inside the
/// personal folder (and not it), and the one the quarantine folder's name was made from.
fn marker_root(quarantine: &Path, personal: &Path) -> Option<PathBuf> {
    let root = PathBuf::from(std::fs::read_to_string(marker_of(quarantine)).ok()?);
    let plain = root.components().all(|c| !matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir));
    let key: String = blake3::hash(root.to_string_lossy().as_bytes()).to_hex().chars().take(16).collect();
    let named = quarantine.file_name().is_some_and(|n| n == std::ffi::OsStr::new(&key));
    (root.is_absolute() && plain && root != personal && root.starts_with(personal) && named).then_some(root)
}

/// Whether `path` is a link of any kind (a symlink, a junction or any other reparse point).
fn is_link(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

/// What tells one folder from another that was put in its place: the file ID the sync uses for it (device and inode on Unix, the NTFS
/// file ID and volume serial on Windows), `None` when it cannot be read.
fn identity(path: &Path, meta: &std::fs::Metadata) -> Option<u64> {
    crate::disk::local_id(meta, path)
}

/// The old folder and every folder above it up to the personal folder, as they were when it was scanned: the resolved path and the
/// identity of each.
type Chain = Vec<(PathBuf, Option<u64>)>;

/// Records the chain of `root` up to `personal` (both resolved).
fn chain_of(root: &Path, personal: &Path) -> Chain {
    let mut chain = Vec::new();
    let mut at = Some(root.to_owned());
    while let Some(path) = at {
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            chain.push((path.clone(), identity(&path, &meta)));
        }
        at = if path == personal { None } else { path.parent().map(Path::to_owned) };
    }
    chain
}

/// Whether every folder of the chain still is the real folder it was: no link of any kind, the same identity, and the path still
/// resolves to itself (a folder moved away and replaced by a link or by a copy is none of these).
fn chain_intact(chain: &Chain) -> bool {
    !chain.is_empty()
        && chain.iter().all(|(path, id)| {
            std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !is_link(&m) && identity(path, &m) == *id)
                && std::fs::canonicalize(path).is_ok_and(|resolved| resolved == *path)
        })
}

/// Whether every folder between `root` and the file `os` names is a real folder now (no link of any kind): a folder swapped for a
/// link since the scan would lead the deletion out of the old folder.
fn ancestors_are_real(root: &Path, os: &[std::ffi::OsString]) -> bool {
    let mut at = root.to_owned();
    for name in os.iter().take(os.len().saturating_sub(1)) {
        at.push(name);
        if !std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir() && !is_link(&m)) {
            return false;
        }
    }
    true
}

/// A move that never replaces: the file gets its new name as a hard link (which fails when the name is taken, on every platform),
/// and only then loses the old one. A volume without hard links (and anything that is not a plain file, a folder that took a file's
/// place) is moved by [`rename_if_absent`]. Any other error leaves the file where it is.
fn move_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let plain = std::fs::symlink_metadata(from).is_ok_and(|m| m.is_file() && !is_link(&m));
    if !plain {
        return rename_if_absent(from, to);
    }
    match link_file(from, to) {
        Ok(()) => {}
        Err(e) if hard_links_unsupported(&e) => return rename_if_absent(from, to),
        Err(e) => return Err(e),
    }
    if let Err(e) = std::fs::remove_file(from) {
        let _ = std::fs::remove_file(to);
        return Err(e);
    }
    Ok(())
}

/// Moves a quarantined file back to `place` (making the folders above it if they are gone); if something has taken the place, next
/// to it as `<name>.cc-kept`, `<name>.cc-kept-2` and so on. Never over anything. An error leaves the file where it is.
fn put_back(held: &Path, place: &Path) -> io::Result<()> {
    if let Some(parent) = place.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = place.file_name().map(std::ffi::OsString::from).unwrap_or_default();
    for n in 0..100u32 {
        let target = if n == 0 {
            place.to_owned()
        } else {
            let mut other = name.clone();
            other.push(if n == 1 { ".cc-kept".to_owned() } else { format!(".cc-kept-{n}") });
            place.with_file_name(other)
        };
        match move_noreplace(held, &target) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("too many files are in the way"))
}

/// How a file's deletion came out.
#[derive(Debug)]
enum Deleted {
    Gone,
    Changed,
    /// Could not be taken out of the old folder, or could not be deleted: it is where it was, in the team space.
    Locked,
    /// Could not be put back: it is still in the quarantine.
    Stranded,
    /// The old folder (or a folder above it) is not the one that was scanned any more.
    Replaced,
}

/// The test hooks of a deletion (see [`hook`]).
#[derive(Clone, Copy, Default)]
struct Hooks {
    after: Option<hook::Callback>,
    between: Option<hook::Callback>,
}

/// What a batch of deletions came to.
struct Batch {
    processed: usize,
    stats: TrashStats,
    /// The old folder was replaced: nothing more was deleted.
    replaced: bool,
}

/// Deletes up to a batch of files of `items` (stopping after [`TRASH_RECHECK`]), each one safely: the chain of folders to the old
/// folder is checked, its folders are checked for links, the file is moved into `quarantine` (never over anything), read there once
/// more and compared with the manifest that was verified, and only a match is deleted; a file that no longer matches is moved back
/// (next to its place, under another name, if something has taken it) and counts as changed.
fn delete_batch(
    root: &Path,
    quarantine: &Path,
    chain: &Chain,
    items: &[Copied],
    done: &AtomicU64,
    cancelled: &dyn Fn() -> bool,
    hooks: Hooks,
) -> Batch {
    let started = Instant::now();
    let mut batch = Batch { processed: 0, stats: TrashStats::default(), replaced: false };
    if !chain_intact(chain) {
        batch.replaced = true;
        return batch;
    }
    // Without the marker that says where this quarantine's files belong nothing could be put back from it: nothing goes in, and every
    // file of the batch stays where it is.
    if !marker_ready(quarantine, root) {
        batch.stats.locked += items.len() as u64;
        batch.processed = items.len();
        return batch;
    }
    for item in items {
        if cancelled() || started.elapsed() > TRASH_RECHECK {
            break;
        }
        match delete_one(root, quarantine, chain, item, cancelled, hooks) {
            Deleted::Gone => {}
            Deleted::Changed => batch.stats.changed += 1,
            Deleted::Locked => batch.stats.locked += 1,
            Deleted::Stranded => batch.stats.stranded += 1,
            Deleted::Replaced => {
                batch.replaced = true;
                return batch;
            }
        }
        batch.processed += 1;
        done.fetch_add(1, Ordering::Relaxed);
    }
    batch
}

/// Takes the file at `path` out of the old folder into a brand-new folder of the quarantine, `<quarantine>/<slot>/<P>/<N>`: the
/// slot folder is made first (it fails if the name is taken, so nothing is ever replaced there) and the file is then renamed to
/// the same relative place inside it in one step, whatever is at `path` at that moment (a file a program saved there meanwhile comes
/// along and is not the verified one, which the checks afterwards find). The file keeps its own name, so a name at the limit of the
/// volume still fits, and no name a person gave a file or folder can be in the way. Returns the slot and where the file now is.
fn take_aside(quarantine: &Path, os: &[std::ffi::OsString], path: &Path, between: Option<hook::Callback>) -> io::Result<(PathBuf, PathBuf)> {
    for _ in 0..8 {
        let slot = quarantine.join(random_hex(8)?);
        match std::fs::create_dir(&slot) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        let held = join(&slot, os);
        if let Some(parent) = held.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            remove_empty_tree(&slot);
            return Err(e);
        }
        if let Some(hook) = between {
            hook(path);
        }
        if let Err(e) = std::fs::rename(path, &held) {
            remove_empty_tree(&slot);
            return Err(e);
        }
        return Ok((slot, held));
    }
    Err(io::Error::other("no free place in the quarantine"))
}

fn delete_one(root: &Path, quarantine: &Path, chain: &Chain, item: &Copied, cancelled: &dyn Fn() -> bool, hooks: Hooks) -> Deleted {
    if !chain_intact(chain) {
        return Deleted::Replaced;
    }
    let path = join(root, &item.entry.os);
    if !ancestors_are_real(root, &item.entry.os) {
        return Deleted::Changed;
    }
    let meta = std::fs::symlink_metadata(&path);
    let unchanged = meta.as_ref().is_ok_and(|m| m.is_file() && !is_link(m) && m.len() == item.entry.size && m.modified().ok() == item.entry.modified);
    if !unchanged {
        // Gone (the person deleted it) is not "kept"; anything else there is.
        return if meta.is_err() { Deleted::Gone } else { Deleted::Changed };
    }
    let expected = meta.ok().and_then(|m| identity(&path, &m));
    let Ok((slot, held)) = take_aside(quarantine, &item.entry.os, &path, hooks.between) else { return Deleted::Locked };
    // What was moved must be the file that was looked at: a regular file with its identity. A folder, a link or another file that
    // took its name in the last moment goes straight back (never over anything) and counts as changed.
    let it_is_the_file =
        std::fs::symlink_metadata(&held).is_ok_and(|m| m.is_file() && !is_link(&m) && (expected.is_none() || identity(&held, &m) == expected));
    if !it_is_the_file {
        if put_back(&held, &path).is_err() {
            return Deleted::Stranded;
        }
        remove_empty_tree(&slot);
        return Deleted::Changed;
    }
    if let Some(hook) = hooks.after {
        hook(&held);
    }
    let same = matches!(chunk_file(&held, cancelled, |_| Ok(())), Ok((m, _)) if m == item.manifest);
    if same && std::fs::remove_file(&held).is_ok() {
        remove_empty_tree(&slot);
        return Deleted::Gone;
    }
    if put_back(&held, &path).is_err() {
        return Deleted::Stranded;
    }
    remove_empty_tree(&slot);
    if same { Deleted::Locked } else { Deleted::Changed }
}

/// Puts back everything a crash (or a put-back that failed) left in the quarantine of this personal folder, whichever job it was:
/// every file goes back to its place (the folders above it are made again) or next to it, never over anything. Returns the
/// quarantine folders that still hold files, or the quarantine's own folder when it can't be read at all (a drive that is not there
/// yet, access denied): what is in it is not known, so it is not taken for empty.
fn recover_quarantine_all(account: &Account) -> Vec<PathBuf> {
    let mut stranded = Vec::new();
    let base = quarantine_base(account);
    let entries = match std::fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // None there, unless the personal folder itself can't be looked at.
            if matches!(std::fs::symlink_metadata(&account.folder), Err(e) if e.kind() != io::ErrorKind::NotFound) {
                stranded.push(base);
            }
            return stranded;
        }
        Err(_) => {
            stranded.push(base);
            return stranded;
        }
    };
    let personal = std::fs::canonicalize(&account.folder).unwrap_or_else(|_| account.folder.clone());
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if !recover_one(&path, &personal) {
                stranded.push(path);
            }
        } else if path.extension().is_some_and(|e| e == "root") && !path.with_extension("").exists() {
            // A marker whose quarantine folder is gone.
            let _ = std::fs::remove_file(&path);
        }
    }
    let _ = std::fs::remove_dir(&base);
    stranded
}

/// Whether `dir` holds a file anywhere below it (or can't be read, which is not taken for empty).
fn holds_files(dir: &Path) -> bool {
    let mut stack = vec![dir.to_owned()];
    while let Some(here) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&here) else { return true };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                _ => return true,
            }
        }
    }
    false
}

/// Whether the quarantine folder `dir` is empty (and removed, with its marker) after its files were put back. Its files lie at
/// `<slot>/<path in the old folder>`; the slot is dropped and the rest goes back below the old folder. A marker that is missing,
/// empty or not sound leaves the folder untouched and reported, and so does a file that is not at such a place.
fn recover_one(dir: &Path, personal: &Path) -> bool {
    if !holds_files(dir) {
        remove_empty_tree(dir);
        let _ = std::fs::remove_file(marker_of(dir));
        return true;
    }
    let Some(root) = marker_root(dir, personal) else { return false };
    let mut clean = true;
    let mut stack = vec![dir.to_owned()];
    while let Some(here) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&here) else {
            clean = false;
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(dir) {
                let mut parts = rel.components();
                parts.next();
                let inside: PathBuf = parts.collect();
                if inside.as_os_str().is_empty() || put_back(&path, &root.join(inside)).is_err() {
                    clean = false;
                }
            } else {
                clean = false;
            }
        }
    }
    if clean {
        remove_empty_tree(dir);
        if !dir.exists() {
            let _ = std::fs::remove_file(marker_of(dir));
        }
    }
    clean
}

/// How the message about files that could not be put back starts.
const STRANDED_LEAD: &str = "Some files could not be put back into the old folder and are kept in ";

/// What to tell the person when files could not be put back out of a quarantine folder.
fn stranded_words(dirs: &[PathBuf]) -> String {
    let dir = dirs.first().map(|d| d.display().to_string()).unwrap_or_default();
    format!("{STRANDED_LEAD}{dir}. Move them back yourself; the agent tries again at its next start.")
}

/// Removes `dir` and the folders in it that are empty.
fn remove_empty_tree(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_tree(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// After the files: what the OS leaves in folders goes with them, empty folders go (deepest first), and so does the quarantine and,
/// when nothing stayed, the old folder itself. Returns how many other files the old folder still holds (not counting the
/// `kept` that [`delete_batch`] counted).
fn finish_old(root: &Path, dirs: &[(Vec<std::ffi::OsString>, Vec<String>)], quarantine: &Path, kept: u64) -> u64 {
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
    let mut sorted: Vec<&Vec<std::ffi::OsString>> = dirs.iter().map(|(os, _)| os).collect();
    sorted.sort_by_key(|os| std::cmp::Reverse(os.len()));
    for os in sorted {
        // Only an empty real folder goes; a link is not ours to remove.
        let place = join(root, os);
        if std::fs::symlink_metadata(&place).is_ok_and(|m| m.is_dir() && !is_link(&m)) {
            let _ = std::fs::remove_dir(&place);
        }
    }
    remove_empty_tree(quarantine);
    if !quarantine.exists() {
        let _ = std::fs::remove_file(marker_of(quarantine));
    }
    if let Some(parent) = quarantine.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    let remaining = count_files(root);
    if remaining == 0 {
        let _ = std::fs::remove_dir(root);
    }
    remaining.saturating_sub(kept)
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
        let project = dir.path().join("Craft Cloud").join("Project");
        std::fs::create_dir_all(&project).unwrap();
        let at_project = |step| FromFolderStatus { folder: project.display().to_string(), ..status(step) };
        for step in [FromFolderStep::Creating, FromFolderStep::Copying, FromFolderStep::Trashing] {
            write_journal(&data, personal, &at_project(step));
            restore(&control, &data, &account);
            let shown = control.from_folder().unwrap();
            assert_eq!((shown.step, shown.failed_at), (FromFolderStep::Interrupted, Some(step)), "{step:?}");
            assert!(shown.message.is_some());
            // The journal says it too, so the next start reports the same.
            assert_eq!(read_journal(&data, personal).map(|s| s.step), Some(FromFolderStep::Interrupted));
        }
        // Stopped in the middle of deleting, with the old folder gone by now: there is nothing left to finish.
        write_journal(&data, personal, &at_project(FromFolderStep::Trashing));
        std::fs::remove_dir_all(&project).unwrap();
        restore(&control, &data, &account);
        assert_eq!(control.from_folder().map(|s| (s.step, s.failed_at)), Some((FromFolderStep::Done, None)));
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

    #[test]
    fn nothing_that_is_moved_aside_or_put_back_ever_replaces_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let (held, place) = (dir.path().join("held.txt"), dir.path().join("a").join("place.txt"));
        // The place was made again meanwhile: the old content goes next to it, the new file is untouched.
        std::fs::write(&held, b"old").unwrap();
        std::fs::create_dir_all(place.parent().unwrap()).unwrap();
        std::fs::write(&place, b"new").unwrap();
        put_back(&held, &place).unwrap();
        assert_eq!(std::fs::read(&place).unwrap(), b"new");
        assert_eq!(std::fs::read(place.with_file_name("place.txt.cc-kept")).unwrap(), b"old");
        assert!(!held.exists());
        // And again: the first `.cc-kept` stays, the next one is numbered.
        std::fs::write(&held, b"older").unwrap();
        put_back(&held, &place).unwrap();
        assert_eq!(std::fs::read(place.with_file_name("place.txt.cc-kept")).unwrap(), b"old");
        assert_eq!(std::fs::read(place.with_file_name("place.txt.cc-kept-2")).unwrap(), b"older");
        // A place whose folders are gone gets them made.
        let deep = dir.path().join("x").join("y").join("z.txt");
        std::fs::write(&held, b"deep").unwrap();
        put_back(&held, &deep).unwrap();
        assert_eq!(std::fs::read(&deep).unwrap(), b"deep");
        // The plain move refuses a name that is taken, and leaves both files.
        std::fs::write(&held, b"moving").unwrap();
        let err = move_noreplace(&held, &place).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!((std::fs::read(&held).unwrap(), std::fs::read(&place).unwrap()), (b"moving".to_vec(), b"new".to_vec()));
    }

    #[test]
    fn a_volume_without_hard_links_still_never_has_a_file_replaced() {
        let dir = tempfile::tempdir().unwrap();
        hook::set_hard_link_error(dir.path(), Some(io::ErrorKind::Unsupported));
        let (held, place) = (dir.path().join("held.txt"), dir.path().join("a").join("place.txt"));
        // The place is free: the file goes back by the rename.
        std::fs::write(&held, b"old").unwrap();
        put_back(&held, &place).unwrap();
        assert_eq!((std::fs::read(&place).unwrap(), held.exists()), (b"old".to_vec(), false));
        // The place is taken: next to it, the new file untouched.
        std::fs::write(&held, b"older").unwrap();
        put_back(&held, &place).unwrap();
        assert_eq!(std::fs::read(&place).unwrap(), b"old");
        assert_eq!(std::fs::read(place.with_file_name("place.txt.cc-kept")).unwrap(), b"older");
        // The finished copy lands on a free name, and is refused on a taken one.
        let (temp, dest) = (dir.path().join("copy.tmp"), dir.path().join("dest.txt"));
        std::fs::write(&temp, b"copy").unwrap();
        land_noreplace(&temp, &dest).unwrap();
        assert_eq!((std::fs::read(&dest).unwrap(), temp.exists()), (b"copy".to_vec(), false));
        std::fs::write(&temp, b"other").unwrap();
        let err = land_noreplace(&temp, &dest).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!((std::fs::read(&dest).unwrap(), std::fs::read(&temp).unwrap()), (b"copy".to_vec(), b"other".to_vec()));
        hook::set_hard_link_error(dir.path(), None);
    }

    #[test]
    fn any_other_error_of_a_hard_link_is_a_failure_that_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        hook::set_hard_link_error(dir.path(), Some(io::ErrorKind::PermissionDenied));
        let (held, place) = (dir.path().join("held.txt"), dir.path().join("place.txt"));
        std::fs::write(&held, b"kept").unwrap();
        assert_eq!(put_back(&held, &place).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!((std::fs::read(&held).unwrap(), place.exists()), (b"kept".to_vec(), false), "no rename fallback");
        let dest = dir.path().join("dest.txt");
        assert_eq!(land_noreplace(&held, &dest).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!((held.exists(), dest.exists()), (true, false));
        hook::set_hard_link_error(dir.path(), None);
        // What counts as "no hard links here".
        assert!(hard_links_unsupported(&io::Error::from(io::ErrorKind::Unsupported)));
        assert!(!hard_links_unsupported(&io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(!hard_links_unsupported(&io::Error::from(io::ErrorKind::AlreadyExists)));
    }

    /// An old folder `Old` holding `a.txt` with this content, a quarantine beside it, the file's item and the folder's chain.
    fn old_folder_with_a_file(dir: &Path, content: &[u8]) -> (PathBuf, PathBuf, Chain, Copied) {
        let root = dir.join("Old");
        std::fs::create_dir_all(&root).unwrap();
        // The quarantine folder exists (the batch made it, with its marker, before anything is taken aside).
        std::fs::create_dir_all(dir.join("q")).unwrap();
        std::fs::write(root.join("a.txt"), content).unwrap();
        let (manifest, size) = chunk_file(&root.join("a.txt"), &|| false, |_| Ok(())).map_err(|_| ()).unwrap();
        let meta = std::fs::metadata(root.join("a.txt")).unwrap();
        let item = Copied {
            entry: FileEntry { os: vec!["a.txt".into()], names: vec!["a.txt".into()], size, modified: meta.modified().ok() },
            manifest,
            team_edit: None,
        };
        let chain = chain_of(&std::fs::canonicalize(&root).unwrap(), &std::fs::canonicalize(dir).unwrap());
        (std::fs::canonicalize(&root).unwrap(), dir.join("q"), chain, item)
    }

    #[test]
    fn a_leftover_in_the_quarantine_is_never_in_the_way_of_a_file_taken_aside() {
        let dir = tempfile::tempdir().unwrap();
        let (root, quarantine, chain, item) = old_folder_with_a_file(dir.path(), b"the new file");
        // A leftover (what a failed put-back keeps) lies in a slot of its own, with the same relative name.
        std::fs::create_dir_all(quarantine.join("0123456789abcdef")).unwrap();
        std::fs::write(quarantine.join("0123456789abcdef").join("a.txt"), b"the leftover").unwrap();
        let out = delete_one(&root, &quarantine, &chain, &item, &|| false, Hooks::default());
        assert!(matches!(out, Deleted::Gone));
        assert!(!root.join("a.txt").exists());
        assert_eq!(std::fs::read(quarantine.join("0123456789abcdef").join("a.txt")).unwrap(), b"the leftover", "the leftover is untouched");
        assert_eq!(std::fs::read_dir(&quarantine).unwrap().count(), 1, "the file's own slot is gone");
    }

    /// What an editor does: writes a temporary file and renames it over the original.
    fn save_b_over(path: &Path) {
        let temp = path.with_extension("editor-temp");
        std::fs::write(&temp, b"B, saved by an editor in between").unwrap();
        std::fs::rename(&temp, path).unwrap();
    }

    #[test]
    fn a_file_saved_at_the_old_place_while_the_original_is_taken_aside_is_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let (root, quarantine, chain, item) = old_folder_with_a_file(dir.path(), b"A, as it was verified");
        // Between the steps of taking the file aside, B is saved at its place.
        let out = delete_one(&root, &quarantine, &chain, &item, &|| false, Hooks { between: Some(save_b_over), after: None });
        assert!(matches!(out, Deleted::Changed), "B is not what was verified: {out:?}");
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"B, saved by an editor in between", "B is back where it was saved");
        assert!(!quarantine.exists() || std::fs::read_dir(&quarantine).unwrap().count() == 0, "nothing is left in the quarantine");
    }

    /// The old file is replaced by another file with the very same content, as a program that saves a copy and renames it does.
    fn replace_by_a_copy(path: &Path) {
        let temp = path.with_extension("copy-temp");
        std::fs::copy(path, &temp).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::rename(&temp, path).unwrap();
    }

    #[test]
    fn what_was_moved_aside_is_checked_to_be_the_file_that_was_looked_at() {
        let dir = tempfile::tempdir().unwrap();
        let (root, quarantine, chain, item) = old_folder_with_a_file(dir.path(), b"same content");
        // Another file with the same content took the name at the last moment: it is not the verified file, and goes back.
        let hooks = Hooks { between: Some(replace_by_a_copy), after: None };
        let out = delete_one(&root, &quarantine, &chain, &item, &|| false, hooks);
        assert!(matches!(out, Deleted::Changed), "kept, not deleted: {out:?}");
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"same content");
        assert!(!quarantine.exists() || std::fs::read_dir(&quarantine).unwrap().count() == 0);
    }

    /// A folder takes the file's name.
    fn replace_by_a_folder(path: &Path) {
        std::fs::remove_file(path).unwrap();
        std::fs::create_dir(path).unwrap();
        std::fs::write(path.join("inside.txt"), b"in the folder").unwrap();
    }

    #[test]
    fn a_folder_that_took_the_files_name_is_put_back_where_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let (root, quarantine, chain, item) = old_folder_with_a_file(dir.path(), b"a file");
        let hooks = Hooks { between: Some(replace_by_a_folder), after: None };
        let out = delete_one(&root, &quarantine, &chain, &item, &|| false, hooks);
        assert!(matches!(out, Deleted::Changed), "kept, and not stranded in the quarantine: {out:?}");
        assert_eq!(std::fs::read(root.join("a.txt").join("inside.txt")).unwrap(), b"in the folder");
        assert!(!quarantine.exists() || std::fs::read_dir(&quarantine).unwrap().count() == 0);
    }

    #[test]
    fn an_old_file_moved_aside_by_a_crash_is_put_right_at_the_next_copy() {
        let dir = tempfile::tempdir().unwrap();
        let team = std::fs::canonicalize(dir.path()).unwrap();
        let tmp = team.join(AGENT_DIR).join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let aside = |id: &str, key: &str, content: &[u8]| {
            std::fs::write(tmp.join(format!("{id}.{ASIDE_EXT}")), content).unwrap();
            std::fs::write(tmp.join(format!("{id}.{ASIDE_EXT}.path")), key).unwrap();
        };
        let manifest_of = |content: &[u8]| {
            let path = tmp.join("probe");
            std::fs::write(&path, content).unwrap();
            let (m, _) = chunk_file(&path, &|| false, |_| Ok(())).map_err(|_| ()).unwrap();
            std::fs::remove_file(&path).unwrap();
            m.to_string()
        };
        let written = Mutex::new(Written::from([
            ("own.txt".to_owned(), manifest_of(b"the job's old copy")),
            ("Dir/edit.txt".to_owned(), manifest_of(b"what the job wrote")),
        ]));
        // The crash came after the file was moved aside and before the new one landed: its place is free, the file goes back.
        aside("a1", "Dir/lost.txt", b"must come back");
        std::fs::create_dir_all(team.join("Dir")).unwrap();
        // The new copy landed, the crash came before the cleanup: the job's own old content is dropped.
        aside("a2", "own.txt", b"the job's old copy");
        std::fs::write(team.join("own.txt"), b"the new copy").unwrap();
        // Something else is there and the moved file is an edit: both stay, the edit under a kept name.
        aside("a3", "Dir/edit.txt", b"somebody's edit");
        std::fs::write(team.join("Dir").join("edit.txt"), b"what is there now").unwrap();
        recover_asides(&tmp, &team, &written, &|| false);
        assert_eq!(std::fs::read(team.join("Dir").join("lost.txt")).unwrap(), b"must come back");
        assert_eq!(std::fs::read(team.join("own.txt")).unwrap(), b"the new copy");
        assert_eq!(std::fs::read(team.join("Dir").join("edit.txt")).unwrap(), b"what is there now");
        assert_eq!(std::fs::read(team.join("Dir").join("edit.txt.cc-kept")).unwrap(), b"somebody's edit");
        let left: Vec<_> = std::fs::read_dir(&tmp).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn what_stayed_is_told_by_its_reason() {
        let kept = TrashStats { changed: 1, locked: 2, ..TrashStats::default() };
        let words = kept.words(3).unwrap();
        assert!(words.contains("1 file changed after the copy and is not in the team space as it is now"), "{words}");
        assert!(words.contains("2 files could not be deleted") && words.contains("they are in the team space"), "{words}");
        assert!(words.contains("3 other files stayed"), "{words}");
        assert_eq!(kept.kept(), 3, "the files the sync never carries are not counted");
        assert_eq!(TrashStats::default().words(0), None);
    }

    #[test]
    fn the_start_time_is_the_servers_or_none_and_a_retry_keeps_the_one_it_has() {
        // A new creation takes the server's clock, or none when the server did not say (never the agent's own).
        assert_eq!(started_time(false, None, Some(5)), Some(5));
        assert_eq!(started_time(false, Some(1), Some(5)), Some(5));
        assert_eq!(started_time(false, None, None), None);
        // A crashed creation tried again keeps what it has; a retry never moves it forward.
        assert_eq!(started_time(true, Some(1), Some(5)), Some(1));
        assert_eq!(started_time(true, None, Some(5)), Some(5));
        assert_eq!(started_time(true, None, None), None);
    }
}
