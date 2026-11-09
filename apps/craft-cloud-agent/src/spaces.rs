//! Team spaces on this computer (`craft-cloud-agent run`): finding out which spaces the account is in, one engine per
//! synced space, and what the local API says about them.
//!
//! - **Discovery**: `GET /spaces` is asked at the start, then every two long-poll waits, and at once when a change may
//!   have happened (an invitation accepted or declined here, a space that answered `404`). A server that speaks
//!   protocol 1, or has no `/spaces`, has no team spaces: the personal space syncs as before, the agent asks about the
//!   server's version only (ten times less often), and a team space it remembers shows as unavailable. Nothing sends the
//!   `craft-space` header to such a server. A failed discovery changes nothing; when someone waits for the list (an invitation
//!   accepted or declined here, the sync turned on) it is tried again after a few seconds, for up to [`REFRESH_BOUND`], before
//!   they are released with the old list.
//! - **Folders**: a team space syncs to `Craft Cloud Shared/<name>/` beside the personal folder. The name goes through the
//!   node name rules (anything a file name can't hold becomes `_`), a name that is taken (by another space or by a folder
//!   already there) gets ` (2)`, ` (3)`. The folder is chosen when the space is first seen and kept when the space is
//!   renamed on the server: renaming a folder under running apps would break their open paths.
//! - **Off by default**: an accepted team space does not sync until the person turns it on ([`SpacesHandle::sync`]):
//!   an administrator may add someone to a space of terabytes. Turning it on sets the folder up (a new or empty folder,
//!   or one adopted with consent, with the space's root marker), and the sync begins.
//! - **One engine per space**: each synced space has its own session ([`crate::cli::Session::open_team`]): its own state in
//!   `spaces/<id>/state.json`, root, marker, guards, scheduler, folder watcher and long-poll, all through a transport
//!   bound to the space that refuses an answer without the space's echo. A problem in one space never pauses another.
//! - **A state belongs to its folder**: `state.json` records the space and the canonical folder it was made for, and is used
//!   for that pair only. For any other folder the space starts afresh, as a first sync: what is there and what the server
//!   has are merged, and nothing is deleted on either side. Changing a space's folder, or turning it off, first closes its
//!   session and waits until the pass thread has let go of it, so no pass for the old folder can write a state after the
//!   new folder is set up. The state also records the server's stamp of the account's membership (`joined_ms` in
//!   `GET /spaces`, new when someone is removed and invited again): a state made under another stamp is not used either,
//!   which covers an agent that was not running while access was lost and regained. A server without the stamp is not
//!   compared. Turning a space off forgets its state: turned on again, in the same folder or another, it is a first sync.
//!   Waiting for a session to close never holds up the task: other spaces, the local API and stopping go on, and the caller
//!   is answered when the session is closed (or after [`CLOSE_WAIT`]).
//! - **A first sync merges** (a fresh state, whatever the reason): nothing is deleted on either side, because nothing is known
//!   to have been deleted. A file that was deleted on the server meanwhile and is still in the folder is uploaded again. A
//!   file that exists on both sides with different content is not overwritten: the local one is moved aside as a conflict
//!   copy and the server's version takes its path.
//! - **A state is trusted only when nothing doubtful happened to it**: the setting's `generation` (saved in `account.json`) is
//!   raised before a state is retired or a new folder's marker is written, and a state made under another generation is not
//!   used; a state that can't be removed fails the turn-off; and a session whose folder now leads somewhere else (a link
//!   pointed at another folder) is dropped with its state before the next pass.
//!   A raised generation that can't be saved (account.json can't be written) is kept in memory and saved again at every look at
//!   the list; that space is not started meanwhile (its state shows why), so a session that begins cannot use a state that was
//!   meant to be dropped. Other spaces are not held back.
//! - **One pass at a time**: passes of the spaces run one after another on a thread of their own, and take turns with the
//!   personal space's passes ([`PassGate`]). A pass that is queued when its space is closed (turned off, moved to another
//!   folder, lost) or the agent stops is not run; the running pass finishes. A second change of a space that is still
//!   being closed is refused ([`CHANGING`], `409` in the local API).
//! - **Losing access**: a space that is no longer listed (or answers `404`) stops syncing and is marked
//!   [`SpaceRun::NoAccess`]. Nothing in its folder is touched. Its sync state is dropped once its session is closed, so if
//!   access comes back the folder starts afresh and is merged with the space like a first sync: no local file is deleted
//!   or replaced (a file that differs on both sides keeps both, one as a conflict copy), what was deleted here meanwhile is
//!   not deleted on the server, and what was deleted on the server meanwhile is not deleted here. An archived space is not
//!   lost: its members keep read access, so it stays listed and its engine is read-only. (The server lists it with the stored
//!   role, an editor stays `editor`, although access is capped at viewer: the agent goes by `archived`.)
//! - **One live link per synced space**: each synced team space also has a live hub of its own ([`Hub`]: its own WebSocket
//!   with the space in the `craft-space` header, its documents and journals in `live/<space>/`), started with the space's sync
//!   and stopped with it (turned off, moved, lost). The local API routes a document by the path an app opens to the hub whose
//!   folder holds it ([`Control::route_path`]). The hub is told the role from the list (a viewer, or an archived space, may
//!   not change documents) and asks for a pass of its space when a file is opened that isn't uploaded yet.
//! - **Mass-delete approval**: a team space's held deletions are approved through `spaces/<id>/approval.json`, which
//!   `sync --approve-deletes --space` and `POST /v1/spaces/{id}/approve-deletes` write.
//! - **Read-only**: a role of viewer (or an archived space) puts the space's engine in read-only mode before its first
//!   write; the status says so ([`SpaceStatus::read_only`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use craft_cloud_proto::api::{SpaceListItem, SpaceRole};
use craft_cloud_proto::local::{SpaceInfo, SpaceRun, SpaceStatus, SpaceSync, SpaceSyncRequest};
use craft_cloud_proto::name::FORBIDDEN;
use craft_cloud_proto::{Name, NodeId, SpaceId};
use craft_cloud_sync::TransportError;
use tokio::sync::{mpsc, oneshot, watch};

use crate::account::{Account, DataDir, LastPass, Outcome, PassSummary, SpaceSetting};
use crate::cli::{AgentError, Exit, Session};
use crate::daemon::{RunOptions, counts_as_change, pass_end, take_due_peeks};
use crate::http::{CallError, DeviceToken, HttpTransport, PollError, ServerUrl};
use crate::live::{Hub, HubConfig, TeamHub};
use crate::local_api::Control;
use crate::root::{self, RootMarker};
use crate::schedule::{Ms, Next, Phase, Scheduler};
use crate::settle::Stability;
use crate::watch::{FolderWatcher, WatchEvent};

/// The folder beside the personal one that holds the team spaces' folders.
pub const SHARED_FOLDER: &str = "Craft Cloud Shared";
/// The first HTTP protocol with team spaces (`craft-space`, `/spaces`).
pub const TEAM_PROTOCOL: u32 = 2;
/// What a team space that is not synced here is told when the server is too old.
pub const NEEDS_UPDATE: &str = "the server needs an update for team spaces";
/// The longest the next hub of a space waits for the hub it replaces to end (and let go of the space's live folder).
const HUB_END_WAIT: Duration = Duration::from_secs(10);
/// The longest a hub is given to take a pause or a resume.
const HUB_ANSWER: Duration = Duration::from_secs(5);
/// The longest a local API call waits for the spaces task to answer.
const ANSWER_WAIT: Duration = Duration::from_secs(60);
/// The longest the task waits for a space's session to be closed before it changes the folder (shorter than
/// [`ANSWER_WAIT`], so the caller is answered).
const CLOSE_WAIT: Duration = Duration::from_secs(50);
/// What a change of a space's sync is told when another change of the same space waits for its session to close (the local
/// API answers `409`).
pub const CHANGING: &str = "this space is being changed, try again in a moment";
/// What turning a space on is told when the space's last pass did not end within [`CLOSE_WAIT`].
const STILL_FINISHING: &str = "this space is still finishing its last pass, try again in a moment";
/// What a space that is held back until its raised generation is saved shows as its state.
const UNSAVED: &str = "this space's settings could not be saved yet; it starts again as soon as they can";
/// How soon a discovery that failed is tried again while someone waits for it.
const DISCOVERY_RETRY: Duration = Duration::from_secs(3);
/// The longest those waiting for a discovery are kept waiting for one that works (they are released after that, with the old
/// list).
const REFRESH_BOUND: Duration = Duration::from_secs(15);
/// The longest folder name made from a space's name, in characters.
const MAX_FOLDER_CHARS: usize = 100;

/// Takes turns between the personal space's passes and the team spaces' ones: whoever holds it runs a pass.
#[derive(Clone, Default)]
pub struct PassGate(Arc<Mutex<()>>);

impl PassGate {
    /// Waits for the turn; the pass runs while the guard lives.
    pub fn hold(&self) -> MutexGuard<'_, ()> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Folders

/// The folder name for a team space: its name with what a file name can't hold (and control characters) made `_`, no dots
/// or spaces at the ends, cut to a sane length, and never a reserved name.
pub fn folder_name(space_name: &str) -> String {
    let cleaned: String = space_name.chars().map(|c| if c.is_control() || FORBIDDEN.contains(&c) { '_' } else { c }).collect();
    let mut name: String = cleaned.trim().trim_end_matches(['.', ' ']).chars().take(MAX_FOLDER_CHARS).collect();
    // A leading dot would hide the folder (and `.craft-cloud` is the agent's own).
    if name.starts_with('.') {
        name.replace_range(..1, "_");
    }
    if name.is_empty() {
        return "Team space".to_owned();
    }
    for candidate in [name.clone(), format!("{name} (team space)")] {
        if let Ok(valid) = Name::new(&candidate) {
            return valid.as_str().to_owned();
        }
    }
    "Team space".to_owned()
}

/// How two folder names are told apart: as the node rules do, so names that differ only by case or by normalisation are one.
fn key(name: &str) -> String {
    Name::new(name).map_or_else(|_| name.to_lowercase(), |n| n.fold_key().as_str().to_owned())
}

/// A folder in `shared` for a space called `space_name` that nothing uses: not in `taken` (the folders of the spaces this
/// computer knows) and not already on disk. A name that is taken gets ` (2)`, ` (3)` and so on.
pub fn free_folder(shared: &Path, space_name: &str, taken: &[PathBuf]) -> PathBuf {
    let base = folder_name(space_name);
    let on_disk: BTreeSet<String> = std::fs::read_dir(shared)
        .map(|entries| entries.filter_map(Result::ok).map(|e| key(&e.file_name().to_string_lossy())).collect())
        .unwrap_or_default();
    let used = |candidate: &str| {
        let wanted = key(candidate);
        on_disk.contains(&wanted)
            || taken.iter().any(|t| t.parent() == Some(shared) && t.file_name().is_some_and(|n| key(&n.to_string_lossy()) == wanted))
    };
    for n in 1..=1_000u32 {
        let candidate = if n == 1 { base.clone() } else { format!("{base} ({n})") };
        if !used(&candidate) {
            return shared.join(candidate);
        }
    }
    shared.join(format!("{base} ({})", SpaceId::generate()))
}

/// Where the team spaces' folders go: `Craft Cloud Shared` beside the personal folder.
pub fn shared_root(personal: &Path) -> Option<PathBuf> {
    personal.parent().filter(|p| p.parent().is_some() || p.file_name().is_some()).map(|p| p.join(SHARED_FOLDER))
}

/// What a discovery changed in the settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Reconciled {
    /// The settings after: one per team space the server lists as active (new ones with syncing off and a folder), and the
    /// ones for spaces it no longer lists, unchanged.
    pub settings: Vec<SpaceSetting>,
    /// Spaces that were renamed on the server (their folders keep their names).
    pub renamed: Vec<(SpaceId, String, String)>,
}

/// Settings after the server listed the account's spaces. `shared` is where new folders go (`None`: no place for them; a
/// space without a folder is given one when it is turned on).
pub fn reconcile(settings: &[SpaceSetting], listed: &[SpaceListItem], shared: Option<&Path>) -> Reconciled {
    let mut settings = settings.to_vec();
    let mut renamed = Vec::new();
    for item in listed.iter().filter(|i| i.kind == "team" && i.state == "active") {
        let name = item.name.clone().unwrap_or_default();
        if let Some(setting) = settings.iter_mut().find(|s| s.space == item.id) {
            if let Some(old) = setting.name.replace(name.clone()).filter(|old| *old != name) {
                renamed.push((item.id, old, name));
            }
            if item.root.is_some() {
                setting.root = item.root;
            }
        } else if let Some(shared) = shared {
            let taken: Vec<PathBuf> = settings.iter().map(|s| s.folder.clone()).collect();
            settings.push(SpaceSetting {
                space: item.id,
                folder: free_folder(shared, &name, &taken),
                sync: false,
                name: Some(name),
                root: item.root,
                generation: 0,
            });
        }
    }
    Reconciled { settings, renamed }
}

// ---------------------------------------------------------------------------------------------------------------------
// The handle the local API uses

enum Msg {
    /// Discover now; answered once it is done.
    Refresh(Option<oneshot::Sender<()>>),
    /// A file of the space was opened that isn't uploaded yet, or a save record came: a pass soon.
    SyncSoon(SpaceId),
    Sync(SpaceId, SpaceSyncRequest, oneshot::Sender<Result<SpaceInfo, AgentError>>),
    /// The team space's passes may claim chunks from this space (or no longer, with `None`).
    Claim(SpaceId, Option<SpaceId>),
    Paused(bool),
    /// No new pass starts (the agent is stopping).
    Quiesce,
    Stop,
    /// A second stop: no queued pass starts and the pass thread is not waited for.
    StopNow,
}

/// What the local API calls on the spaces task. Cheap to clone.
#[derive(Clone)]
pub struct SpacesHandle {
    tx: mpsc::UnboundedSender<Msg>,
}

impl SpacesHandle {
    /// Looks at the server's list of spaces now, and returns once that is done (or the wait ran out).
    pub async fn refresh(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Refresh(Some(tx))).is_ok() {
            let _ = tokio::time::timeout(ANSWER_WAIT, rx).await;
        }
    }

    /// Looks at the server's list of spaces soon, without waiting for it (a live link was told the account's access changed).
    pub fn look_soon(&self) {
        let _ = self.tx.send(Msg::Refresh(None));
    }

    /// Asks for a pass of a team space soon (an app opened a file of it that isn't uploaded yet). Not while syncing is paused.
    pub fn sync_soon(&self, space: SpaceId) {
        let _ = self.tx.send(Msg::SyncSoon(space));
    }

    /// Turns the sync of a team space on this computer on or off ([`SpaceSyncRequest`]).
    pub async fn sync(&self, space: SpaceId, request: SpaceSyncRequest) -> Result<SpaceInfo, AgentError> {
        let gone = || AgentError::Usage("the agent is stopping".into());
        let (tx, rx) = oneshot::channel();
        self.tx.send(Msg::Sync(space, request, tx)).map_err(|_| gone())?;
        match tokio::time::timeout(ANSWER_WAIT, rx).await {
            Ok(Ok(answer)) => answer,
            _ => Err(gone()),
        }
    }

    /// Lets the passes of team space `space` claim chunks from `from` (the personal space, when a folder is being made into a team
    /// space) instead of uploading them, or stops that with `None`. Kept in memory only: after a restart the passes upload, which is
    /// slower and as safe.
    pub fn claim_from(&self, space: SpaceId, from: Option<SpaceId>) {
        let _ = self.tx.send(Msg::Claim(space, from));
    }

    /// Syncing is paused by the user (or resumed): no pass of a team space starts meanwhile.
    pub fn set_paused(&self, paused: bool) {
        let _ = self.tx.send(Msg::Paused(paused));
    }

    /// The agent is stopping: no new pass starts.
    pub fn quiesce(&self) {
        let _ = self.tx.send(Msg::Quiesce);
    }

    /// A second stop while the task waits for the pass thread to finish: it stops waiting (the thread ends with the pass it
    /// runs) and the task ends at once.
    pub fn stop_now(&self) {
        let _ = self.tx.send(Msg::StopNow);
    }
}

/// What starts the spaces task.
pub struct SpacesConfig {
    pub data: DataDir,
    pub account: Account,
    pub options: RunOptions,
    pub control: Control,
    pub gate: PassGate,
    pub paused: bool,
}

/// The running spaces task and the thread that runs the team spaces' passes.
pub struct Spaces {
    handle: SpacesHandle,
    task: tokio::task::JoinHandle<()>,
}

impl Spaces {
    /// Starts looking after the account's team spaces. The first discovery runs at once.
    pub fn start(config: SpacesConfig) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = SpacesHandle { tx };
        let task = tokio::spawn(Manager::new(config).run(rx));
        Self { handle, task }
    }

    pub fn handle(&self) -> SpacesHandle {
        self.handle.clone()
    }

    /// Stops the task: a running pass finishes first, then every folder is let go of. With `at_once` (the second stop) no
    /// queued pass starts and the thread of the passes is not waited for: it ends with the pass it runs. A second stop that
    /// comes while the first waits ([`SpacesHandle::stop_now`]) ends the wait.
    pub async fn stop(self, at_once: bool) {
        let _ = self.handle.tx.send(if at_once { Msg::StopNow } else { Msg::Stop });
        let _ = self.task.await;
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The passes' thread

/// One pass of a team space, asked of the thread.
struct Job {
    setting: SpaceSetting,
    stability: Stability,
    /// The role is viewer (or the space is archived): the engine is told before its first write.
    read_only: bool,
    approve: Vec<NodeId>,
    /// The server's stamp of the account's membership (see [`SpaceListItem::joined_ms`]) that the saved state must have been
    /// made under.
    joined_ms: Option<i64>,
    /// Which running of the space's sync asked (see [`Active::run`]), so a result that comes after the sync was stopped or
    /// moved to another folder is not taken for the current one's.
    run: u64,
    /// The space the pass may claim chunks from (see [`SpacesHandle::claim_from`]).
    claim_from: Option<SpaceId>,
}

enum ToThread {
    Pass(Box<Job>),
    /// Let go of the space's folder (its session ends), and with `forget` remove its saved state. `ack` is told once that
    /// is done: no pass of the space runs or writes its state after that.
    Close {
        space: SpaceId,
        forget: bool,
        ack: Option<oneshot::Sender<Result<(), String>>>,
    },
}

/// A finished pass, from the thread.
struct Done {
    space: SpaceId,
    run: u64,
    result: Result<PassSummary, AgentError>,
    last_pass: Option<LastPass>,
    seq: Option<u64>,
    recheck_in: Option<Duration>,
}

/// What reaches the spaces task besides its messages.
enum Ev {
    Watch(SpaceId, u64, WatchEvent),
    WatcherGone(SpaceId, u64),
    /// The space's change feed moved, or can't continue (a pass sorts it out).
    Remote(SpaceId),
    /// The long-poll got an answer after failing.
    Back(SpaceId),
    /// The long-poll was told `404`: the space may be gone for this account (a discovery finds out).
    Gone,
    /// A discovery ended.
    Listed(Looked),
    Pass(Box<Done>),
    /// The wait for a space's session to close ended, for the change asked under this number.
    Closed(u64, Closure),
    /// A stopped hub of the space has ended (or the wait for it ran out): the next hub may start.
    HubEnded(SpaceId),
    /// The pass thread found that a space's folder leads somewhere else than its session was opened on, and dropped the
    /// session: the space's generation is raised and saved before its next pass.
    FolderMoved(SpaceId),
}

/// How the wait for a space's session to close ended.
enum Closure {
    /// The session is closed (and the state forgotten if that was asked).
    Closed,
    /// The pass thread did not let go within [`CLOSE_WAIT`].
    TimedOut,
    /// The session is closed but its saved state could not be removed.
    NotForgotten(String),
}

/// The runnings of the sync ([`Active::run`]) that are current: a queued pass of any other is not started.
type Live = Arc<Mutex<BTreeSet<u64>>>;

fn live_set(live: &Live) -> MutexGuard<'_, BTreeSet<u64>> {
    live.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The thread of the team spaces' passes: a session per space, each opened by its first pass, ended when it needs the user
/// or the space is closed. Runs a pass while it holds the turn ([`PassGate`]); a pass whose space was closed since it was
/// queued (turned off, moved, lost, or the agent is stopping) is not run.
fn pass_thread(
    data: DataDir,
    account: Account,
    gate: PassGate,
    live: Live,
    jobs: std::sync::mpsc::Receiver<ToThread>,
    events: mpsc::UnboundedSender<Ev>,
) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
    let mut sessions: Vec<(SpaceId, Session)> = Vec::new();
    while let Ok(job) = jobs.recv() {
        let job = match job {
            ToThread::Close { space, forget, ack } => {
                sessions.retain(|(id, _)| *id != space);
                let forgotten = if forget { data.space_dir(space).remove_state().map_err(|e| e.to_string()) } else { Ok(()) };
                if let Err(e) = &forgotten {
                    crate::runlog::note(format_args!("can't forget the saved state of team space {space}: {e}"));
                }
                if let Some(ack) = ack {
                    let _ = ack.send(forgotten);
                }
                continue;
            }
            ToThread::Pass(job) => *job,
        };
        let space = job.setting.space;
        let _turn = gate.hold();
        // The space was closed (turned off, moved to another folder, lost) after this pass was queued: it is not run. The
        // result is reported all the same, so the task knows the thread is free.
        if !live_set(&live).contains(&job.run) {
            let done = Done { space, run: job.run, result: Ok(PassSummary::default()), last_pass: None, seq: None, recheck_in: None };
            if events.send(Ev::Pass(Box::new(done))).is_err() {
                return;
            }
            continue;
        }
        // A session is for one folder and one membership: the space's folder may have changed since it opened, or the account
        // may have been removed from the space and invited again (its state is then not used, see `load_state_for`).
        if sessions.iter().any(|(id, s)| {
            *id == space && (s.account().folder != job.setting.folder || s.joined_ms().zip(job.joined_ms).is_some_and(|(a, b)| a != b))
        }) {
            sessions.retain(|(id, _)| *id != space);
        }
        // The folder may lead somewhere else than when the session opened (a link pointed at another folder): the session's
        // state is about the old one, so it is dropped and the space starts afresh there, as a first sync. The task is told
        // first, so it raises and saves the space's generation: a state that can't be removed is not trusted again either.
        // This pass is not run; the next one starts under the new generation.
        if sessions.iter().any(|(id, s)| *id == space && s.folder_moved()) {
            sessions.retain(|(id, _)| *id != space);
            crate::runlog::note(format_args!("the folder of team space {space} leads somewhere else now: starting afresh, nothing is deleted"));
            let removed = data.space_dir(space).remove_state();
            let _ = events.send(Ev::FolderMoved(space));
            let why = match removed {
                Ok(()) => "the folder of this space leads somewhere else now; the next pass starts afresh there".to_owned(),
                Err(e) => format!(
                    "the folder of this space leads somewhere else now, and its saved state could not be removed ({e}); the next pass starts afresh there"
                ),
            };
            let done = Done { space, run: job.run, result: Err(AgentError::Usage(why)), last_pass: None, seq: None, recheck_in: None };
            if events.send(Ev::Pass(Box::new(done))).is_err() {
                return;
            }
            continue;
        }
        let index = match sessions.iter().position(|(id, _)| *id == space) {
            Some(index) => Ok(index),
            None => Session::open_team(&data, &account, &job.setting, job.joined_ms, Some(job.stability.clone())).map(|session| {
                sessions.push((space, session));
                sessions.len() - 1
            }),
        };
        let result = match index {
            Ok(index) => match sessions.get_mut(index) {
                Some((_, session)) => {
                    if job.read_only {
                        session.set_read_only(true);
                    }
                    session.set_claim_from(job.claim_from);
                    runtime.block_on(session.pass(job.approve))
                }
                None => Err(AgentError::Usage("the space's session is gone".into())),
            },
            Err(e) => Err(e),
        };
        // A stop that needs the user (the folder went, say) opens the folder afresh next time.
        if result.as_ref().is_err_and(|e| e.exit() == Exit::NeedsYou) {
            sessions.retain(|(id, _)| *id != space);
        }
        let session = sessions.iter().find(|(id, _)| *id == space).map(|(_, s)| s);
        let last_pass = session.and_then(Session::last_pass).or_else(|| {
            data.space_dir(space)
                .load_state_for(space, &job.setting.folder, job.joined_ms, job.setting.generation)
                .ok()
                .and_then(|(s, _)| s.last_pass)
        });
        let seq = session.filter(|s| s.state().space.is_some()).map(|s| s.state().seq);
        let recheck_in = session.and_then(Session::recheck_in);
        if events.send(Ev::Pass(Box::new(Done { space, run: job.run, result, last_pass, seq, recheck_in }))).is_err() {
            return;
        }
    }
}

/// The long-poll of a team space: waits for the space's change feed to move past what the passes read, and tells the task.
/// Backs off on errors as the personal long-poll does, and says when the server answered `404` (the account may have
/// lost the space). It keeps time on tokio's clock.
async fn team_poll(
    transport: HttpTransport,
    space: SpaceId,
    mut since: watch::Receiver<Option<u64>>,
    events: mpsc::UnboundedSender<Ev>,
    options: RunOptions,
) {
    let mut known: Option<u64> = None;
    let mut failures: u32 = 0;
    let mut last_start: Option<tokio::time::Instant> = None;
    loop {
        if let Some(last) = last_start {
            let gap = options.poll_min_spacing.saturating_sub(last.elapsed());
            if !gap.is_zero() {
                tokio::time::sleep(gap).await;
            }
        }
        let passes = *since.borrow_and_update();
        let from = match (passes, known) {
            (Some(passes), Some(seen)) => passes.max(seen),
            (Some(passes), None) => passes,
            (None, _) => {
                if since.changed().await.is_err() {
                    return;
                }
                continue;
            }
        };
        last_start = Some(tokio::time::Instant::now());
        let tell = |ev: Ev| events.send(ev).is_ok();
        let pause = match transport.wait_for_changes(from, options.poll_wait).await {
            Ok(seq) => {
                if failures > 0 && !tell(Ev::Back(space)) {
                    return;
                }
                failures = 0;
                if seq != from {
                    known = Some(seq);
                    if !tell(Ev::Remote(space)) {
                        return;
                    }
                }
                None
            }
            Err(PollError::Busy(after)) => Some(after.unwrap_or(options.poll_busy)),
            Err(PollError::Resync) => {
                known = None;
                if !tell(Ev::Remote(space)) {
                    return;
                }
                failures = failures.saturating_add(1);
                Some(backoff(&options, failures))
            }
            Err(PollError::Failed(error)) => {
                if error == TransportError::NotFound && !tell(Ev::Gone) {
                    return;
                }
                failures = failures.saturating_add(1);
                Some(backoff(&options, failures))
            }
        };
        if let Some(pause) = pause {
            tokio::time::sleep(pause).await;
        }
    }
}

/// The delay after the `failures`-th failed long-poll in a row: doubled each time, up to the cap.
fn backoff(options: &RunOptions, failures: u32) -> Duration {
    let first = options.poll_retry_first.max(Duration::from_millis(1));
    first.saturating_mul(1u32.checked_shl(failures.saturating_sub(1).min(20)).unwrap_or(u32::MAX)).min(options.poll_retry_max)
}

// ---------------------------------------------------------------------------------------------------------------------
// The task

/// What a discovery found.
enum Looked {
    /// The server has no team spaces (protocol 1, or no `/spaces`).
    Unsupported,
    /// The team spaces of the account, as the server lists them (invitations among them).
    Listed(Vec<SpaceListItem>),
    /// It couldn't be found out (offline, a server error): nothing changes.
    Failed(String),
}

/// Asks the server about team spaces: its version first, then the list. A `404` for the list from a server that said it speaks
/// team spaces (a proxy while the server restarts, say) is a failed look, not the end of team spaces: the version is asked
/// once more, and only a server that now says protocol 1 is taken to have none.
async fn look(server: ServerUrl, token: DeviceToken) -> Looked {
    match crate::http::info(&server).await {
        Ok(info) if info.protocol_max >= TEAM_PROTOCOL => {}
        Ok(_) => return Looked::Unsupported,
        Err(e) => return Looked::Failed(e.to_string()),
    }
    let transport = match HttpTransport::new(server.clone(), &token) {
        Ok(transport) => transport,
        Err(e) => return Looked::Failed(e.to_string()),
    };
    match transport.spaces().await {
        Ok(list) => Looked::Listed(list.spaces.into_iter().filter(|i| i.kind == "team").collect()),
        Err(e @ (CallError::Transport(TransportError::NotFound) | CallError::Refused(crate::http::Refusal { status: 404, .. }))) => {
            match crate::http::info(&server).await {
                Ok(info) if info.protocol_max < TEAM_PROTOCOL => Looked::Unsupported,
                Ok(_) => Looked::Failed(e.to_string()),
                Err(again) => Looked::Failed(again.to_string()),
            }
        }
        Err(e) => Looked::Failed(e.to_string()),
    }
}

/// A synced team space's running parts.
struct Active {
    scheduler: Scheduler,
    stability: Stability,
    roots: Vec<PathBuf>,
    watcher: Option<FolderWatcher>,
    /// Counts the watchers started, so the end of an old one's events isn't taken for the current one stopping.
    generation: u64,
    /// Which running of the space's sync this is (a new number each time one is started), echoed by its passes' results.
    run: u64,
    /// The long-poll; `None` while syncing is paused by the user.
    poll: Option<tokio::task::JoinHandle<()>>,
    since: watch::Sender<Option<u64>>,
    approved: Vec<NodeId>,
    /// What the long-poll is started again with.
    polling: Polling,
    /// Where the pass thread looks up whether [`Active::run`] is still current; this running is taken out when it ends.
    live: Live,
}

/// What a long-poll of a team space is made of.
struct Polling {
    transport: HttpTransport,
    space: SpaceId,
    events: mpsc::UnboundedSender<Ev>,
    options: RunOptions,
}

impl Active {
    /// Starts the long-poll if it isn't running.
    fn start_poll(&mut self) {
        if self.poll.is_none() {
            let Polling { transport, space, events, options } = &self.polling;
            self.poll = Some(tokio::spawn(team_poll(transport.clone(), *space, self.since.subscribe(), events.clone(), *options)));
        }
    }

    /// Ends the long-poll (syncing is paused).
    fn stop_poll(&mut self) {
        if let Some(poll) = self.poll.take() {
            poll.abort();
        }
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        live_set(&self.live).remove(&self.run);
        self.stop_poll();
    }
}

/// A team space this computer knows: what is saved of it, and how its sync goes.
struct Team {
    setting: SpaceSetting,
    active: Option<Active>,
    /// The server no longer lists the space as the account's (or answered `404`).
    lost: bool,
    read_only: bool,
    last_sync_ms: Option<i64>,
    /// The space's live link, while its sync runs.
    hub: Option<Hub>,
    /// The read-only state the hub was last told.
    hub_read_only: bool,
    /// Hubs of the space that were stopped and have not ended yet: the next one starts after them, so two never share the
    /// space's live folder.
    stopping: u32,
}

/// A change of a space's sync that waits for its session to close.
struct Pending {
    id: u64,
    space: SpaceId,
    request: SpaceSyncRequest,
    reply: oneshot::Sender<Result<SpaceInfo, AgentError>>,
}

/// What applying a change of a space's sync came to.
enum Applied {
    Done(Box<SpaceInfo>),
    /// The space's session must be closed first (and its saved state forgotten with `forget`).
    NeedsClose {
        forget: bool,
    },
}

struct Manager {
    data: DataDir,
    account: Account,
    options: RunOptions,
    control: Control,
    start: Instant,
    teams: Vec<Team>,
    /// The server's list at the last discovery (team spaces only).
    listed: Vec<SpaceListItem>,
    /// Whether the server does team spaces (`None` until the first discovery got an answer).
    supported: Option<bool>,
    next_discovery: Ms,
    /// The discovery under way (it runs apart from the task, so a server that doesn't answer never holds it up).
    discovery: Option<tokio::task::JoinHandle<()>>,
    /// Who waits for the discovery under way, and for the one after it (asked for while that one ran).
    waiting: Vec<oneshot::Sender<()>>,
    waiting_next: Vec<oneshot::Sender<()>>,
    /// The last discovery failure that was noted, so a server that stays down is noted once.
    noted: Option<String>,
    /// A look at the list was asked for while one was under way (it may have started before the change that asks): one more
    /// follows it.
    again: bool,
    events: mpsc::UnboundedReceiver<Ev>,
    events_tx: mpsc::UnboundedSender<Ev>,
    jobs: std::sync::mpsc::Sender<ToThread>,
    /// Told when the thread of the passes has ended (`None`: the thread could not be started).
    thread_ended: Option<oneshot::Receiver<()>>,
    /// The current runnings of the spaces' syncs (see [`Active::live`]).
    live: Live,
    /// Team spaces whose passes may claim chunks from another space (see [`SpacesHandle::claim_from`]).
    claims: std::collections::BTreeMap<SpaceId, SpaceId>,
    /// Turning a space off or moving it waits for its session to close, apart from the task: those under way.
    pending: Vec<Pending>,
    /// The spaces whose raised generation could not be saved (account.json can't be written): it is saved again at every look at
    /// the list, and these spaces are not started meanwhile (the status says why). Other spaces and running syncs go on. Without
    /// this, a restart before the save would find the old generation on disk and trust a state that was meant to be dropped.
    unsaved: BTreeSet<SpaceId>,
    /// The last number given to a change in `pending`.
    changes: u64,
    /// When those waiting for a discovery stop being kept waiting for one that works.
    refresh_until: Option<Ms>,
    /// The last number given to a running of a space's sync ([`Active::run`]).
    runs: u64,
    /// The space whose pass runs now.
    in_flight: Option<SpaceId>,
    /// Where the next pass is looked for first, so spaces take turns.
    turn: usize,
    paused: bool,
    stopping: bool,
}

/// A random number for jitter and backoff (the spaces' schedulers only use it to spread retries out).
fn random() -> u64 {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

fn ms(d: Duration) -> Ms {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl Manager {
    fn new(config: SpacesConfig) -> Self {
        let SpacesConfig { data, account, options, control, gate, paused } = config;
        let (events_tx, events) = mpsc::unbounded_channel();
        let (jobs, jobs_rx) = std::sync::mpsc::channel();
        let (thread_data, thread_account, thread_events) = (data.clone(), account.clone(), events_tx.clone());
        let live = Live::default();
        let thread_live = live.clone();
        let (ended_tx, ended_rx) = oneshot::channel();
        // (The thread is not joined: the task waits for `ended_tx` to be dropped, which a second stop can cut short.)
        let thread_ended = std::thread::Builder::new()
            .name("craft-cloud-spaces".into())
            .spawn(move || {
                pass_thread(thread_data, thread_account, gate, thread_live, jobs_rx, thread_events);
                drop(ended_tx);
            })
            .ok()
            .map(|_| ended_rx);
        let teams = account
            .spaces
            .iter()
            .map(|setting| Team {
                setting: setting.clone(),
                active: None,
                lost: false,
                read_only: false,
                last_sync_ms: None,
                hub: None,
                hub_read_only: false,
                stopping: 0,
            })
            .collect();
        Self {
            data,
            account,
            options,
            control,
            start: Instant::now(),
            teams,
            listed: Vec::new(),
            supported: None,
            next_discovery: 0,
            discovery: None,
            waiting: Vec::new(),
            waiting_next: Vec::new(),
            noted: None,
            again: false,
            events,
            events_tx,
            jobs,
            thread_ended,
            live,
            claims: std::collections::BTreeMap::new(),
            pending: Vec::new(),
            unsaved: BTreeSet::new(),
            changes: 0,
            refresh_until: None,
            runs: 0,
            in_flight: None,
            turn: 0,
            paused,
            stopping: false,
        }
    }

    fn now(&self) -> Ms {
        ms(self.start.elapsed())
    }

    /// How often the server's list is looked at: every two long-poll waits (ten times less often when the server has no
    /// team spaces).
    fn discovery_every(&self) -> Ms {
        let every = ms(self.options.poll_wait).saturating_mul(2).max(1_000);
        if self.supported == Some(false) { every.saturating_mul(10) } else { every }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        self.publish();
        let mut at_once = false;
        loop {
            let now = self.now();
            if now >= self.next_discovery {
                self.start_discovery();
            }
            let wake = self.drive(self.now()).min(self.next_discovery);
            let sleep = tokio::time::sleep(Duration::from_millis(wake.saturating_sub(self.now())));
            tokio::select! {
                msg = rx.recv() => match msg {
                    None | Some(Msg::Stop) => break,
                    Some(Msg::StopNow) => {
                        at_once = true;
                        break;
                    }
                    Some(msg) => self.message(msg),
                },
                event = self.events.recv() => {
                    if let Some(event) = event {
                        self.event(event);
                    }
                }
                () = sleep => {}
            }
        }
        self.finish(at_once, &mut rx).await;
    }

    /// Lets go of every folder: a running pass finishes, then the thread ends. Stopping `at_once`, the thread is left to end
    /// by itself (with the pass it runs, if any) and no queued pass starts, as the personal pass thread is treated.
    async fn finish(mut self, at_once: bool, rx: &mut mpsc::UnboundedReceiver<Msg>) {
        for team in &mut self.teams {
            team.active = None;
            team.hub = None;
        }
        self.publish_hubs();
        // A space being turned off when the agent stops stays off: its `Close` (which forgets the state) is queued already.
        let mut turned_off = false;
        for pending in std::mem::take(&mut self.pending).into_iter().filter(|p| !p.request.on) {
            if let Some(setting) = self.account.spaces.iter_mut().find(|s| s.space == pending.space) {
                setting.sync = false;
                turned_off = true;
            }
        }
        if turned_off && let Err(e) = self.data.save_account(&self.account) {
            crate::runlog::note(format_args!("can't remember the team spaces: {e}"));
        }
        if let Some(discovery) = self.discovery.take() {
            discovery.abort();
        }
        let Self { jobs, thread_ended, .. } = self;
        drop(jobs);
        // A running pass finishes first. A second stop (`StopNow`) ends the wait: a pass that hangs (a server that does not
        // answer) never keeps the agent from stopping.
        if let Some(mut ended) = thread_ended
            && !at_once
        {
            loop {
                tokio::select! {
                    _ = &mut ended => break,
                    msg = rx.recv() => match msg {
                        Some(Msg::StopNow) | None => break,
                        // (Anything else is too late to matter.)
                        Some(_) => {}
                    },
                }
            }
        }
    }

    fn message(&mut self, msg: Msg) {
        match msg {
            Msg::Refresh(done) => {
                // A discovery already under way may have started before the change that asks for this one: the answer
                // waits for the one after it.
                if done.is_some() && self.refresh_until.is_none() {
                    self.refresh_until = Some(self.now().saturating_add(ms(REFRESH_BOUND)));
                }
                match (self.discovery.is_some(), done) {
                    (true, Some(done)) => self.waiting_next.push(done),
                    (false, Some(done)) => {
                        self.waiting.push(done);
                        self.start_discovery();
                    }
                    (true, None) => self.again = true,
                    (false, None) => self.start_discovery(),
                }
            }
            Msg::Sync(space, request, reply) => self.set_sync(space, request, reply),
            Msg::Claim(space, from) => match from {
                Some(from) => {
                    self.claims.insert(space, from);
                }
                None => {
                    self.claims.remove(&space);
                }
            },
            Msg::Paused(paused) => {
                self.paused = paused;
                // The long-polls rest while syncing is paused, as the personal one does, and start again on resume.
                for active in self.teams.iter_mut().filter_map(|t| t.active.as_mut()) {
                    if paused {
                        active.stop_poll();
                    } else {
                        active.start_poll();
                        active.scheduler.urgent_change();
                    }
                }
                // So do their live links (closed and not dialed while paused).
                for hub in self.teams.iter().filter_map(|t| t.hub.as_ref()) {
                    let handle = hub.handle();
                    tokio::spawn(async move {
                        let _ = tokio::time::timeout(HUB_ANSWER, handle.set_paused(paused)).await;
                    });
                }
                self.publish();
            }
            Msg::SyncSoon(space) => {
                let paused = self.paused;
                if let Some(active) = self.active_mut(space).filter(|_| !paused) {
                    active.scheduler.urgent_change();
                }
            }
            Msg::Quiesce => self.stopping = true,
            Msg::Stop | Msg::StopNow => {}
        }
    }

    fn event(&mut self, event: Ev) {
        let now = self.now();
        match event {
            Ev::Watch(space, generation, WatchEvent::Changed { paths, closed }) => {
                if let Some(active) = self.active_mut(space).filter(|a| a.generation == generation)
                    && counts_as_change(&active.roots, &active.stability, &paths, closed)
                {
                    active.scheduler.local_change(now);
                }
            }
            Ev::Watch(space, generation, WatchEvent::Lost(why)) => {
                crate::runlog::note(format_args!("the folder watcher of a team space lost track ({why}): looking at the whole folder"));
                if let Some(active) = self.active_mut(space).filter(|a| a.generation == generation) {
                    active.scheduler.urgent_change();
                    active.watcher = None;
                    active.scheduler.set_watching(false);
                }
            }
            Ev::WatcherGone(space, generation) => {
                if let Some(active) = self.active_mut(space).filter(|a| a.generation == generation && a.watcher.is_some()) {
                    active.scheduler.urgent_change();
                    active.watcher = None;
                    active.scheduler.set_watching(false);
                }
            }
            Ev::Remote(space) => {
                if let Some(active) = self.active_mut(space) {
                    active.scheduler.urgent_change();
                }
            }
            Ev::Back(space) => {
                if let Some(active) = self.active_mut(space) {
                    active.scheduler.server_back();
                }
            }
            // Looked at by a discovery at once: it lists the space or it doesn't.
            Ev::Gone => self.next_discovery = 0,
            Ev::Listed(looked) => self.listed(looked),
            Ev::Pass(done) => self.pass_done(*done, now),
            Ev::Closed(id, closed) => self.closed(id, closed),
            Ev::HubEnded(space) => {
                if let Some(index) = self.teams.iter().position(|t| t.setting.space == space) {
                    if let Some(team) = self.teams.get_mut(index) {
                        team.stopping = team.stopping.saturating_sub(1);
                    }
                    self.sync_hub(index);
                    self.publish_hubs();
                }
            }
            Ev::FolderMoved(space) => {
                if let Err(e) = self.raise_generation(space) {
                    crate::runlog::note(format_args!(
                        "can't save the raised generation of team space {space} yet (tried again at the next look): {e}"
                    ));
                }
                self.settled();
            }
        }
    }

    fn active_mut(&mut self, space: SpaceId) -> Option<&mut Active> {
        self.teams.iter_mut().find(|t| t.setting.space == space).and_then(|t| t.active.as_mut())
    }

    fn pass_done(&mut self, done: Done, now: Ms) {
        if self.in_flight == Some(done.space) {
            self.in_flight = None;
        }
        let Some(team) = self.teams.iter_mut().find(|t| t.setting.space == done.space) else { return };
        // A `404` may mean the account has no access any more: a discovery looks at once, and stops the space if the
        // server no longer lists it. Until then the pass counts as failed (retried with a growing delay), so a `404` that
        // means something else (a chunk the server lacks) never makes a loop.
        if matches!(done.result, Err(AgentError::Server(TransportError::NotFound))) {
            self.next_discovery = 0;
        }
        // The result of a pass of a sync that was stopped (or moved to another folder) since is not the current one's.
        let Some(active) = team.active.as_mut().filter(|a| a.run == done.run) else {
            self.publish();
            return;
        };
        if let Some(seq) = done.seq {
            let _ = active.since.send(Some(seq));
        }
        let pass = pass_end(&done.result, done.recheck_in);
        if let Some(last) = &done.last_pass
            && !matches!(last.outcome, Outcome::Failed(_))
        {
            team.last_sync_ms = Some(last.finished_ms);
        }
        if let Err(e) = &done.result {
            crate::runlog::note(format_args!("a team space's pass failed: {e}"));
        }
        active.scheduler.pass_finished(now, pass);
        self.publish();
    }

    /// Starts the pass that is due, if none runs; returns when to look again.
    fn drive(&mut self, now: Ms) -> Ms {
        let mut wake = Ms::MAX;
        let count = self.teams.len();
        if count == 0 || self.stopping {
            return wake;
        }
        for offset in 0..count {
            let index = (self.turn + offset) % count;
            let mut start: Option<(usize, Box<Job>)> = None;
            let paused = self.paused;
            let busy = self.in_flight.is_some();
            let joined_ms = self.teams.get(index).and_then(|t| self.item_of(t.setting.space)).and_then(|i| i.joined_ms);
            let Some(team) = self.teams.get_mut(index) else { continue };
            let read_only = team.read_only;
            let setting = team.setting.clone();
            let Some(active) = team.active.as_mut() else { continue };
            if take_due_peeks(&setting.folder, &active.stability) {
                active.scheduler.local_change(now);
            }
            if paused {
                continue;
            }
            match active.scheduler.next(now) {
                Next::RunPass if !busy => {
                    active.scheduler.pass_started();
                    let approve = std::mem::take(&mut active.approved);
                    let claim_from = self.claims.get(&setting.space).copied();
                    start = Some((
                        index,
                        Box::new(Job { setting, stability: active.stability.clone(), read_only, approve, joined_ms, run: active.run, claim_from }),
                    ));
                }
                Next::RunPass => {}
                Next::CheckApproval => {
                    let state_dir = self.data.space_dir(setting.space);
                    let summary = active_summary(&state_dir, &setting);
                    let waiting = [summary.as_ref().and_then(PassSummary::pending_set), summary.as_ref().and_then(PassSummary::held_set)];
                    if let Ok(Some(approval)) = state_dir.take_approval()
                        && waiting.contains(&Some(approval.set.clone()))
                    {
                        active.approved.extend(approval.nodes);
                        active.scheduler.urgent_change();
                    }
                    active.scheduler.approval_checked(now);
                    wake = wake.min(now);
                }
                Next::Wait(until) => wake = wake.min(until.unwrap_or(Ms::MAX)),
                Next::Exit => {}
            }
            if let Some(peek) = active.stability.next_peek_in() {
                wake = wake.min(now.saturating_add(ms(peek)));
            }
            if let Some((index, job)) = start {
                self.in_flight = Some(job.setting.space);
                self.turn = index + 1;
                if self.jobs.send(ToThread::Pass(job)).is_err() {
                    self.in_flight = None;
                }
                self.publish();
                // One pass at a time: the others are looked at again when it ends.
                return wake.min(now.saturating_add(1_000));
            }
        }
        wake
    }

    /// Starts asking the server which spaces the account is in (a discovery under way is let be).
    fn start_discovery(&mut self) {
        if self.discovery.is_some() {
            return;
        }
        self.next_discovery = self.now().saturating_add(self.discovery_every());
        let (server, token, events) = (self.account.server.clone(), self.account.token.clone(), self.events_tx.clone());
        self.discovery = Some(tokio::spawn(async move {
            let _ = events.send(Ev::Listed(look(server, token).await));
        }));
    }

    /// A discovery ended: the teams are brought in line with what it found, and whoever waited for it is told.
    fn listed(&mut self, looked: Looked) {
        self.discovery = None;
        let failed = matches!(looked, Looked::Failed(_));
        match looked {
            Looked::Listed(list) => {
                self.noted = None;
                self.supported = Some(true);
                self.listed = list;
                self.settled();
            }
            Looked::Unsupported => {
                self.listed.clear();
                self.supported = Some(false);
                self.settled();
            }
            Looked::Failed(why) => self.discovery_failed(&why),
        }
        if std::mem::take(&mut self.again) {
            self.start_discovery();
        }
        // Whoever waits for the list (an accepted invitation is to be found in it) is not told a list that is out of date: a
        // discovery that failed is tried again within seconds, and they wait for that, up to a bound.
        let now = self.now();
        if failed && self.refresh_until.is_some_and(|until| now < until) && !(self.waiting.is_empty() && self.waiting_next.is_empty()) {
            let later = std::mem::take(&mut self.waiting_next);
            self.waiting.extend(later);
            self.next_discovery = now.saturating_add(ms(DISCOVERY_RETRY));
            return;
        }
        self.refresh_until = None;
        for done in self.waiting.drain(..) {
            let _ = done.send(());
        }
        if !self.waiting_next.is_empty() {
            self.waiting = std::mem::take(&mut self.waiting_next);
            self.refresh_until = Some(now.saturating_add(ms(REFRESH_BOUND)));
            self.start_discovery();
        }
    }

    fn discovery_failed(&mut self, why: &str) {
        if self.noted.as_deref() != Some(why) {
            crate::runlog::note(format_args!("couldn't look at the account's team spaces: {why}"));
            self.noted = Some(why.to_owned());
        }
    }

    /// The list (or its absence) is known: settings, runs and the local API's view follow.
    fn settled(&mut self) {
        if !self.unsaved.is_empty() {
            match self.data.save_account(&self.account) {
                Ok(()) => self.unsaved.clear(),
                Err(e) => crate::runlog::note(format_args!("can't save a raised generation of a team space yet: {e}")),
            }
        }
        let shared = shared_root(&self.account.folder);
        let before = self.account.spaces.clone();
        let reconciled = reconcile(&self.account.spaces, &self.listed, shared.as_deref());
        for (space, _, _) in &reconciled.renamed {
            crate::runlog::note(format_args!("team space {space} was renamed on the server; its folder keeps its name"));
        }
        self.account.spaces = reconciled.settings;
        if self.account.spaces != before
            && let Err(e) = self.data.save_account(&self.account)
        {
            crate::runlog::note(format_args!("can't remember the team spaces: {e}"));
        }
        // Teams follow the settings (a new space is a new team).
        for setting in &self.account.spaces {
            match self.teams.iter_mut().find(|t| t.setting.space == setting.space) {
                Some(team) => team.setting = setting.clone(),
                None => self.teams.push(Team {
                    setting: setting.clone(),
                    active: None,
                    lost: false,
                    read_only: false,
                    last_sync_ms: None,
                    hub: None,
                    hub_read_only: false,
                    stopping: 0,
                }),
            }
        }
        for index in 0..self.teams.len() {
            self.follow(index);
            self.sync_hub(index);
        }
        self.publish_hubs();
        self.publish();
    }

    /// The listing of a team's space, if the account has it as an active member.
    fn item_of(&self, space: SpaceId) -> Option<&SpaceListItem> {
        self.listed.iter().find(|i| i.id == space && i.state == "active")
    }

    /// Starts or stops the team's sync to match what the settings, the server's list and the server's version say.
    fn follow(&mut self, index: usize) {
        let supported = self.supported != Some(false);
        let Some(team) = self.teams.get(index) else { return };
        let space = team.setting.space;
        let listed = (self.supported == Some(true)).then(|| self.item_of(space).cloned());
        // Only a discovery that got an answer says the space is gone.
        let lost = match &listed {
            Some(item) => item.is_none(),
            None => team.lost,
        };
        let read_only = listed.as_ref().and_then(Option::as_ref).is_some_and(|i| i.role == SpaceRole::Viewer || i.archived);
        let joined_ms = listed.as_ref().and_then(Option::as_ref).and_then(|i| i.joined_ms);
        // A space whose session is being closed (turned off, moved) is not started again before that is done.
        let changing = self.pending.iter().any(|p| p.space == space);
        let want = team.setting.sync && supported && !lost && !self.stopping && !changing;
        // Access was just lost: the saved state goes, once the session is closed, so that if access comes back the folder
        // is merged with the space afresh (what was deleted on either side meanwhile is not deleted on the other).
        let newly_lost = lost && !team.lost;
        if newly_lost && let Err(e) = self.raise_generation(space) {
            crate::runlog::note(format_args!("can't save the raised generation of lost team space {space} yet (tried again at the next look): {e}"));
        }
        let timing = self.options.timing;
        let (options, events, now, paused) = (self.options, self.events_tx.clone(), self.now(), self.paused);
        let (data, account, live) = (self.data.clone(), self.account.clone(), self.live.clone());
        let may_start = !self.unsaved.contains(&space);
        self.runs += 1;
        let run = self.runs;
        let Some(team) = self.teams.get_mut(index) else { return };
        team.lost = lost;
        // A role that changed in either direction is acted on by a pass now: a viewer made an editor uploads what it kept,
        // an editor made a viewer stops before its next write.
        if team.read_only != read_only
            && let Some(active) = team.active.as_mut()
        {
            active.scheduler.urgent_change();
        }
        team.read_only = read_only;
        match (want, team.active.is_some()) {
            (true, false) if may_start => {
                team.active = start_active(&data, &account, &team.setting, joined_ms, timing, options, events, now, (run, live), paused);
            }
            (false, true) => {
                team.active = None;
                let _ = self.jobs.send(ToThread::Close { space, forget: newly_lost, ack: None });
            }
            (false, false) if newly_lost => {
                let _ = self.jobs.send(ToThread::Close { space, forget: true, ack: None });
            }
            _ => {}
        }
    }

    /// The team's live link follows its sync: started with it, stopped when it stops (nothing of the space's documents is
    /// deleted: they and their journals stay in `live/<space>/`), and told when the role changes.
    fn sync_hub(&mut self, index: usize) {
        let (data, account, options, control, paused) = (self.data.clone(), self.account.clone(), self.options, self.control.clone(), self.paused);
        let events = self.events_tx.clone();
        let Some(team) = self.teams.get_mut(index) else { return };
        match (team.active.is_some(), team.hub.is_some()) {
            // The hub this one replaces has not ended: the space's live folder is still its (`Ev::HubEnded` comes back here).
            (true, false) if team.stopping > 0 => {}
            (true, false) => {
                let config = HubConfig {
                    data,
                    space: team.setting.space,
                    server: account.server.clone(),
                    token: account.token.clone(),
                    machine: account.device_name.clone(),
                    folder: team.setting.folder.clone(),
                    record_delay: options.record_delay,
                    session_grace: options.session_grace,
                    announce_delay: options.announce_delay,
                    team: true,
                    username: account.username.clone(),
                    read_only: team.read_only,
                    generation: team.setting.generation,
                };
                match Hub::start(config, control, paused) {
                    Ok(hub) => {
                        team.hub = Some(hub);
                        team.hub_read_only = team.read_only;
                    }
                    Err(e) => crate::runlog::note(format_args!("live documents of team space {} are off: {e}", team.setting.space)),
                }
            }
            (false, true) => stop_hub(team, &events),
            _ => {}
        }
        if let Some(hub) = &team.hub
            && team.hub_read_only != team.read_only
        {
            hub.handle().set_read_only(team.read_only);
            team.hub_read_only = team.read_only;
        }
    }

    /// Tells the local API which team spaces have a live link and where their folders are.
    fn publish_hubs(&self) {
        let hubs = self
            .teams
            .iter()
            .filter_map(|t| t.hub.as_ref().map(|h| TeamHub { space: t.setting.space, folder: t.setting.folder.clone(), handle: h.handle() }))
            .collect();
        self.control.set_team_hubs(hubs);
    }

    /// Turns the sync of a team space on or off here. Turning it off, or moving it to another folder, first closes the
    /// space's session; the task does not wait for that (other spaces, a stop and the local API go on), and the caller is
    /// answered once it is done or [`CLOSE_WAIT`] has passed ([`Manager::closed`]).
    fn set_sync(&mut self, space: SpaceId, request: SpaceSyncRequest, reply: oneshot::Sender<Result<SpaceInfo, AgentError>>) {
        if self.pending.iter().any(|p| p.space == space) {
            let _ = reply.send(Err(AgentError::Usage(CHANGING.into())));
            return;
        }
        match self.apply_sync(space, &request, None) {
            Ok(Applied::Done(info)) => {
                let _ = reply.send(Ok(*info));
            }
            Ok(Applied::NeedsClose { forget }) => {
                self.changes += 1;
                let id = self.changes;
                self.pending.push(Pending { id, space, request, reply });
                self.close_session(id, space, forget);
            }
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    }

    /// The wait for a session to close ended: the change that waited for it is applied and its caller answered.
    fn closed(&mut self, id: u64, closure: Closure) {
        let Some(index) = self.pending.iter().position(|p| p.id == id) else { return };
        let Pending { space, request, reply, .. } = self.pending.remove(index);
        // A state that could not be removed is not acknowledged as gone: the change fails, with the error. (The generation
        // was raised and saved before, so that state is not used again either way.)
        if let Closure::NotForgotten(why) = &closure {
            self.settled();
            let _ = reply.send(Err(AgentError::Usage(format!("the saved state of this space could not be removed: {why}"))));
            return;
        }
        let answer = match self.apply_sync(space, &request, Some(matches!(closure, Closure::Closed))) {
            Ok(Applied::Done(info)) => Ok(*info),
            Ok(Applied::NeedsClose { .. }) => Err(AgentError::Usage("the space's session could not be closed; try again in a moment".into())),
            Err(e) => Err(e),
        };
        let _ = reply.send(answer);
    }

    /// Applies turning a space's sync on or off. `closed` is `None` before the space's session was closed, and afterwards
    /// whether it was closed within the wait; a change that needs the session closed first answers
    /// [`Applied::NeedsClose`] while it is `None`.
    fn apply_sync(&mut self, space: SpaceId, request: &SpaceSyncRequest, closed: Option<bool>) -> Result<Applied, AgentError> {
        let not_found = || AgentError::Server(TransportError::NotFound);
        let known = self.account.spaces.iter().position(|s| s.space == space);
        let item = self.item_of(space).cloned();
        if request.on && (self.supported == Some(false)) {
            return Err(AgentError::Usage(NEEDS_UPDATE.into()));
        }
        if request.on && item.is_none() {
            return Err(not_found());
        }
        if !request.on {
            let index = known.ok_or_else(not_found)?;
            // The session ends before the setting says off, so a pass that is running finishes first (if it takes longer
            // than the wait, turning the sync on again waits for it then). Its saved state is forgotten: turned on again,
            // in this folder or another, the space starts afresh as a first sync, so what was deleted here or on the server
            // while it was off is not deleted on the other side.
            if closed.is_none() {
                // The old state is made unusable before it is removed: a generation raised and saved first, so a state that
                // survives its removal (a file that can't be deleted, a crash in between) is not trusted when the space is
                // turned on again.
                self.raise_generation(space)?;
                return Ok(Applied::NeedsClose { forget: true });
            }
            if let Some(setting) = self.account.spaces.get_mut(index) {
                setting.sync = false;
            }
        } else {
            let item = item.ok_or_else(not_found)?;
            let root = item.root.ok_or_else(|| AgentError::Usage("the server didn't say where the space starts; try again".into()))?;
            let chosen = request.folder.as_deref().map(PathBuf::from);
            let current = known.and_then(|i| self.account.spaces.get(i)).map(|s| s.folder.clone());
            let shared = shared_root(&self.account.folder);
            let taken: Vec<PathBuf> = self.account.spaces.iter().filter(|s| s.space != space).map(|s| s.folder.clone()).collect();
            let name = item.name.clone().unwrap_or_default();
            let folder = match (chosen, current, shared) {
                (Some(folder), _, _) => folder,
                (None, Some(folder), _) if !folder.as_os_str().is_empty() => folder,
                (None, _, Some(shared)) => free_folder(&shared, &name, &taken),
                _ => return Err(AgentError::Usage("choose a folder for the space (there is no place beside the personal folder)".into())),
            };
            if !folder.is_absolute() {
                return Err(AgentError::Usage("the folder must be an absolute path".into()));
            }
            self.check_folder(space, &folder)?;
            // A sync that goes on in the same folder is left running. Otherwise its session is closed first, and the pass
            // thread has let go of it before the new folder is set up: a pass for the old folder must not write its state
            // after that. The wait is only for a space that has a session here or a pass under way: nothing else of the space
            // can write its state, any older `Close` of it is ahead in the thread's queue of whatever the thread does for
            // another space, and the thread may be busy with that for minutes.
            let running = self.teams.iter().any(|t| t.setting.space == space && t.active.is_some());
            let unchanged = running && known.and_then(|i| self.account.spaces.get(i)).is_some_and(|s| s.sync && s.folder == folder);
            if !unchanged {
                match closed {
                    None if running || self.in_flight == Some(space) => return Ok(Applied::NeedsClose { forget: false }),
                    None | Some(true) => {}
                    Some(false) => return Err(AgentError::Usage(STILL_FINISHING.into())),
                }
            }
            // Setting up a folder that has no marker yet installs one and starts the space's state afresh. The old state is
            // made unusable (the generation raised and saved) before the marker is written, so a crash between the marker and
            // the removal of the state leaves a state that is never trusted for this folder.
            let data = self.data.clone();
            set_up_folder(&data, &folder, space, root, request.adopt, || self.raise_generation(space))?;
            let generation = known.and_then(|i| self.account.spaces.get(i)).map_or(0, |s| s.generation);
            let setting = SpaceSetting { space, folder, sync: true, name: item.name.clone(), root: Some(root), generation };
            match known {
                Some(index) => {
                    if let Some(slot) = self.account.spaces.get_mut(index) {
                        *slot = setting;
                    }
                }
                None => self.account.spaces.push(setting),
            }
        }
        self.data.save_account(&self.account)?;
        self.settled();
        self.info_of(space).map(|info| Applied::Done(Box::new(info))).ok_or_else(not_found)
    }

    /// Raises the generation of the space's setting and saves it, so a state made under the old one is not used again.
    /// A raise that could not be saved stays in memory and is saved again at the next look at the list; until then no space is
    /// started (see `unsaved`), so a state that was meant to be unusable is not used by a session that begins.
    fn raise_generation(&mut self, space: SpaceId) -> Result<(), AgentError> {
        if let Some(setting) = self.account.spaces.iter_mut().find(|s| s.space == space) {
            setting.generation = setting.generation.saturating_add(1);
            if let Err(e) = self.data.save_account(&self.account) {
                self.unsaved.insert(space);
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Stops the space's sync here and asks the pass thread to close its session (a pass that was running has ended and
    /// nothing of the space writes its state any more once it is closed). The task is told by [`Ev::Closed`], `false` if
    /// that didn't happen within [`CLOSE_WAIT`].
    fn close_session(&mut self, id: u64, space: SpaceId, forget: bool) {
        let events = self.events_tx.clone();
        if let Some(team) = self.teams.iter_mut().find(|t| t.setting.space == space) {
            team.active = None;
            stop_hub(team, &events);
        }
        self.publish_hubs();
        let (ack, closed) = oneshot::channel();
        let events = self.events_tx.clone();
        if self.jobs.send(ToThread::Close { space, forget, ack: Some(ack) }).is_err() {
            let _ = events.send(Ev::Closed(id, Closure::Closed));
            return;
        }
        tokio::spawn(async move {
            // A thread that has ended drops the acknowledgement: nothing runs then either.
            let closure = match tokio::time::timeout(CLOSE_WAIT, closed).await {
                Ok(Ok(Err(why))) => Closure::NotForgotten(why),
                Ok(_) => Closure::Closed,
                Err(_) => Closure::TimedOut,
            };
            let _ = events.send(Ev::Closed(id, closure));
        });
    }

    /// A team folder must not lie inside another synced folder (or the other way round), nor the agent's data folder.
    fn check_folder(&self, space: SpaceId, folder: &Path) -> Result<(), AgentError> {
        crate::cli::check_apart(&self.data, folder)?;
        let others =
            std::iter::once(&self.account.folder).chain(self.account.spaces.iter().filter(|s| s.space != space && s.sync).map(|s| &s.folder));
        for other in others {
            if crate::cli::nested(other, folder) {
                return Err(AgentError::Usage(format!(
                    "{} lies inside another synced folder, or holds one ({}); choose a folder of its own",
                    folder.display(),
                    other.display()
                )));
            }
        }
        Ok(())
    }

    // The local API's view -----------------------------------------------------------------------------------------

    fn run_of(&self, team: &Team) -> Option<SpaceRun> {
        if team.lost {
            return Some(SpaceRun::NoAccess {
                message: format!(
                    "You no longer have access to {}. Your local copy is kept and no longer syncs.",
                    team.setting.name.as_deref().unwrap_or("this space")
                ),
            });
        }
        if team.setting.sync && self.supported == Some(false) {
            return Some(SpaceRun::Unavailable { message: NEEDS_UPDATE.into() });
        }
        if team.active.is_none() && team.setting.sync && self.unsaved.contains(&team.setting.space) {
            return Some(SpaceRun::Paused { reason: UNSAVED.into(), reason_code: None });
        }
        let active = team.active.as_ref()?;
        if self.paused {
            return Some(SpaceRun::PausedByUser);
        }
        Some(match active.scheduler.phase() {
            Phase::Idle => SpaceRun::Idle,
            Phase::Syncing => SpaceRun::Syncing,
            Phase::Retrying { message, .. } => SpaceRun::Retrying { message },
            Phase::Paused { reason, reason_code, .. } => SpaceRun::Paused { reason, reason_code },
            Phase::Stopping => SpaceRun::Idle,
        })
    }

    fn info_of(&self, space: SpaceId) -> Option<SpaceInfo> {
        let team = self.teams.iter().find(|t| t.setting.space == space);
        let item = self.listed.iter().find(|i| i.id == space);
        self.info(team, item)
    }

    fn info(&self, team: Option<&Team>, item: Option<&SpaceListItem>) -> Option<SpaceInfo> {
        let space = team.map(|t| t.setting.space).or_else(|| item.map(|i| i.id))?;
        let name = item.and_then(|i| i.name.clone()).or_else(|| team.and_then(|t| t.setting.name.clone()));
        let invited = item.is_some_and(|i| i.state != "active");
        let run_state = team.and_then(|t| self.run_of(t));
        let sync = match team {
            Some(t) if t.lost || (t.setting.sync && self.supported == Some(false)) => SpaceSync::Unavailable,
            Some(t) if t.setting.sync && !invited => SpaceSync::On,
            _ => SpaceSync::Off,
        };
        Some(SpaceInfo {
            space,
            kind: "team".into(),
            name,
            role: item.map_or(SpaceRole::Viewer, |i| i.role),
            state: if invited { "invited" } else { "active" }.into(),
            archived: item.is_some_and(|i| i.archived),
            folder: team.filter(|t| !invited && !t.setting.folder.as_os_str().is_empty()).map(|t| t.setting.folder.display().to_string()),
            sync,
            run_state,
            last_sync_ms: team.and_then(|t| t.last_sync_ms),
            read_only: team.is_some_and(|t| t.read_only && t.active.is_some()),
        })
    }

    /// Tells the local API what the spaces look like now.
    fn publish(&self) {
        let mut infos: Vec<SpaceInfo> = Vec::new();
        for item in &self.listed {
            let team = self.teams.iter().find(|t| t.setting.space == item.id);
            infos.extend(self.info(team, Some(item)));
        }
        // Spaces this computer remembers that the server doesn't list: the account lost them (or the server is too old).
        for team in self.teams.iter().filter(|t| !self.listed.iter().any(|i| i.id == t.setting.space)) {
            infos.extend(self.info(Some(team), None));
        }
        let status: Vec<SpaceStatus> = infos
            .iter()
            .filter(|i| i.sync != SpaceSync::Off)
            .map(|i| SpaceStatus {
                space: i.space,
                run_state: i.run_state.clone().unwrap_or(SpaceRun::Idle),
                last_sync_ms: i.last_sync_ms,
                read_only: i.read_only,
            })
            .collect();
        self.control.set_spaces(infos, status);
    }
}

/// The last pass summary a team space's own `state.json` holds, if that state is for the space's folder.
fn active_summary(state_dir: &DataDir, setting: &SpaceSetting) -> Option<PassSummary> {
    state_dir.load_state_for(setting.space, &setting.folder, None, setting.generation).ok().and_then(|(s, _)| s.last_pass).and_then(|l| l.summary)
}

/// Stops a team's hub. The next hub of the space starts when this one has ended (at most [`HUB_END_WAIT`] later): it writes its
/// documents and closes its connection first, and must not do that while another hub works in the same folder.
fn stop_hub(team: &mut Team, events: &mpsc::UnboundedSender<Ev>) {
    let Some(hub) = team.hub.take() else { return };
    let ended = hub.stop();
    team.stopping += 1;
    let (space, events) = (team.setting.space, events.clone());
    tokio::spawn(async move {
        let _ = tokio::time::timeout(HUB_END_WAIT, ended).await;
        let _ = events.send(Ev::HubEnded(space));
    });
}

/// The parts of a synced team space: scheduler, folder watcher, long-poll.
#[allow(clippy::too_many_arguments)]
fn start_active(
    data: &DataDir,
    account: &Account,
    setting: &SpaceSetting,
    joined_ms: Option<i64>,
    timing: crate::schedule::Timing,
    options: RunOptions,
    events: mpsc::UnboundedSender<Ev>,
    now: Ms,
    (run, live): (u64, Live),
    paused: bool,
) -> Option<Active> {
    let space = setting.space;
    let transport = HttpTransport::new(account.server.clone(), &account.token).and_then(|t| t.bound_to(space)).ok()?;
    let since = data
        .space_dir(space)
        .load_state_for(space, &setting.folder, joined_ms, setting.generation)
        .ok()
        .and_then(|(saved, _)| saved.state.space.map(|_| saved.state.seq));
    let (since_tx, _) = watch::channel(since);
    let polling = Polling { transport, space, events: events.clone(), options };
    let mut scheduler = Scheduler::new(timing, now, random);
    let roots = crate::watch::roots(&setting.folder);
    let generation = 1;
    let watcher = {
        let (wtx, mut wrx) = mpsc::unbounded_channel();
        match crate::watch::watch(&setting.folder, wtx) {
            Ok(watcher) => {
                let forward = events;
                tokio::spawn(async move {
                    while let Some(event) = wrx.recv().await {
                        if forward.send(Ev::Watch(space, generation, event)).is_err() {
                            return;
                        }
                    }
                    let _ = forward.send(Ev::WatcherGone(space, generation));
                });
                Some(watcher)
            }
            Err(_) => None,
        }
    };
    scheduler.set_watching(watcher.is_some());
    let mut active = Active {
        scheduler,
        stability: Stability::new(timing.stability),
        roots,
        watcher,
        generation,
        run,
        poll: None,
        since: since_tx,
        approved: Vec::new(),
        polling,
        live: live.clone(),
    };
    live_set(&live).insert(run);
    if !paused {
        active.start_poll();
    }
    Some(active)
}

/// For tests: the folder at which the next set-up stops right after its marker is written, as if the agent had crashed there.
#[doc(hidden)]
pub static STOP_AFTER_MARKER: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Whether the set-up of `folder` is to stop now (once; see [`STOP_AFTER_MARKER`]).
fn stop_after_marker(folder: &Path) -> bool {
    let mut armed = STOP_AFTER_MARKER.lock().unwrap_or_else(PoisonError::into_inner);
    if armed.as_deref() == Some(folder) {
        *armed = None;
        return true;
    }
    false
}

/// Makes `folder` the folder of `space`, whose root is `root`: a new or empty folder gets the space's marker; one with
/// files is taken only with `adopt` (what differs from the space's becomes conflict copies, nothing is deleted); one that
/// already has this space's marker is taken as it is; another space's marker refuses it.
fn set_up_folder(
    data: &DataDir,
    folder: &Path,
    space: SpaceId,
    root: NodeId,
    adopt: bool,
    invalidate_state: impl FnOnce() -> Result<(), AgentError>,
) -> Result<(), AgentError> {
    let marker = RootMarker { space, root };
    let existing = if folder.is_dir() { root::read(folder)? } else { None };
    match existing {
        Some(found) if found == marker => return Ok(()),
        Some(_) => return Err(root::RootError::SetUpForOtherSpace(folder.to_owned()).into()),
        None => {}
    }
    if folder.is_dir() && crate::cli::has_files(folder)? && !adopt {
        return Err(AgentError::FolderHasFiles(folder.to_owned()));
    }
    invalidate_state()?;
    std::fs::create_dir_all(folder).map_err(|source| AgentError::Io { path: folder.to_owned(), source })?;
    root::write(folder, &marker)?;
    if stop_after_marker(folder) {
        return Err(AgentError::Usage("stopped after the marker was written (test hook)".into()));
    }
    // A fresh folder starts from a fresh state: what a state remembers of an older folder must not decide what this one
    // is missing.
    data.space_dir(space).remove_state()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_cloud_proto::Epoch;

    fn item(name: &str, state: &str) -> SpaceListItem {
        SpaceListItem {
            id: SpaceId::generate(),
            kind: "team".into(),
            name: Some(name.into()),
            role: SpaceRole::Editor,
            state: state.into(),
            archived: false,
            created_ms: 0,
            members: 2,
            root: (state == "active").then(NodeId::generate),
            epoch: (state == "active").then(Epoch::generate),
            joined_ms: (state == "active").then_some(1),
        }
    }

    #[test]
    fn folder_names_are_safe_file_names() {
        assert_eq!(folder_name("Studio"), "Studio");
        assert_eq!(folder_name("Plans: 2026 / v2"), "Plans_ 2026 _ v2");
        assert_eq!(folder_name("a<b>c|d?e*f\\g\"h"), "a_b_c_d_e_f_g_h");
        assert_eq!(folder_name("Team..."), "Team", "no dots at the end, which Windows drops");
        assert_eq!(folder_name(".hidden"), "_hidden");
        assert_eq!(folder_name(".craft-cloud"), "_craft-cloud");
        assert_eq!(folder_name("CON"), "CON (team space)", "a reserved name");
        assert_eq!(folder_name("   "), "Team space");
        assert_eq!(folder_name("tab\there"), "tab_here");
        assert_eq!(folder_name(&"x".repeat(300)).chars().count(), MAX_FOLDER_CHARS);
        assert_eq!(folder_name("Cafe\u{301}"), "Caf\u{e9}", "NFC, as file names are");
        for name in ["Studio", "a/b", "con", "..", "x\u{0}y", "\u{202e}rtl"] {
            assert!(Name::new(&folder_name(name)).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn a_taken_folder_name_is_numbered() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join(SHARED_FOLDER);
        assert_eq!(free_folder(&shared, "Studio", &[]), shared.join("Studio"), "a shared folder that doesn't exist yet");
        std::fs::create_dir_all(shared.join("Studio")).unwrap();
        assert_eq!(free_folder(&shared, "Studio", &[]), shared.join("Studio (2)"), "a folder on disk");
        let taken = vec![shared.join("Studio (2)"), shared.join("studio (3)")];
        assert_eq!(free_folder(&shared, "Studio", &taken), shared.join("Studio (4)"), "folders of other spaces, whatever the case");
        assert_eq!(free_folder(&shared, "Other", &taken), shared.join("Other"));
        assert_eq!(shared_root(Path::new("/home/ana/Craft Cloud")), Some(PathBuf::from("/home/ana").join(SHARED_FOLDER)));
    }

    #[test]
    fn a_discovery_adds_new_spaces_with_syncing_off_and_follows_renames() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join(SHARED_FOLDER);
        let (a, b, invited) = (item("Plans: 1", "active"), item("Plans? 1", "active"), item("Invited", "invited"));
        let first = reconcile(&[], &[a.clone(), b.clone(), invited], Some(&shared));
        assert_eq!(first.settings.len(), 2, "an invitation has no setting");
        assert!(first.settings.iter().all(|s| !s.sync && s.root.is_some()));
        let folders: Vec<_> = first.settings.iter().map(|s| s.folder.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(folders, ["Plans_ 1", "Plans_ 1 (2)"]);
        // The same list again changes nothing; a rename changes the name and keeps the folder.
        assert_eq!(reconcile(&first.settings, &[a.clone(), b.clone()], Some(&shared)).settings, first.settings);
        let renamed = SpaceListItem { name: Some("Plans two".into()), ..a.clone() };
        let second = reconcile(&first.settings, &[renamed, b.clone()], Some(&shared));
        assert_eq!(second.renamed, vec![(a.id, "Plans: 1".to_owned(), "Plans two".to_owned())]);
        assert_eq!(
            second.settings.first().map(|s| (s.folder.clone(), s.name.clone())),
            Some((first.settings[0].folder.clone(), Some("Plans two".into())))
        );
        // A space that is no longer listed keeps its setting (its folder and name say which it was).
        assert_eq!(reconcile(&first.settings, &[b], Some(&shared)).settings, first.settings);
        // Without a place for folders, a new space gets no setting yet.
        assert!(reconcile(&[], &[a], None).settings.is_empty());
    }
}
