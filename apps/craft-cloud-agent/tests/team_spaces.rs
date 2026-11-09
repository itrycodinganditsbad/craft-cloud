//! The agent with several spaces, against a real server on a loopback port: an invitation reaches the local API, is
//! accepted there, the team folder appears beside the personal one and syncs both ways between two accounts; a role
//! lowered to viewer makes the folder read-only; a removed member keeps the local copy, which stops syncing and is reported;
//! a renamed space keeps its folder; folder names clash and are made safe; an answer without the space's echo is refused;
//! an old server shows no team spaces and is never sent the `craft-space` header; and a Phase 1 data folder starts unchanged.
//! Review round: a state belongs to its folder (moving a space, turning it off and on, signing out), regained access merges
//! instead of resuming, held deletions of a team space are approved from the command line and the local API, a second stop
//! ends at once with a silent server, an archived space stays readable, pausing rests the long-polls, and one `404` for the
//! list does not end team spaces.
//! Second review round: turning a space off forgets its state, access lost and regained while the agent is stopped merges, a
//! space being turned off does not hold the spaces task up (a second stop, other calls, a queued pass), and a failed discovery
//! is retried for those waiting for the list.
//! Live documents in team spaces (live protocol 3): a document in a team folder is routed to the space's own live link; two
//! accounts and a viewer, identity only for apps that ask, a role changed while connected, journals that wait through a
//! removal, two spaces at once, blobs and recents of a space, a server that cannot serve the link, and one Ctrl-C with a hung
//! team pass.
//! Every wait is for a condition, with a timeout; the loops get short timings so the tests stay quick.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderName, Request, Response, StatusCode};
use craft_cloud_agent::account::DataDir;
use craft_cloud_agent::cli::{self, AgentError, Exit, LoginArgs};
use craft_cloud_agent::daemon::{RunOptions, run_loop};
use craft_cloud_agent::http::{HttpTransport, ServerUrl};
use craft_cloud_agent::schedule::Timing;
use craft_cloud_app::live::{Document, Documents, Event, NotLiveReason, OpenOptions, Opened};
use craft_cloud_app::{Client, NewRecent, SpaceInfo, SpaceList, SpaceRole, SpaceRun, SpaceSync, SpaceSyncRequest};
use craft_cloud_meta::{Meta, Role, SpaceActor, UserRecord};
use craft_cloud_proto::{SpaceId, SpaceName};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_sync::Transport;
use loro::{ExportMode, LoroDoc};
use tokio::sync::mpsc;

const PASSWORD: &str = "correct horse battery";
/// Longest wait for anything to show up.
const PATIENCE: Duration = Duration::from_secs(40);

/// A server on a loopback port with two accounts, `ana` and `ben`.
struct World {
    url: String,
    meta: Meta,
    ana: UserRecord,
    ben: UserRecord,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dirs: Vec<tempfile::TempDir>,
}

async fn world() -> World {
    let (db, blobs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let state = AppState::new(meta.clone(), store, vec![]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = craft_cloud_server::serve(state, listener, async move {
            let _ = rx.await;
        })
        .await;
    });
    let hash = hash_password(PASSWORD).unwrap();
    let ana = meta.create_user("ana", &hash, Role::Member, 0).await.unwrap();
    let ben = meta.create_user("ben", &hash, Role::Member, 0).await.unwrap();
    World { url, meta, ana, ben, _shutdown: tx, _dirs: vec![db, blobs] }
}

impl World {
    /// A team space `name` owned by ana.
    async fn space(&self, name: &str) -> SpaceId {
        self.meta.create_team_space(&SpaceActor::Cli, &SpaceName::new(name).unwrap(), self.ana.id, None, 0).await.unwrap()
    }

    /// Ana (the owner) invites ben: a pending invitation that gives no access until he accepts it. (The command line's
    /// invitation is accepted at once.)
    async fn invite_ben(&self, space: SpaceId, role: SpaceRole) {
        let ana = SpaceActor::User { user: self.ana.id, device: None };
        self.meta.invite_member(&ana, space, self.ben.id, role, 0).await.unwrap();
    }
}

/// Timings for tests: everything in fractions of a second.
fn quick() -> RunOptions {
    RunOptions {
        timing: Timing {
            debounce: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            stability: Duration::from_millis(200),
            periodic: Duration::from_secs(600),
            unwatched_periodic: Duration::from_millis(500),
            backoff_first: Duration::from_millis(200),
            backoff_max: Duration::from_secs(1),
            recheck: Duration::from_millis(300),
            recheck_slow: Duration::from_secs(5),
            approval_check: Duration::from_millis(100),
            held_check: Duration::from_millis(300),
        },
        heartbeat: Duration::from_millis(200),
        // The spaces are looked at every two of these.
        poll_wait: Duration::from_millis(500),
        poll_retry_first: Duration::from_millis(100),
        poll_retry_max: Duration::from_secs(1),
        poll_min_spacing: Duration::from_millis(100),
        poll_busy: Duration::from_millis(200),
        fail_watch: false,
        record_delay: Duration::ZERO,
        session_grace: Duration::from_secs(10),
        announce_delay: Duration::ZERO,
        autostart: craft_cloud_agent::autostart::Target::Off,
    }
}

type LoopResult = (Result<Exit, AgentError>, String);

struct Running {
    stop: mpsc::UnboundedSender<()>,
    thread: Option<std::thread::JoinHandle<LoopResult>>,
}

impl Running {
    async fn stop(mut self) -> LoopResult {
        let _ = self.stop.send(());
        let thread = self.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap()).await.unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // A test that ends (or failed) midway: two stops end the loop at once, and it is waited for, so no loop is left
        // writing into a folder the test removes.
        let _ = self.stop.send(());
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// An account on a computer: its data folder, its personal folder, and the loop that runs there.
struct Person {
    /// First, so the loop is stopped before the folders go.
    running: Option<Running>,
    data: DataDir,
    personal: PathBuf,
    dir: tempfile::TempDir,
}

impl Person {
    async fn signed_in(url: &str, user: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let person = Self { running: None, data: DataDir::new(dir.path().join("data")), personal: dir.path().join("Craft Cloud"), dir };
        let args = LoginArgs {
            url: url.to_owned(),
            user: user.to_owned(),
            folder: Some(person.personal.clone()),
            device_name: Some(format!("{user}-pc")),
            adopt: false,
        };
        cli::login(&person.data, &args, PASSWORD, &mut Vec::new()).await.unwrap();
        person
    }

    fn run(&mut self) {
        self.run_with(quick());
    }

    /// Waits until the loop's local API answers.
    async fn ready(&self) {
        eventually("the local API answers", || async { self.api(|c| c.status()).await.is_ok() }).await;
    }

    fn run_with(&mut self, options: RunOptions) {
        let (stop, stops) = mpsc::unbounded_channel();
        let data = self.data.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut out = Vec::new();
            let exit = runtime.block_on(run_loop(&data, options, stops, &mut out));
            (exit, String::from_utf8_lossy(&out).into_owned())
        });
        self.running = Some(Running { stop, thread: Some(thread) });
    }

    async fn stop(&mut self) -> LoopResult {
        self.running.take().unwrap().stop().await
    }

    /// The shared place of team folders: beside the personal folder.
    fn shared(&self) -> PathBuf {
        self.dir.path().join("Craft Cloud Shared")
    }

    /// A client of this computer's agent (for what blocks: the app stand-ins run it off the async threads).
    fn client(&self) -> Client {
        Client::connect_in(self.data.path()).unwrap()
    }

    /// A call of the local API, off the async threads (the client blocks).
    async fn api<T: Send + 'static>(
        &self,
        call: impl FnOnce(Client) -> Result<T, craft_cloud_app::Error> + Send + 'static,
    ) -> Result<T, craft_cloud_app::Error> {
        let dir = self.data.path().to_owned();
        tokio::task::spawn_blocking(move || call(Client::connect_in(&dir)?)).await.unwrap()
    }

    async fn spaces(&self) -> Option<SpaceList> {
        self.api(|c| c.spaces()).await.ok()
    }

    /// The team space `id` as the local API lists it.
    async fn space(&self, id: SpaceId) -> Option<SpaceInfo> {
        self.spaces().await?.spaces.into_iter().find(|s| s.space == id)
    }

    async fn accept(&self, id: SpaceId) {
        self.api(move |c| c.accept_space(id)).await.unwrap();
        // The agent looks at its list again before the call answers, but a look that failed under load leaves the old list.
        eventually("the accepted space is listed as active", || async { self.space(id).await.is_some_and(|s| s.state == "active") }).await;
    }

    /// Turns the sync of a team space on and returns where it syncs.
    async fn sync_on(&self, id: SpaceId) -> PathBuf {
        let info = self.api(move |c| c.set_space_sync(id, &SpaceSyncRequest { on: true, folder: None, adopt: false })).await.unwrap();
        assert_eq!(info.sync, SpaceSync::On, "{info:?}");
        PathBuf::from(info.folder.unwrap())
    }

    async fn status(&self) -> craft_cloud_app::Status {
        self.api(|c| c.status()).await.unwrap()
    }

    /// Turns the sync of a team space on, in `folder`.
    async fn sync_on_in(&self, id: SpaceId, folder: &Path) -> SpaceInfo {
        let folder = folder.display().to_string();
        let info = self.api(move |c| c.set_space_sync(id, &SpaceSyncRequest { on: true, folder: Some(folder), adopt: false })).await.unwrap();
        assert_eq!(info.sync, SpaceSync::On, "{info:?}");
        info
    }

    /// Turns the sync of a team space off.
    async fn sync_off(&self, id: SpaceId) {
        let info = self.api(move |c| c.set_space_sync(id, &SpaceSyncRequest { on: false, folder: None, adopt: false })).await.unwrap();
        assert_eq!(info.sync, SpaceSync::Off, "{info:?}");
    }

    /// The saved state file of a team space, as JSON, if there is one.
    fn space_state(&self, id: SpaceId) -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(self.data.path().join("spaces").join(id.to_string()).join("state.json")).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// The files directly in `folder`, by name (hidden ones, the agent's own, left out).
fn names(folder: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

/// Waits until `cond` holds, failing after [`PATIENCE`].
async fn eventually<F: std::future::Future<Output = bool>>(what: &str, mut cond: impl FnMut() -> F) {
    let deadline = Instant::now() + PATIENCE;
    while !cond().await {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn write(folder: &Path, rel: &str, data: &str) {
    let path = folder.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

fn read(folder: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(folder.join(rel)).ok()
}

async fn has_file(folder: PathBuf, rel: &'static str, data: &'static str) -> bool {
    read(&folder, rel).as_deref() == Some(data)
}

/// Ana and ben, each with a computer, both running, and a team space `Studio` that ben was invited to as `role`.
struct Team {
    world: World,
    ana: Person,
    ben: Person,
    space: SpaceId,
}

async fn team(role: SpaceRole) -> Team {
    let world = world().await;
    let (mut ana, mut ben) = (Person::signed_in(&world.url, "ana").await, Person::signed_in(&world.url, "ben").await);
    let space = world.space("Studio").await;
    world.invite_ben(space, role).await;
    ana.run();
    ben.run();
    Team { world, ana, ben, space }
}

/// As [`team`], with ben's computer reaching the server through a front that can misbehave.
async fn team_via_front(role: SpaceRole, behaviour: Front) -> (Team, Arc<Front>, tokio::task::JoinHandle<()>) {
    let world = world().await;
    let (url, front, task) = front(&world.url, behaviour).await;
    let (mut ana, mut ben) = (Person::signed_in(&world.url, "ana").await, Person::signed_in(&url, "ben").await);
    let space = world.space("Studio").await;
    world.invite_ben(space, role).await;
    ana.run();
    ben.run();
    (Team { world, ana, ben, space }, front, task)
}

/// Ben accepts, and both turn the space's sync on. Returns ana's and ben's team folders.
async fn both_syncing(t: &Team) -> (PathBuf, PathBuf) {
    let space = t.space;
    eventually("the invitation is listed", || async move { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(space).await;
    // Ana's agent may not have looked at the server's list yet (the space was made just before it started).
    eventually("ana's agent lists the space", || async move { t.ana.space(space).await.is_some_and(|s| s.state == "active") }).await;
    let (ana_folder, ben_folder) = (t.ana.sync_on(space).await, t.ben.sync_on(space).await);
    (ana_folder, ben_folder)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invitation_is_accepted_in_the_local_api_and_the_team_folder_syncs_both_ways() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;

    // The invitation is listed, with no folder; the personal space comes first.
    eventually("the invitation is listed", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    let list = t.ben.spaces().await.unwrap();
    assert_eq!(list.spaces.first().map(|s| s.kind.as_str()), Some("personal"), "{list:?}");
    let invited = t.ben.space(space).await.unwrap();
    assert_eq!((invited.sync, invited.folder.clone(), invited.name.as_deref()), (SpaceSync::Off, None, Some("Studio")), "{invited:?}");

    // Accepted: it is an active member, syncing off, with its folder chosen beside the personal one but not made yet.
    t.ben.accept(space).await;
    let accepted = t.ben.space(space).await.unwrap();
    assert_eq!((accepted.state.as_str(), accepted.sync, accepted.role), ("active", SpaceSync::Off, SpaceRole::Editor), "{accepted:?}");
    let planned = PathBuf::from(accepted.folder.unwrap());
    assert_eq!(planned, t.ben.shared().join("Studio"));
    assert!(!planned.exists(), "nothing is made until the person turns the sync on");
    assert!(t.ben.status().await.spaces.is_empty(), "a space that doesn't sync has no status entry");

    // Turned on: the folder appears with the root marker, and the team space syncs both ways.
    eventually("ana's agent lists the space", || async { t.ana.space(space).await.is_some_and(|s| s.state == "active") }).await;
    let (ana_folder, ben_folder) = (t.ana.sync_on(space).await, t.ben.sync_on(space).await);
    assert_eq!(ben_folder, planned);
    assert!(ben_folder.join(".craft-cloud/root.json").is_file());
    write(&ana_folder, "brief.txt", "from ana");
    eventually("ana's file reaches ben", || has_file(ben_folder.clone(), "brief.txt", "from ana")).await;
    write(&ben_folder, "Shots/010.psd", "layers by ben");
    write(&ben_folder, "brief.txt", "edited by ben");
    eventually("ben's work reaches ana", || async {
        read(&ana_folder, "Shots/010.psd").as_deref() == Some("layers by ben") && read(&ana_folder, "brief.txt").as_deref() == Some("edited by ben")
    })
    .await;

    // The status names the space with its state; the personal folder is not mixed in.
    eventually("the status says idle for the space", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == space && s.run_state == SpaceRun::Idle && s.last_sync_ms.is_some() && !s.read_only)
    })
    .await;
    assert!(!t.ben.personal.join("Studio").exists() && !t.ben.personal.join("brief.txt").exists());
    assert_eq!(std::fs::read_dir(&t.ben.personal).unwrap().filter_map(Result::ok).filter(|e| e.file_name() != ".craft-cloud").count(), 0);
    // Its own state, apart from the personal one.
    assert!(t.ben.data.path().join("spaces").join(space.to_string()).join("state.json").is_file());

    let (ana, mut ben) = (t.ana, t.ben);
    let (exit, out) = ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    // Started again: the team folder is still synced, without turning it on again.
    ben.run();
    write(&ana_folder, "after.txt", "after the restart");
    eventually("the restarted agent still syncs the team folder", || has_file(ben_folder.clone(), "after.txt", "after the restart")).await;
    let _ = ana;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declining_an_invitation_removes_it() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    eventually("the invitation is listed", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.api(move |c| c.decline_space(space)).await.unwrap();
    assert!(t.ben.space(space).await.is_none(), "declined: no longer listed");
    // A second answer to the same invitation is the server's `not_found`, passed through (not an old agent's).
    let again = t.ben.api(move |c| c.decline_space(space)).await.unwrap_err();
    assert!(matches!(&again, craft_cloud_app::Error::Refused { code, .. } if code == "not_found"), "{again:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_role_lowered_to_viewer_makes_the_folder_read_only() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    write(&ana_folder, "base.txt", "base");
    eventually("ben has the file", || has_file(ben_folder.clone(), "base.txt", "base")).await;

    t.world.meta.change_member_role(&SpaceActor::Cli, space, t.world.ben.id, SpaceRole::Viewer, 1).await.unwrap();
    eventually("the local API says read-only", || async { t.ben.space(space).await.is_some_and(|s| s.role == SpaceRole::Viewer && s.read_only) })
        .await;
    eventually("the status says read-only", || async { t.ben.status().await.spaces.iter().any(|s| s.space == space && s.read_only) }).await;

    // A viewer's edit and new file stay on the viewer's disk; the server's changes still arrive.
    write(&ben_folder, "base.txt", "edited by the viewer");
    write(&ben_folder, "mine.txt", "viewer's own");
    write(&ana_folder, "from-ana.txt", "still arrives");
    eventually("ana's later file reaches the viewer", || has_file(ben_folder.clone(), "from-ana.txt", "still arrives")).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(read(&ana_folder, "base.txt").as_deref(), Some("base"), "the viewer's edit never went up");
    assert_eq!(read(&ana_folder, "mine.txt"), None, "nor the viewer's new file");
    assert_eq!(read(&ben_folder, "base.txt").as_deref(), Some("edited by the viewer"), "and is not undone here");
    assert_eq!(read(&ben_folder, "mine.txt").as_deref(), Some("viewer's own"));

    // Raised again: what was only here goes up.
    t.world.meta.change_member_role(&SpaceActor::Cli, space, t.world.ben.id, SpaceRole::Editor, 2).await.unwrap();
    eventually("ben's files go up once he may write", || async {
        read(&ana_folder, "mine.txt").as_deref() == Some("viewer's own") && read(&ana_folder, "base.txt").as_deref() == Some("edited by the viewer")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_member_keeps_the_local_copy_which_stops_syncing_and_is_reported() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    write(&ana_folder, "plans/a.txt", "plan a");
    write(&ben_folder, "notes.txt", "ben's notes");
    eventually("both have everything", || async { read(&ben_folder, "plans/a.txt").is_some() && read(&ana_folder, "notes.txt").is_some() }).await;

    t.world.meta.remove_member(&SpaceActor::Cli, space, t.world.ben.id, 1).await.unwrap();
    eventually("the space reports no access", || async {
        t.ben.status().await.spaces.iter().any(|s| {
            s.space == space && matches!(&s.run_state, SpaceRun::NoAccess { message } if message.contains("Studio") && message.contains("kept"))
        })
    })
    .await;
    let info = t.ben.space(space).await.unwrap();
    assert_eq!(info.sync, SpaceSync::Unavailable, "{info:?}");
    assert!(matches!(info.run_state, Some(SpaceRun::NoAccess { .. })), "{info:?}");

    // Nothing was deleted here, the folder and its marker stay, and nothing moves either way any more.
    assert_eq!(read(&ben_folder, "plans/a.txt").as_deref(), Some("plan a"));
    assert_eq!(read(&ben_folder, "notes.txt").as_deref(), Some("ben's notes"));
    assert!(ben_folder.join(".craft-cloud/root.json").is_file());
    write(&ana_folder, "later.txt", "after the removal");
    write(&ben_folder, "local-only.txt", "written after the removal");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(read(&ben_folder, "later.txt"), None, "no further changes arrive");
    assert_eq!(read(&ana_folder, "local-only.txt"), None, "and none leave");
    assert_eq!(read(&ana_folder, "notes.txt").as_deref(), Some("ben's notes"), "the server still has ben's earlier work");

    // The personal space is unaffected, and a re-invited member syncs again without losing the local files.
    assert!(t.ben.status().await.signed_in);
    t.world.invite_ben(space, SpaceRole::Editor).await;
    eventually("the invitation is listed again", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(space).await;
    eventually("the sync goes on and catches up", || async {
        read(&ben_folder, "later.txt").as_deref() == Some("after the removal")
            && read(&ana_folder, "local-only.txt").as_deref() == Some("written after the removal")
    })
    .await;
    assert_eq!(read(&ben_folder, "notes.txt").as_deref(), Some("ben's notes"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renamed_space_keeps_its_folder_and_shows_the_new_name() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    write(&ana_folder, "one.txt", "1");
    eventually("synced", || has_file(ben_folder.clone(), "one.txt", "1")).await;

    let owner = SpaceActor::User { user: t.world.ana.id, device: None };
    t.world.meta.rename_team_space(&owner, space, &SpaceName::new("Studio Two").unwrap(), 1).await.unwrap();
    eventually("the new name is shown", || async { t.ben.space(space).await.is_some_and(|s| s.name.as_deref() == Some("Studio Two")) }).await;
    let info = t.ben.space(space).await.unwrap();
    assert_eq!(info.folder.as_deref().map(PathBuf::from), Some(ben_folder.clone()), "the folder keeps its name: {info:?}");
    assert!(ben_folder.is_dir() && !t.ben.shared().join("Studio Two").exists());
    write(&ana_folder, "two.txt", "2");
    eventually("the sync goes on in the same folder", || has_file(ben_folder.clone(), "two.txt", "2")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spaces_whose_names_make_the_same_folder_get_numbered_and_a_folder_in_the_way_is_not_taken() {
    let world = world().await;
    let first = world.space("Plans: 2026").await;
    let second = world.space("Plans? 2026").await;
    let third = world.space("Studio").await;
    for space in [first, second, third] {
        world.invite_ben(space, SpaceRole::Editor).await;
    }
    let mut ben = Person::signed_in(&world.url, "ben").await;
    // A folder of that name is already there, with files in it.
    write(&ben.shared().join("Studio"), "mine.txt", "not a team space's");
    ben.run();
    for space in [first, second, third] {
        let ben = &ben;
        eventually("listed", || async move { ben.space(space).await.is_some() }).await;
        ben.accept(space).await;
    }
    let folder = |space: SpaceId| {
        let ben = &ben;
        async move { ben.space(space).await.unwrap().folder.map(PathBuf::from).unwrap() }
    };
    let (a, b, c) = (folder(first).await, folder(second).await, folder(third).await);
    let mut names = [a.clone(), b.clone()].map(|p| p.file_name().unwrap().to_string_lossy().into_owned());
    names.sort();
    assert_eq!(names, ["Plans_ 2026", "Plans_ 2026 (2)"], "the unsafe characters are replaced, and the clash is numbered");
    assert_eq!(c.file_name().unwrap().to_string_lossy(), "Studio (2)", "a folder already there is not taken");
    // Turning on a folder that is free works; the occupied one is never adopted without consent.
    ben.sync_on(first).await;
    let in_the_way = ben.shared().join("Studio").display().to_string();
    let occupied = ben.api(move |cl| cl.set_space_sync(third, &SpaceSyncRequest { on: true, folder: Some(in_the_way), adopt: false })).await;
    assert!(matches!(&occupied, Err(craft_cloud_app::Error::Refused { code, .. }) if code == "folder_has_files"), "{occupied:?}");
    assert_eq!(read(&ben.shared().join("Studio"), "mine.txt").as_deref(), Some("not a team space's"));
    // A folder inside the personal folder is refused.
    let inside = ben.personal.join("Team").display().to_string();
    let refused = ben.api(move |cl| cl.set_space_sync(third, &SpaceSyncRequest { on: true, folder: Some(inside), adopt: false })).await;
    assert!(matches!(&refused, Err(craft_cloud_app::Error::Refused { code, .. }) if code == "invalid"), "{refused:?}");
}

// ---------------------------------------------------------------------------------------------------------------------
// A server in front of the real one that can misbehave like an old one

/// What the front does to the requests it forwards.
#[derive(Default)]
struct Front {
    /// Answers `/api/v1/info` with protocol 1 and `/api/v1/spaces` with `404`, as a server from before team spaces does.
    old: bool,
    /// Leaves the `craft-space` header out of every answer, as a server that ignores the header does.
    no_echo: bool,
    /// Every path asked, and whether it carried `craft-space`.
    seen: Mutex<Vec<(String, bool)>>,
    /// Answers `/api/v1/spaces` with `404` this many more times, then goes on as usual (a proxy during a restart).
    spaces_404: Mutex<u32>,
    /// Does not answer while set: the connection stays open and silent, and the request goes on once it is cleared.
    stall: AtomicBool,
    /// Waits this long (milliseconds) before forwarding each request.
    delay_ms: AtomicU64,
    /// The long-polls of a team space asked so far.
    polls: AtomicUsize,
    /// Answers the live upgrade of a team space (`/api/v1/live` with `craft-space`) with `404`, as a server whose live service
    /// does not serve team spaces does.
    live_404: AtomicBool,
}

async fn front(upstream: &str, behaviour: Front) -> (String, Arc<Front>, tokio::task::JoinHandle<()>) {
    let behaviour = Arc::new(behaviour);
    let state = (upstream.to_owned(), behaviour.clone(), reqwest::Client::new());
    let app = axum::Router::new().fallback(forward).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, behaviour, task)
}

type FrontState = (String, Arc<Front>, reqwest::Client);

async fn forward(State((upstream, front, client)): State<FrontState>, request: Request<Body>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    let space_header = HeaderName::from_static("craft-space");
    let named = parts.headers.contains_key(&space_header);
    front.seen.lock().unwrap().push((path.clone(), named));
    if named && parts.uri.query().is_some_and(|q| q.contains("wait=")) {
        front.polls.fetch_add(1, Ordering::SeqCst);
    }
    if named && path == "/api/v1/live" && front.live_404.load(Ordering::SeqCst) {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("{\"error\":\"not_found\",\"message\":\"no such space\"}"))
            .unwrap();
    }
    // A request that arrives while `stall` is set (or is held by it) waits until it is cleared.
    while front.stall.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let delay = front.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let refuse = path == "/api/v1/spaces" && {
        let mut left = front.spaces_404.lock().unwrap();
        let refuse = *left > 0;
        *left = left.saturating_sub(1);
        refuse
    };
    if refuse {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("{\"error\":\"not_found\",\"message\":\"no such route\"}"))
            .unwrap();
    }
    if front.old && path == "/api/v1/spaces" {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("{\"error\":\"not_found\",\"message\":\"no such route\"}"))
            .unwrap();
    }
    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap();
    let target = format!("{upstream}{}", parts.uri.path_and_query().map_or("", |p| p.as_str()));
    let mut call = client.request(parts.method.clone(), target).body(bytes);
    for (name, value) in &parts.headers {
        if name != "host" && name != "content-length" {
            call = call.header(name, value);
        }
    }
    let answer = call.send().await.unwrap();
    let status = answer.status();
    let headers = answer.headers().clone();
    let mut body = answer.bytes().await.unwrap().to_vec();
    if front.old && path == "/api/v1/info" {
        let mut info: serde_json::Value = serde_json::from_slice(&body).unwrap();
        info["protocol_max"] = 1.into();
        body = serde_json::to_vec(&info).unwrap();
    }
    let mut response = Response::builder().status(status);
    for (name, value) in &headers {
        if name == "content-length" || name == "transfer-encoding" || (front.no_echo && name == "craft-space") {
            continue;
        }
        response = response.header(name, value);
    }
    response.body(Body::from(body)).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_that_answers_without_the_echo_is_refused() {
    let world = world().await;
    let space = world.space("Studio").await;
    world.invite_ben(space, SpaceRole::Editor).await;
    world.meta.accept_invitation(world.ben.id, None, space, 1).await.unwrap();
    let ben = Person::signed_in(&world.url, "ben").await;
    let account = ben.data.load_account().unwrap().unwrap();
    let (url, front, _task) = front(&world.url, Front { no_echo: true, ..Front::default() }).await;
    let transport = HttpTransport::new(ServerUrl::parse(&url).unwrap(), &account.token).unwrap();

    // Without the space, an answer needs no echo (it is the personal space, as ever).
    assert!(transport.space().await.is_ok());
    // Bound to a team space, the same answer is refused: it could be about the personal space.
    let bound = transport.clone().bound_to(space).unwrap();
    let refused = bound.space().await.unwrap_err();
    assert!(refused.to_string().contains("too old for team spaces"), "{refused}");
    assert!(bound.tree(None).await.is_err_and(|e| e.to_string().contains("too old for team spaces")));
    assert!(front.seen.lock().unwrap().iter().any(|(path, named)| path.ends_with("/space") && *named));

    // The real server's answers carry the echo and are accepted; so is a refusal of the layer (`404`).
    let direct = HttpTransport::new(ServerUrl::parse(&world.url).unwrap(), &account.token).unwrap();
    assert_eq!(direct.clone().bound_to(space).unwrap().space().await.unwrap().space, space);
    let other = direct.bound_to(SpaceId::generate()).unwrap().space().await.unwrap_err();
    assert_eq!(other, craft_cloud_sync::TransportError::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_server_shows_no_team_spaces_and_is_never_sent_the_space_header() {
    let world = world().await;
    let space = world.space("Studio").await;
    world.invite_ben(space, SpaceRole::Editor).await;
    let (url, front, _task) = front(&world.url, Front { old: true, ..Front::default() }).await;
    let mut ben = Person::signed_in(&url, "ben").await;
    ben.run();
    ben.ready().await;
    write(&ben.personal, "mine.txt", "personal as ever");
    eventually("the personal folder syncs", || async { ben.status().await.last_sync_ms.is_some() }).await;
    // Discovery runs more than once in this time; it asks about the version only.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let list = ben.spaces().await.unwrap();
    assert_eq!(list.spaces.len(), 1, "personal only: {list:?}");
    assert_eq!(list.spaces[0].kind, "personal");
    assert!(ben.status().await.spaces.is_empty());
    {
        let seen = front.seen.lock().unwrap();
        assert!(seen.iter().all(|(_, named)| !named), "no request names a space: {seen:?}");
        assert!(seen.iter().all(|(path, _)| path != "/api/v1/spaces"), "/spaces is not even asked: {seen:?}");
        let infos = seen.iter().filter(|(path, _)| path == "/api/v1/info").count();
        assert!(infos < 40, "no loop of version checks: {infos}");
    }
    // Turning a team space on is refused with the reason.
    let refused = ben.api(move |c| c.set_space_sync(space, &SpaceSyncRequest { on: true, folder: None, adopt: false })).await;
    assert!(refused.is_err(), "{refused:?}");
    let (exit, out) = ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phase_1_data_folder_starts_unchanged() {
    let world = world().await;
    let mut ben = Person::signed_in(&world.url, "ben").await;
    let account_file = ben.data.path().join("account.json");
    let before = std::fs::read(&account_file).unwrap();
    assert!(!String::from_utf8_lossy(&before).contains("spaces"), "an account from before team spaces has no such field");
    write(&ben.personal, "doc.txt", "as before");
    ben.run();
    ben.ready().await;
    eventually("a pass finished", || async { ben.status().await.last_sync_ms.is_some() }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (exit, out) = ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert_eq!(std::fs::read(&account_file).unwrap(), before, "account.json is byte for byte as it was");
    assert!(!ben.data.path().join("spaces").exists(), "no folder for team spaces' state without a team space");
    assert!(ben.data.path().join("state.json").is_file());
    let _: SocketAddr = "127.0.0.1:1".parse().unwrap();
}

// ---------------------------------------------------------------------------------------------------------------------
// The review round of P1-5

/// H3: a state made for one folder is never used for another. A space that synced to A, then to an empty B (which also
/// received what was added on the server later), turned back to A: A has the marker of the space, but B's state must not
/// decide what A is missing. The files B got and A never had are not deleted on the server. Signing out forgets the states.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_state_made_for_one_folder_is_not_used_for_another() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, a) = both_syncing(&t).await;
    for i in 0..6 {
        write(&ana_folder, &format!("f{i}.txt"), &format!("content {i}"));
    }
    eventually("ben's first folder has the files", || has_file(a.clone(), "f5.txt", "content 5")).await;

    // Off, and on in an empty folder B: it fills with everything, and with what is added on the server later.
    t.ben.sync_off(space).await;
    let b = t.ben.dir.path().join("Elsewhere");
    t.ben.sync_on_in(space, &b).await;
    eventually("the second folder has the files", || has_file(b.clone(), "f5.txt", "content 5")).await;
    write(&ana_folder, "later.txt", "added later");
    eventually("the second folder has what came later", || has_file(b.clone(), "later.txt", "added later")).await;
    assert_eq!(read(&a, "later.txt"), None, "the first folder is not synced meanwhile");

    // Off, and on in A again: A has the space's marker, and B's state is not used for it.
    t.ben.sync_off(space).await;
    t.ben.sync_on_in(space, &a).await;
    eventually("A catches up", || has_file(a.clone(), "later.txt", "added later")).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(names(&ana_folder).len(), 7, "nothing was deleted on the server: {:?}", names(&ana_folder));
    assert_eq!(read(&ana_folder, "later.txt").as_deref(), Some("added later"));
    assert_eq!(names(&a).len(), 7, "nor in A: {:?}", names(&a));
    assert_eq!(names(&b).len(), 7, "nor in B: {:?}", names(&b));
    let state = t.ben.space_state(space).unwrap();
    assert_eq!(state["folder"].as_str().map(PathBuf::from), std::fs::canonicalize(&a).ok(), "the state names the folder it is for");

    // Signing out forgets the states of the team spaces, as it does the personal one; the folders and files stay.
    let (_ana, mut ben) = (t.ana, t.ben);
    let (exit, out) = ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    cli::logout(&ben.data, true, &mut Vec::new()).await.unwrap();
    assert!(ben.space_state(space).is_none(), "the team space's state is gone");
    assert!(!ben.data.path().join("state.json").exists());
    assert_eq!(names(&a).len(), 7);
    assert_eq!(names(&b).len(), 7);
    assert!(a.join(".craft-cloud/root.json").is_file() && b.join(".craft-cloud/root.json").is_file(), "the markers stay");
}

/// H1: moving a space that is syncing to another folder stops syncing the old one at once and starts the new folder from
/// a fresh state: nothing the old folder held is taken for a deletion in the new one, before or after a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moving_a_syncing_space_to_another_folder_syncs_the_new_folder_and_deletes_nothing() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, a) = both_syncing(&t).await;
    for i in 0..8 {
        write(&ana_folder, &format!("doc{i}.txt"), &format!("document {i}"));
    }
    eventually("ben has the eight files", || has_file(a.clone(), "doc7.txt", "document 7")).await;

    // The person moves the space to an empty folder while it syncs.
    let b = t.ben.dir.path().join("Moved");
    let info = t.ben.sync_on_in(space, &b).await;
    assert_eq!(info.folder.as_deref().map(PathBuf::from), Some(b.clone()));
    eventually("the new folder fills", || async { names(&b).len() == 8 }).await;

    // From now on the new folder is the one that syncs; the old one is left as it is.
    write(&a, "left-behind.txt", "written in the old folder");
    write(&ana_folder, "new.txt", "written by ana");
    eventually("the new folder gets ana's later file", || has_file(b.clone(), "new.txt", "written by ana")).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(read(&ana_folder, "left-behind.txt"), None, "the old folder no longer goes up");
    assert_eq!(read(&a, "new.txt"), None, "and gets nothing");

    // A restart opens the new folder with the state made for it.
    let (_ana, mut ben) = (t.ana, t.ben);
    let (exit, out) = ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    ben.run();
    write(&ana_folder, "after.txt", "after the restart");
    eventually("the restarted agent syncs the new folder", || has_file(b.clone(), "after.txt", "after the restart")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(names(&ana_folder).len(), 10, "nothing was deleted on the server: {:?}", names(&ana_folder));
    assert_eq!(names(&b).len(), 10, "{:?}", names(&b));
}

/// H2: turning a space off waits for its running pass to end, so the pass cannot write the old folder's state after the
/// space is set up for another folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_a_space_off_waits_for_its_pass_and_the_old_state_is_not_written_after() {
    let (t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let space = t.space;
    let (ana_folder, a) = both_syncing(&t).await;
    for i in 0..4 {
        write(&ana_folder, &format!("f{i}.txt"), "content");
    }
    eventually("ben has the files", || async { names(&a).len() == 4 }).await;

    // Every request takes a while now, so a pass lasts several seconds.
    front.delay_ms.store(400, Ordering::SeqCst);
    write(&a, "busy.txt", "ben's new file");
    eventually("a pass of the space is running", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == space && s.run_state == SpaceRun::Syncing)
    })
    .await;
    t.ben.sync_off(space).await;
    // Off forgets the state, and the call is answered only when the running pass has ended: the pass cannot write the state
    // after it (a call answered early would leave a state behind, or see one written afterwards).
    assert!(t.ben.space_state(space).is_none(), "the state is gone when the call is answered");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(t.ben.space_state(space).is_none(), "no pass of the old folder writes its state after the space is off");

    // On in another folder: the state it starts from is not the old folder's.
    front.delay_ms.store(0, Ordering::SeqCst);
    let b = t.ben.dir.path().join("Elsewhere");
    t.ben.sync_on_in(space, &b).await;
    eventually("the new folder fills", || async { names(&b).len() >= 4 }).await;
    eventually("the state is the new folder's", || async {
        t.ben.space_state(space).is_some_and(|s| s["folder"].as_str().map(PathBuf::from) == std::fs::canonicalize(&b).ok())
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    for i in 0..4 {
        assert_eq!(read(&ana_folder, &format!("f{i}.txt")).as_deref(), Some("content"), "nothing was deleted on the server");
        assert_eq!(read(&b, &format!("f{i}.txt")).as_deref(), Some("content"), "nor in the new folder");
    }
}

/// H4: access comes back after it was lost. Meanwhile ben deleted a file here and ana deleted one on the server, both
/// edited a third, and ben made a new one. Nothing ben has is deleted or replaced, his delete is not sent, hers does not
/// remove his file, the two edits are both kept, and his new file goes up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn regained_access_merges_and_never_deletes_for_what_happened_meanwhile() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    for (name, content) in
        [("keep.txt", "kept"), ("ben-deletes.txt", "ben removes this"), ("ana-deletes.txt", "ana removes this"), ("both.txt", "base")]
    {
        write(&ana_folder, name, content);
    }
    eventually("ben has everything", || async { names(&ben_folder).len() == 4 }).await;

    t.world.meta.remove_member(&SpaceActor::Cli, space, t.world.ben.id, 1).await.unwrap();
    eventually("the space reports no access", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == space && matches!(s.run_state, SpaceRun::NoAccess { .. }))
    })
    .await;
    eventually("its saved state is dropped", || async { t.ben.space_state(space).is_none() }).await;

    // While access is lost: deletes on both sides, an edit on both, a new file here.
    std::fs::remove_file(ben_folder.join("ben-deletes.txt")).unwrap();
    std::fs::remove_file(ana_folder.join("ana-deletes.txt")).unwrap();
    write(&ana_folder, "both.txt", "ana's edit");
    write(&ben_folder, "both.txt", "ben's edit");
    write(&ben_folder, "ben-new.txt", "ben's new file");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(read(&ana_folder, "ben-deletes.txt").as_deref(), Some("ben removes this"), "ben's delete did not go up");

    // Access is given again.
    t.world.invite_ben(space, SpaceRole::Editor).await;
    eventually("the invitation is listed again", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(space).await;
    eventually("the folders are merged", || async {
        read(&ana_folder, "ben-new.txt").as_deref() == Some("ben's new file")
            && read(&ben_folder, "ben-deletes.txt").as_deref() == Some("ben removes this")
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    // Ben's own deleted file came back from the server; ana's delete did not remove ben's file.
    assert_eq!(read(&ana_folder, "ben-deletes.txt").as_deref(), Some("ben removes this"), "still on the server");
    assert_eq!(read(&ben_folder, "ana-deletes.txt").as_deref(), Some("ana removes this"), "ana's delete did not remove ben's file");
    assert_eq!(read(&ana_folder, "keep.txt").as_deref(), Some("kept"));
    // The two edits are both kept: one under the name, one as a conflict copy.
    let both: Vec<String> = names(&ben_folder).into_iter().filter(|n| n.contains("both")).collect();
    let contents: Vec<String> = both.iter().filter_map(|n| read(&ben_folder, n)).collect();
    assert!(contents.contains(&"ana's edit".to_owned()) && contents.contains(&"ben's edit".to_owned()), "{both:?} {contents:?}");
}

/// The deletions a team space holds back (a mass delete) are approved from the command line, with the space's name, and
/// from the local API; the running agent then carries them out. (Ana's guard holds her deletes back; once they are on the
/// server, ben's guard holds them back on his side, and he approves too.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_deletions_of_a_team_space_are_approved_from_the_command_line_and_the_local_api() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    for i in 0..30 {
        write(&ana_folder, &format!("f{i:02}.txt"), "content");
    }
    eventually("ben has the thirty files", || async { names(&ben_folder).len() == 30 }).await;
    let waiting_set = |person: &Person| {
        person.data.space_dir(space).load_state().ok().and_then(|s| s.last_pass).and_then(|l| l.summary).and_then(|s| s.pending_set())
    };
    let approve = |person: &Person, args: Vec<String>| {
        let data = person.data.path().to_owned();
        async move {
            let mut argv = vec!["craft-cloud-agent".to_owned(), "--data-dir".to_owned(), data.display().to_string(), "sync".to_owned()];
            argv.extend(args);
            let cli = <cli::Cli as clap::Parser>::try_parse_from(argv).unwrap();
            let mut out = Vec::new();
            let exit = cli::run(cli, &mut std::io::empty(), &mut out).await;
            (exit, String::from_utf8_lossy(&out).into_owned())
        }
    };
    let args = |list: &[&str]| list.iter().map(|a| (*a).to_owned()).collect::<Vec<String>>();

    // Ana deletes twenty-five: her guard holds them back, and ben keeps his files.
    for i in 0..25 {
        std::fs::remove_file(ana_folder.join(format!("f{i:02}.txt"))).unwrap();
    }
    eventually("ana's agent holds the deletions", || async { waiting_set(&t.ana).is_some() }).await;
    let set = waiting_set(&t.ana).unwrap();
    assert_eq!(names(&ben_folder).len(), 30);

    // A space that isn't there is refused.
    let (unknown, _) = approve(&t.ana, args(&["--approve-deletes", "--space", "Nowhere"])).await;
    assert!(matches!(unknown, Err(AgentError::Usage(_))), "{unknown:?}");
    // The bare flag lists first (nothing showed the set before), and approves nothing.
    let (first, out) = approve(&t.ana, args(&["--approve-deletes", "--space", "studio"])).await;
    assert_eq!(first.unwrap(), Exit::NeedsApproval, "{out}");
    assert!(out.contains("nothing approved"), "{out}");
    assert!(!t.ana.data.space_dir(space).path().join("approval.json").exists());
    // A set that isn't the one waiting approves nothing either.
    let (wrong, out) = approve(&t.ana, args(&["--approve-deletes=0000000000", "--space", "Studio"])).await;
    assert_eq!(wrong.unwrap(), Exit::NeedsApproval, "{out}");
    assert_eq!(names(&ben_folder).len(), 30);
    // The set itself, with the space named by its ID.
    let (given, out) = approve(&t.ana, vec![format!("--approve-deletes={set}"), "--space".into(), space.to_string()]).await;
    assert_eq!(given.unwrap(), Exit::Done, "{out}");
    assert!(out.contains("will carry out these deletions"), "{out}");
    // On the server now; ben's guard holds them, and he approves them his way.
    eventually("ben's agent holds the deletions", || async { waiting_set(&t.ben).is_some() }).await;
    let ben_set = waiting_set(&t.ben).unwrap();
    assert_eq!(names(&ben_folder).len(), 30);
    let (given, out) = approve(&t.ben, vec![format!("--approve-deletes={ben_set}"), "--space".into(), "Studio".into()]).await;
    assert_eq!(given.unwrap(), Exit::Done, "{out}");
    eventually("the approved deletions reach ben", || async { names(&ben_folder).len() == 5 }).await;

    // The second round goes through the local API: twenty new files, eighteen of them deleted.
    for i in 0..20 {
        write(&ana_folder, &format!("g{i:02}.txt"), "more");
    }
    eventually("ben has them", || async { names(&ben_folder).len() == 25 }).await;
    for i in 0..18 {
        std::fs::remove_file(ana_folder.join(format!("g{i:02}.txt"))).unwrap();
    }
    eventually("ana's agent holds the next deletions", || async { waiting_set(&t.ana).is_some() }).await;
    let set = waiting_set(&t.ana).unwrap();
    let wrong = t.ana.api(move |c| c.approve_space_deletes(space, Some("0000000000"))).await.unwrap();
    assert!(!wrong.approved && wrong.message.contains("nothing approved"), "{wrong:?}");
    let answer = t.ana.api(move |c| c.approve_space_deletes(space, Some(&set))).await.unwrap();
    assert!(answer.approved, "{answer:?}");
    eventually("ben's agent holds them", || async { waiting_set(&t.ben).is_some() }).await;
    let ben_set = waiting_set(&t.ben).unwrap();
    assert_eq!(names(&ben_folder).len(), 25);
    let answer = t.ben.api(move |c| c.approve_space_deletes(space, Some(&ben_set))).await.unwrap();
    assert!(answer.approved, "{answer:?}");
    eventually("the deletions approved through the local API are carried out", || async { names(&ben_folder).len() == 7 }).await;
    // A space that doesn't sync here is not found.
    let missing = t.ana.api(|c| c.approve_space_deletes(SpaceId::generate(), None)).await.unwrap_err();
    assert!(matches!(&missing, craft_cloud_app::Error::Refused { code, .. } if code == "not_found"), "{missing:?}");
}

/// M1: a second stop ends the loop at once even when the server is silent and a pass of a team space is queued behind
/// another pass: no queued team pass starts, and the thread of the team passes is not waited for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_stop_does_not_wait_for_the_team_spaces_with_a_silent_server() {
    let (mut t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let (_, ben_folder) = both_syncing(&t).await;
    write(&ben_folder, "first.txt", "first");
    eventually("the first file went up", || async { t.ben.status().await.spaces.iter().any(|s| s.last_sync_ms.is_some()) }).await;

    front.stall.store(true, Ordering::SeqCst);
    write(&ben_folder, "stuck.txt", "never answered");
    write(&t.ben.personal, "stuck-too.txt", "never answered");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut running = t.ben.running.take().unwrap();
    let _ = running.stop.send(());
    let _ = running.stop.send(());
    let thread = running.thread.take().unwrap();
    let started = Instant::now();
    let ended = tokio::time::timeout(Duration::from_secs(8), tokio::task::spawn_blocking(move || thread.join().unwrap())).await;
    front.stall.store(false, Ordering::SeqCst);
    let (exit, out) = ended.expect("the loop ends within seconds of the second stop, though the server is silent").unwrap();
    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(exit.unwrap(), Exit::Failed, "{out}");
}

/// M2: a member of an archived space keeps read access: the space stays listed (marked archived), syncs read-only and is not
/// reported as lost; nothing of the member's goes up until it is unarchived.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_of_an_archived_space_keeps_a_read_only_copy() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    write(&ana_folder, "plan.txt", "the plan");
    eventually("ben has the plan", || has_file(ben_folder.clone(), "plan.txt", "the plan")).await;

    let owner = SpaceActor::User { user: t.world.ana.id, device: None };
    t.world.meta.set_space_archived(&owner, space, true, 1).await.unwrap();
    eventually("ben's space is read-only and archived", || async {
        t.ben.space(space).await.is_some_and(|s| s.archived && s.read_only && s.sync == SpaceSync::On)
    })
    .await;
    write(&ben_folder, "mine.txt", "written while archived");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let info = t.ben.space(space).await.unwrap();
    assert!(!matches!(info.run_state, Some(SpaceRun::NoAccess { .. })), "not reported as lost: {info:?}");
    assert_eq!((info.sync, info.role), (SpaceSync::On, SpaceRole::Editor), "{info:?}");
    assert_eq!(read(&ana_folder, "mine.txt"), None, "nothing goes up meanwhile");
    assert_eq!(read(&ben_folder, "plan.txt").as_deref(), Some("the plan"), "and nothing is lost here");

    t.world.meta.set_space_archived(&owner, space, false, 2).await.unwrap();
    eventually("what was kept goes up once it is unarchived", || has_file(ana_folder.clone(), "mine.txt", "written while archived")).await;
}

/// L3: pausing rests the long-polls of the team spaces, resuming starts them again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_rests_the_team_spaces_long_polls() {
    let (t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let _ = both_syncing(&t).await;
    eventually("the team space long-polls", || async { front.polls.load(Ordering::SeqCst) >= 3 }).await;

    t.ben.api(|c| c.pause()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let resting = front.polls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(front.polls.load(Ordering::SeqCst), resting, "no long-poll is asked while paused");

    t.ben.api(|c| c.resume()).await.unwrap();
    eventually("the long-poll starts again", || async { front.polls.load(Ordering::SeqCst) > resting }).await;
}

/// L4: one `404` for the list of spaces from a server that speaks team spaces does not end them: the space goes on
/// syncing and is not shown as unavailable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_404_for_the_list_does_not_end_the_team_spaces() {
    let (t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    write(&ana_folder, "one.txt", "1");
    eventually("synced", || has_file(ben_folder.clone(), "one.txt", "1")).await;

    *front.spaces_404.lock().unwrap() = 1;
    eventually("the 404 was answered", || async { *front.spaces_404.lock().unwrap() == 0 }).await;
    // Looked at for a while after it: the space keeps syncing.
    for _ in 0..8 {
        let info = t.ben.space(space).await.unwrap();
        assert_eq!(info.sync, SpaceSync::On, "{info:?}");
        assert!(!matches!(info.run_state, Some(SpaceRun::Unavailable { .. } | SpaceRun::NoAccess { .. })), "{info:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    write(&ana_folder, "two.txt", "2");
    eventually("the sync goes on", || has_file(ben_folder.clone(), "two.txt", "2")).await;
}

// ---------------------------------------------------------------------------------------------------------------------
// The second review round of P1-5

/// N1: turning a space off forgets its state, so turning it on again in the same folder is a first sync: what was deleted
/// here, or on the server, while it was off is not deleted on the other side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_a_space_off_forgets_its_state_so_turning_it_on_again_deletes_nothing() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    for i in 0..6 {
        write(&ana_folder, &format!("f{i}.txt"), &format!("content {i}"));
    }
    eventually("ben has the files", || async { names(&ben_folder).len() == 6 }).await;

    t.ben.sync_off(space).await;
    assert!(t.ben.space_state(space).is_none(), "turning the sync off forgets the saved state");
    // While it is off, ben deletes one file here as from a local copy, and ana deletes another one on the server.
    std::fs::remove_file(ben_folder.join("f0.txt")).unwrap();
    std::fs::remove_file(ana_folder.join("f1.txt")).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(read(&ben_folder, "f1.txt").as_deref(), Some("content 1"), "ana's delete does not reach a space that is off");

    // On again in the same folder: nothing is deleted on either side. What each side deleted comes back from the other.
    let again = t.ben.sync_on(space).await;
    assert_eq!(again, ben_folder);
    eventually("ben's deleted file comes back from the server", || has_file(ben_folder.clone(), "f0.txt", "content 0")).await;
    eventually("ana's deleted file is put back on the server", || has_file(ana_folder.clone(), "f1.txt", "content 1")).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(names(&ana_folder).len(), 6, "nothing was deleted on the server: {:?}", names(&ana_folder));
    assert_eq!(names(&ben_folder).len(), 6, "nor here: {:?}", names(&ben_folder));
}

/// H4, the agent was down for the whole loss of access: the member is removed and invited again while it is stopped, a file
/// is deleted on each side, and the agent is started. The server's stamp of the membership differs from the one the saved state
/// was made under, so the state is not used: nothing is deleted on either side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn access_lost_and_regained_while_the_agent_is_stopped_merges_and_deletes_nothing() {
    let mut t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    for i in 0..4 {
        write(&ana_folder, &format!("f{i}.txt"), &format!("content {i}"));
    }
    eventually("ben has the files", || async { names(&ben_folder).len() == 4 }).await;
    let (exit, out) = t.ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");

    // Ben is removed and invited again while his agent is not running. (The stamps given here differ from the one the server
    // gave when he first accepted, which is the real time.)
    t.world.meta.remove_member(&SpaceActor::Cli, space, t.world.ben.id, 1).await.unwrap();
    t.world.invite_ben(space, SpaceRole::Editor).await;
    t.world.meta.accept_invitation(t.world.ben.id, None, space, 2).await.unwrap();
    std::fs::remove_file(ben_folder.join("f0.txt")).unwrap();
    std::fs::remove_file(ana_folder.join("f1.txt")).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    t.ben.run();
    eventually("the file ben deleted comes back from the server", || has_file(ben_folder.clone(), "f0.txt", "content 0")).await;
    eventually("the file ana deleted is put back on the server", || has_file(ana_folder.clone(), "f1.txt", "content 1")).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(names(&ana_folder).len(), 4, "nothing was deleted on the server: {:?}", names(&ana_folder));
    assert_eq!(names(&ben_folder).len(), 4, "nor here: {:?}", names(&ben_folder));
}

/// N2: turning a space off while its pass is queued behind a slow pass of the personal space does not hold the spaces task up:
/// a second stop ends the loop at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_stop_does_not_wait_for_a_space_that_is_being_turned_off() {
    let (mut t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let space = t.space;
    let (_, ben_folder) = both_syncing(&t).await;
    write(&ben_folder, "first.txt", "first");
    eventually("the first file went up", || async { t.ben.status().await.spaces.iter().any(|s| s.last_sync_ms.is_some()) }).await;

    // The personal pass hangs, and the team space's pass waits for its turn behind it.
    front.stall.store(true, Ordering::SeqCst);
    write(&t.ben.personal, "stuck-too.txt", "never answered");
    eventually("the personal pass is running", || async { t.ben.status().await.run_state == craft_cloud_app::RunState::Syncing }).await;
    write(&ben_folder, "stuck.txt", "never answered");
    eventually("the team space's pass is queued", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == space && s.run_state == SpaceRun::Syncing)
    })
    .await;
    // Turning the space off waits for that pass to let go of it: the call stays open.
    let dir = t.ben.data.path().to_owned();
    let off = tokio::task::spawn_blocking(move || {
        Client::connect_in(&dir).and_then(|c| c.set_space_sync(space, &SpaceSyncRequest { on: false, folder: None, adopt: false }))
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!off.is_finished(), "the change waits for the space's session to close");

    let mut running = t.ben.running.take().unwrap();
    let _ = running.stop.send(());
    let _ = running.stop.send(());
    let thread = running.thread.take().unwrap();
    let started = Instant::now();
    let ended = tokio::time::timeout(Duration::from_secs(8), tokio::task::spawn_blocking(move || thread.join().unwrap())).await;
    front.stall.store(false, Ordering::SeqCst);
    let (exit, out) = ended.expect("the loop ends within seconds of the second stop, though a space is being turned off").unwrap();
    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(exit.unwrap(), Exit::Failed, "{out}");
    drop(off);
}

/// N2: a pass of a space that was queued before the space was turned off does not run afterwards, and turning the space off
/// does not hold up the answer to anything else. Here the pass waits behind a slow personal pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_pass_of_a_space_that_was_turned_off_does_not_run() {
    let (t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    write(&ben_folder, "first.txt", "first");
    eventually("the first file reached ana", || has_file(ana_folder.clone(), "first.txt", "first")).await;

    // Nothing is running (a pass after the first file looks at it again after the stability window), so the personal pass
    // takes the first turn.
    eventually("nothing runs", || async {
        let status = t.ben.status().await;
        status.run_state == craft_cloud_app::RunState::Idle && status.spaces.iter().all(|s| s.run_state == SpaceRun::Idle)
    })
    .await;
    // The personal pass hangs, and the team space's pass waits behind it.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    eventually("still nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;
    front.stall.store(true, Ordering::SeqCst);
    write(&t.ben.personal, "slow.txt", "slow");
    // The personal pass runs and no pass of the team space does: the personal pass holds the turn.
    eventually("the personal pass is running and the team space's is not", || async {
        let status = t.ben.status().await;
        status.run_state == craft_cloud_app::RunState::Syncing && status.spaces.iter().all(|s| s.run_state == SpaceRun::Idle)
    })
    .await;
    write(&ben_folder, "queued.txt", "written before the space was turned off");
    eventually("the team space's pass is queued", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == space && s.run_state == SpaceRun::Syncing)
    })
    .await;
    // The change waits for the pass to let go; other calls are answered meanwhile.
    let dir = t.ben.data.path().to_owned();
    let off = tokio::task::spawn_blocking(move || {
        Client::connect_in(&dir).and_then(|c| c.set_space_sync(space, &SpaceSyncRequest { on: false, folder: None, adopt: false }))
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!off.is_finished(), "the change waits for the space's session to close");
    let started = Instant::now();
    assert!(t.ben.spaces().await.is_some());
    assert!(started.elapsed() < Duration::from_secs(5), "the local API is not held up by a space that is being turned off");
    front.stall.store(false, Ordering::SeqCst);
    let info = off.await.unwrap().unwrap();
    assert_eq!(info.sync, SpaceSync::Off, "{info:?}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(read(&ana_folder, "queued.txt"), None, "the pass queued before the space was turned off did not run");
    assert_eq!(read(&ben_folder, "queued.txt").as_deref(), Some("written before the space was turned off"));
}

/// M1, the pass thread: a second stop ends the loop while a team pass is queued behind a slow pass; the queued pass never
/// starts, even when the slow pass ends later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_team_pass_queued_when_the_second_stop_comes_never_starts() {
    let (mut t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let space = t.space;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    write(&ben_folder, "first.txt", "first");
    eventually("the first file reached ana", || has_file(ana_folder.clone(), "first.txt", "first")).await;

    // Nothing is running (a pass after the first file looks at it again after the stability window), so the personal pass
    // takes the first turn.
    eventually("nothing runs", || async {
        let status = t.ben.status().await;
        status.run_state == craft_cloud_app::RunState::Idle && status.spaces.iter().all(|s| s.run_state == SpaceRun::Idle)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    eventually("still nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;
    front.stall.store(true, Ordering::SeqCst);
    write(&t.ben.personal, "slow.txt", "slow");
    // The personal pass runs and no pass of the team space does: the personal pass holds the turn.
    eventually("the personal pass is running and the team space's is not", || async {
        let status = t.ben.status().await;
        status.run_state == craft_cloud_app::RunState::Syncing && status.spaces.iter().all(|s| s.run_state == SpaceRun::Idle)
    })
    .await;
    write(&ben_folder, "queued.txt", "never to be uploaded");
    eventually("the team space's pass is queued", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == space && s.run_state == SpaceRun::Syncing)
    })
    .await;
    let mut running = t.ben.running.take().unwrap();
    let _ = running.stop.send(());
    let _ = running.stop.send(());
    let thread = running.thread.take().unwrap();
    let (exit, out) = tokio::time::timeout(Duration::from_secs(8), tokio::task::spawn_blocking(move || thread.join().unwrap()))
        .await
        .expect("the loop ends within seconds of the second stop")
        .unwrap();
    assert_eq!(exit.unwrap(), Exit::Failed, "{out}");
    // The slow pass ends by itself later; the queued pass must not take its turn.
    front.stall.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(read(&ana_folder, "queued.txt"), None, "the queued pass did not start after the second stop");
}

/// A discovery that fails while someone waits for the list is tried again within seconds, and they wait for that: an
/// invitation accepted here and the sync turned on at once finds the space in the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepting_and_turning_the_sync_on_at_once_works_after_one_failed_discovery() {
    // Ben's agent looks at the list only when asked (the periodic look is far off), so the one failure is the look the accept
    // asks for.
    let world = world().await;
    let (url, front, _task) = front(&world.url, Front::default()).await;
    let mut ben = Person::signed_in(&url, "ben").await;
    let space = world.space("Studio").await;
    world.invite_ben(space, SpaceRole::Editor).await;
    ben.run_with(RunOptions { poll_wait: Duration::from_secs(30), ..quick() });
    let t = Team { ana: Person::signed_in(&world.url, "ana").await, ben, world, space };
    eventually("the invitation is listed", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    *front.spaces_404.lock().unwrap() = 1;
    t.ben.api(move |c| c.accept_space(space)).await.unwrap();
    // No wait for the list in between: the agent has found the space, or the answer to the accept waited until it did.
    let info = t.ben.api(move |c| c.set_space_sync(space, &SpaceSyncRequest { on: true, folder: None, adopt: false })).await;
    assert!(info.is_ok_and(|i| i.sync == SpaceSync::On), "the sync could be turned on right after the accept");
    assert_eq!(*front.spaces_404.lock().unwrap(), 0, "the failed discovery happened");
}

// ---------------------------------------------------------------------------------------------------------------------
// The third review round of P1-5

/// F1: turning a space on does not wait for another space's long pass: the pass thread is busy with space A, and space B,
/// which has no session and no pass, is turned on at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_on_a_space_is_not_held_up_by_a_long_pass_of_another_space() {
    let (t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let a = t.space;
    let b = t.world.space("Archive").await;
    t.world.invite_ben(b, SpaceRole::Editor).await;
    for space in [a, b] {
        let ben = &t.ben;
        eventually("the invitation is listed", || async move { ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
        t.ben.accept(space).await;
    }
    let a_folder = t.ben.sync_on(a).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;

    // A's pass hangs; the pass thread is busy with it.
    front.stall.store(true, Ordering::SeqCst);
    write(&a_folder, "big.txt", "a long first sync, say");
    eventually("A's pass is running", || async { t.ben.status().await.spaces.iter().any(|s| s.space == a && s.run_state == SpaceRun::Syncing) })
        .await;
    let dir = t.ben.data.path().to_owned();
    let on = tokio::task::spawn_blocking(move || {
        Client::connect_in(&dir).and_then(|c| c.set_space_sync(b, &SpaceSyncRequest { on: true, folder: None, adopt: false }))
    });
    let answered = tokio::time::timeout(Duration::from_secs(10), on).await;
    front.stall.store(false, Ordering::SeqCst);
    let info = answered.expect("space B is turned on while A's pass runs").unwrap().unwrap();
    assert_eq!(info.sync, SpaceSync::On, "{info:?}");
}

/// F2: the agent stops while a space is being turned off (its pass has not let go yet): the space stays off at the next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_space_being_turned_off_when_the_agent_stops_stays_off() {
    let (mut t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let space = t.space;
    let (_, ben_folder) = both_syncing(&t).await;
    write(&ben_folder, "first.txt", "first");
    eventually("the first file went up", || async { t.ben.status().await.spaces.iter().any(|s| s.last_sync_ms.is_some()) }).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;

    front.stall.store(true, Ordering::SeqCst);
    write(&ben_folder, "busy.txt", "never answered");
    eventually("the space's pass is running", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == space && s.run_state == SpaceRun::Syncing)
    })
    .await;
    let dir = t.ben.data.path().to_owned();
    let off = tokio::task::spawn_blocking(move || {
        Client::connect_in(&dir).and_then(|c| c.set_space_sync(space, &SpaceSyncRequest { on: false, folder: None, adopt: false }))
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!off.is_finished(), "turning the space off waits for its pass");

    // A stop waits for the running pass (the personal space is idle, so the loop does not wait for a second stop); the
    // space's turn-off is applied when the spaces task ends, before the pass lets go.
    let mut running = t.ben.running.take().unwrap();
    let _ = running.stop.send(());
    let thread = running.thread.take().unwrap();
    let ended = tokio::spawn(async move { tokio::task::spawn_blocking(move || thread.join().unwrap()).await.unwrap() });
    tokio::time::sleep(Duration::from_secs(2)).await;
    front.stall.store(false, Ordering::SeqCst);
    let (exit, out) = tokio::time::timeout(Duration::from_secs(30), ended).await.expect("the loop ends once the pass has").unwrap();
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    drop(off);

    t.ben.run();
    eventually("the space is listed", || async { t.ben.space(space).await.is_some() }).await;
    let info = t.ben.space(space).await.unwrap();
    assert_eq!(info.sync, SpaceSync::Off, "the space that was being turned off stays off: {info:?}");
}

/// The members, usage, name and leaving of a team space, and the administrator's calls, go through the local API to the
/// server: the server decides who may do what, and the agent keeps the folder when the account leaves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_members_usage_name_and_leaving_of_a_space_and_the_admin_calls_are_passed_on_to_the_server() {
    use craft_cloud_app::{AddSpaceMember, AdminCreateSpace, AdminSetSpaceOwner, ChangeSpaceMember, SpaceConfirm, UpdateSpace, UserQuery};
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let hash = hash_password(PASSWORD).unwrap();
    let cleo = t.world.meta.create_user("cleo", &hash, Role::Member, 0).await.unwrap();
    let root = t.world.meta.create_user("root", &hash, Role::Admin, 0).await.unwrap();
    let mut root_pc = Person::signed_in(&t.world.url, "root").await;
    root_pc.run();
    root_pc.ready().await;
    eventually("the invitation is listed", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(space).await;

    // Members: any member reads them, with roles and states.
    let members = t.ana.api(move |c| c.space_members(space)).await.unwrap().members;
    let roles: Vec<(&str, SpaceRole, &str)> = members.iter().map(|m| (m.username.as_str(), m.role, m.state.as_str())).collect();
    assert!(roles.contains(&("ana", SpaceRole::Owner, "active")) && roles.contains(&("ben", SpaceRole::Editor, "active")), "{roles:?}");
    let seen_by_ben = t.ben.api(move |c| c.space_members(space)).await.unwrap().members;
    assert_eq!(seen_by_ben.len(), members.len());

    // An editor may not invite: the server's `read_only` passes through.
    let invite = |name: &str, role| AddSpaceMember { username: name.to_owned(), role, confirm: SpaceConfirm::default() };
    let refused = t
        .ben
        .api(move |c| {
            c.add_space_member(space, &AddSpaceMember { username: "cleo".into(), role: SpaceRole::Viewer, confirm: SpaceConfirm::default() })
        })
        .await
        .unwrap_err();
    assert!(matches!(&refused, craft_cloud_app::Error::Refused { code, .. } if code == "read_only"), "{refused:?}");

    // An owner invites, changes the role and withdraws the invitation.
    let request = invite("cleo", SpaceRole::Viewer);
    let added = t.ana.api(move |c| c.add_space_member(space, &request)).await.unwrap();
    assert_eq!((added.username.as_str(), added.role, added.state.as_str()), ("cleo", SpaceRole::Viewer, "invited"));
    let cleo_id = cleo.id.0;
    let changed = t
        .ana
        .api(move |c| c.change_space_member(space, cleo_id, &ChangeSpaceMember { role: SpaceRole::Editor, confirm: SpaceConfirm::default() }))
        .await
        .unwrap();
    assert_eq!(changed.role, SpaceRole::Editor);
    t.ana.api(move |c| c.remove_space_member(space, cleo_id)).await.unwrap();
    let after = t.ana.api(move |c| c.space_members(space)).await.unwrap().members;
    assert!(after.iter().all(|m| m.username != "cleo"), "{after:?}");
    let unknown = t
        .ana
        .api(move |c| {
            c.add_space_member(space, &AddSpaceMember { username: "nobody".into(), role: SpaceRole::Viewer, confirm: SpaceConfirm::default() })
        })
        .await
        .unwrap_err();
    assert!(matches!(&unknown, craft_cloud_app::Error::Refused { code, .. } if code == "not_found"), "{unknown:?}");

    // Usage, and a new name that shows in ben's list while his folder keeps its name.
    let usage = t.ben.api(move |c| c.space_usage(space)).await.unwrap();
    assert_eq!(usage.stored_bytes, 0);
    let planned = t.ben.space(space).await.unwrap().folder.unwrap();
    let renamed = t.ana.api(move |c| c.update_space(space, &UpdateSpace { name: Some("Studio 2".into()), ..UpdateSpace::default() })).await.unwrap();
    assert_eq!(renamed.name, "Studio 2");
    eventually("ben's list has the new name", || async { t.ben.space(space).await.is_some_and(|s| s.name.as_deref() == Some("Studio 2")) }).await;
    assert_eq!(t.ben.space(space).await.unwrap().folder.as_deref(), Some(planned.as_str()), "the folder keeps its name");

    // The administrator's calls need the administrator's password; a member is `forbidden`.
    let as_member = t.ben.api(|c| c.admin_spaces(&UserQuery::default())).await.unwrap_err();
    assert!(matches!(&as_member, craft_cloud_app::Error::Refused { code, .. } if code == "forbidden"), "{as_member:?}");
    let confirm = SpaceConfirm { password: Some(PASSWORD.into()), totp_code: None, recovery_code: None };
    let wrong = SpaceConfirm { password: Some("wrong password".into()), totp_code: None, recovery_code: None };
    let ana_id = t.world.ana.id.0;
    let (create_confirm, create_wrong) = (confirm.clone(), wrong.clone());
    let refused = root_pc
        .api(move |c| c.admin_create_space(&AdminCreateSpace { name: "Briefs".into(), owner: ana_id, quota_bytes: None, confirm: create_wrong }))
        .await
        .unwrap_err();
    assert!(matches!(&refused, craft_cloud_app::Error::Refused { code, .. } if code == "wrong_password"), "{refused:?}");
    let created = root_pc
        .api(move |c| {
            c.admin_create_space(&AdminCreateSpace { name: "Briefs".into(), owner: ana_id, quota_bytes: Some(8 << 20), confirm: create_confirm })
        })
        .await
        .unwrap();
    assert_eq!((created.name.as_str(), created.owners, created.quota_bytes), ("Briefs", 1, Some(8 << 20)));
    let listed = root_pc.api(|c| c.admin_spaces(&UserQuery { query: Some("brie".into()), ..UserQuery::default() })).await.unwrap();
    assert_eq!(listed.spaces.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Briefs"]);
    let briefs = created.id;
    let limit_confirm = confirm.clone();
    let limited = root_pc
        .api(move |c| c.update_space(briefs, &UpdateSpace { name: None, quota_bytes: Some(Some(16 << 20)), confirm: limit_confirm }))
        .await
        .unwrap();
    assert_eq!(limited.quota_bytes, Some(16 << 20));
    let archive_confirm = confirm.clone();
    root_pc.api(move |c| c.admin_archive_space(briefs, &archive_confirm)).await.unwrap();
    let archived = root_pc.api(|c| c.admin_spaces(&UserQuery::default())).await.unwrap();
    assert!(archived.spaces.iter().any(|s| s.id == briefs && s.archived_ms.is_some()), "{archived:?}");
    let unarchive_confirm = confirm.clone();
    root_pc.api(move |c| c.admin_unarchive_space(briefs, &unarchive_confirm)).await.unwrap();
    let owner_confirm = confirm.clone();
    let cleo_user = cleo.id.0;
    root_pc.api(move |c| c.admin_set_space_owner(briefs, &AdminSetSpaceOwner { user: cleo_user, confirm: owner_confirm })).await.unwrap();
    let after_owner = root_pc.api(|c| c.admin_spaces(&UserQuery::default())).await.unwrap();
    assert!(after_owner.spaces.iter().any(|s| s.id == briefs && s.owners == 2 && s.archived_ms.is_none()), "{after_owner:?}");
    let _ = root;

    // Leaving: ben's folder stays with its files and the space stops syncing there; the last owner may not leave.
    let ben_folder = t.ben.sync_on(space).await;
    write(&ben_folder, "mine.txt", "ben's file");
    let ana_folder = t.ana.sync_on(space).await;
    eventually("the file is on ana's side", || has_file(ana_folder.clone(), "mine.txt", "ben's file")).await;
    t.ben.api(move |c| c.leave_space(space)).await.unwrap();
    assert_eq!(read(&ben_folder, "mine.txt").as_deref(), Some("ben's file"), "the local copy is kept");
    eventually("ben no longer syncs the space", || async { t.ben.space(space).await.is_none_or(|s| s.sync != SpaceSync::On) }).await;
    let now = t.ana.api(move |c| c.space_members(space)).await.unwrap().members;
    assert!(now.iter().all(|m| m.username != "ben"), "{now:?}");
    let state_before = t.ana.space_state(space);
    assert!(state_before.is_some(), "ana's space has a saved state");
    let last = t.ana.api(move |c| c.leave_space(space)).await.unwrap_err();
    assert!(matches!(&last, craft_cloud_app::Error::Refused { code, .. } if code == "last_owner"), "{last:?}");
    // The server refused first, so nothing changed here: still syncing, its state untouched.
    let info = t.ana.space(space).await.unwrap();
    assert_eq!(info.sync, SpaceSync::On, "{info:?}");
    assert!(t.ana.space_state(space).is_some(), "the saved state was not forgotten");
}

/// A refused upload is the server's answer, not a network error. The server decides from the headers (an oversized chunk) and
/// answers before the client has finished sending the body; it reads the rest and drops it first, so the client sees the answer
/// and not a reset connection (which Windows reports before the answer can be read).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_oversized_chunk_is_the_servers_answer_and_not_a_network_error() {
    let world = world().await;
    let ben = Person::signed_in(&world.url, "ben").await;
    let account = ben.data.load_account().unwrap().unwrap();
    let transport = HttpTransport::new(ServerUrl::parse(&world.url).unwrap(), &account.token).unwrap();
    let big = vec![7u8; 4 * 1024 * 1024 + 1];
    let id = craft_cloud_proto::ChunkId::from_bytes(*blake3::hash(&big).as_bytes());
    // Several at once, many times over: the race is between the client's writing and the server's answer.
    let body = bytes::Bytes::from(big);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (transport, body) = (transport.clone(), body.clone());
        tasks.push(tokio::spawn(async move {
            for _ in 0..25 {
                let refused = transport.put_chunk(&id, body.clone()).await.unwrap_err();
                assert!(refused.to_string().contains("over the size limit"), "{refused}");
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The fourth review round of P1-5 (items 3 to 5 of the independent review)

/// Makes `link` a symlink (Unix) or junction (Windows) to the folder `target`.
fn link_dir(link: &Path, target: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd.exe").args(["/d", "/c", "mklink", "/J"]).arg(link).arg(target).output().unwrap();
        assert!(made.status.success(), "{made:?}");
    }
}

/// Removes the link `link` made by [`link_dir`], not what it leads to.
fn unlink_dir(link: &Path) {
    #[cfg(unix)]
    std::fs::remove_file(link).unwrap();
    #[cfg(windows)]
    std::fs::remove_dir(link).unwrap();
}

/// Item 3: a team space syncs through a link to folder A. While its session is open, the link is pointed at folder B, which
/// carries the same space's marker but not A's files. The session notices before the next pass and starts afresh in B (a
/// first sync): the files A had are not taken for deleted in B, and nothing is deleted on the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_link_pointed_at_another_folder_while_syncing_deletes_nothing() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    eventually("the invitation is listed", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(space).await;
    eventually("ana's agent lists the space", || async { t.ana.space(space).await.is_some_and(|s| s.state == "active") }).await;
    let ana_folder = t.ana.sync_on(space).await;
    let (a, b, link) = (t.ben.dir.path().join("A"), t.ben.dir.path().join("B"), t.ben.dir.path().join("Link"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    link_dir(&link, &a);
    t.ben.sync_on_in(space, &link).await;
    for i in 0..6 {
        write(&ana_folder, &format!("f{i}.txt"), &format!("content {i}"));
    }
    eventually("the folder behind the link has the files", || async { names(&a).len() == 6 }).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;

    // Folder B carries the space's marker (as an older folder of the space would) but none of the files.
    std::fs::create_dir_all(b.join(".craft-cloud")).unwrap();
    std::fs::copy(a.join(".craft-cloud/root.json"), b.join(".craft-cloud/root.json")).unwrap();
    unlink_dir(&link);
    link_dir(&link, &b);
    write(&ana_folder, "poke.txt", "asks for a pass");
    eventually("the folder behind the link now has everything", || async { names(&b).len() == 7 }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(names(&ana_folder).len(), 7, "nothing was deleted on the server: {:?}", names(&ana_folder));
    assert_eq!(names(&a).len(), 6, "the old folder is left as it is");
}

/// After a link is pointed at another folder (and the space starts afresh there), a local change in the new folder wakes a pass:
/// the folder watcher is made again for where the link leads now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_change_behind_a_repointed_link_is_noticed() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    eventually("the invitation is listed", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(space).await;
    eventually("ana's agent lists the space", || async { t.ana.space(space).await.is_some_and(|s| s.state == "active") }).await;
    let ana_folder = t.ana.sync_on(space).await;
    let (a, b, link) = (t.ben.dir.path().join("A"), t.ben.dir.path().join("B"), t.ben.dir.path().join("Link"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    link_dir(&link, &a);
    t.ben.sync_on_in(space, &link).await;
    write(&ana_folder, "first.txt", "the first file");
    eventually("the folder behind the link has the file", || async { names(&a).len() == 1 }).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;

    // The link is pointed at B (with the space's marker); a change on the server asks for the pass that notices.
    std::fs::create_dir_all(b.join(".craft-cloud")).unwrap();
    std::fs::copy(a.join(".craft-cloud/root.json"), b.join(".craft-cloud/root.json")).unwrap();
    unlink_dir(&link);
    link_dir(&link, &b);
    write(&ana_folder, "poke.txt", "asks for a pass");
    eventually("the folder behind the link now has everything", || async { names(&b).len() == 2 }).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;

    // A change in B reaches the server although nothing else asks for a pass; one in the old folder A is not synced.
    write(&a, "old.txt", "in the folder the link left");
    write(&link, "late.txt", "written behind the link");
    eventually("the local change reaches the server", || async { read(&ana_folder, "late.txt").as_deref() == Some("written behind the link") }).await;
    assert_eq!(read(&ana_folder, "old.txt"), None, "the old folder is not synced any more");
}

/// A file that can't be deleted while this lives: held open without delete sharing (Windows), or in a read-only folder (Unix).
struct Held {
    _file: std::fs::File,
    #[cfg(unix)]
    dir: PathBuf,
}

impl Held {
    fn new(path: &Path) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Share read and write, not delete.
            Self { _file: std::fs::OpenOptions::new().read(true).share_mode(1 | 2).open(path).unwrap() }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = path.parent().unwrap().to_owned();
            let file = std::fs::File::open(path).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            Self { _file: file, dir }
        }
    }
}

#[cfg(unix)]
impl Drop for Held {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o755));
    }
}

/// Item 4: turning a space off when its saved state can't be removed (something holds the file) is not acknowledged as done:
/// the call fails with the error. The setting's generation was raised and saved first, so the state that survived is never
/// trusted again: after a restart, files deleted here meanwhile are not deleted on the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_state_that_cannot_be_removed_fails_the_turn_off_and_is_never_trusted_again() {
    let mut t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, a) = both_syncing(&t).await;
    for i in 0..6 {
        write(&ana_folder, &format!("f{i}.txt"), &format!("content {i}"));
    }
    eventually("ben has the files", || async { names(&a).len() == 6 }).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;
    let state_path = t.ben.data.path().join("spaces").join(space.to_string()).join("state.json");
    assert!(state_path.is_file());

    let generation = |t: &Team| t.ben.data.load_account().unwrap().unwrap().spaces.iter().find(|s| s.space == space).map(|s| s.generation);
    let before = generation(&t).unwrap();
    let held = Held::new(&state_path);
    let off = t.ben.api(move |c| c.set_space_sync(space, &SpaceSyncRequest { on: false, folder: None, adopt: false })).await;
    assert!(off.is_err(), "the state could not be removed: the turn-off is not acknowledged: {off:?}");
    assert!(state_path.is_file(), "the state survived");
    assert_eq!(generation(&t), Some(before + 1), "the generation was raised and saved");

    // The agent stops while the file is still held (nothing replaces it), then files are deleted here.
    let (exit, out) = t.ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    drop(held);
    assert!(state_path.is_file(), "the old state is still there");
    for i in 0..3 {
        std::fs::remove_file(a.join(format!("f{i}.txt"))).unwrap();
    }
    t.ben.run();
    eventually("the files deleted here come back from the server", || async { names(&a).len() == 6 }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(names(&ana_folder).len(), 6, "nothing was deleted on the server: {:?}", names(&ana_folder));
}

/// Item 5: a team folder is wiped and recreated at the same path; setting it up installs the space's marker and stops right
/// there (as a crash would), before the old state is removed. The old state must not be trusted for the new folder after a
/// restart: the files it knew are not taken for deleted, and the folder is filled from the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_after_the_marker_is_installed_does_not_revive_the_old_state() {
    let mut t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, a) = both_syncing(&t).await;
    for i in 0..6 {
        write(&ana_folder, &format!("f{i}.txt"), &format!("content {i}"));
    }
    eventually("ben has the files", || async { names(&a).len() == 6 }).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;
    t.ben.api(|c| c.pause()).await.unwrap();

    // The folder is wiped and made again, empty; the person turns the sync on for it, and the set-up stops after the marker.
    std::fs::remove_dir_all(&a).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    *craft_cloud_agent::spaces::STOP_AFTER_MARKER.lock().unwrap() = Some(a.clone());
    let again = t.ben.api(move |c| c.set_space_sync(space, &SpaceSyncRequest { on: true, folder: None, adopt: false })).await;
    assert!(again.is_err(), "the set-up stopped after the marker: {again:?}");
    assert!(a.join(".craft-cloud/root.json").is_file(), "the marker was written");
    assert!(t.ben.space_state(space).is_some(), "the old state was not removed yet");

    // The agent restarts; the old state must not decide what the new folder is missing.
    let (exit, out) = t.ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    t.ben.run();
    t.ben.ready().await;
    t.ben.api(|c| c.resume()).await.unwrap();
    eventually("the folder is filled from the server", || async { names(&a).len() == 6 }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(names(&ana_folder).len(), 6, "nothing was deleted on the server: {:?}", names(&ana_folder));
}

// ---------------------------------------------------------------------------------------------------------------------
// The fifth review round of P1-5 (the re-review of the fourth)

/// Replaces `folder` at the same path as a backup restore would: it is wiped and made again with the space's marker and only
/// the files in `keep` (with the content they had). The new folder has another ID.
fn restore_backup(folder: &Path, keep: &[&str]) {
    let marker = std::fs::read(folder.join(".craft-cloud/root.json")).unwrap();
    let kept: Vec<(String, Vec<u8>)> = keep.iter().map(|n| ((*n).to_owned(), std::fs::read(folder.join(n)).unwrap())).collect();
    std::fs::remove_dir_all(folder).unwrap();
    std::fs::create_dir_all(folder.join(".craft-cloud")).unwrap();
    std::fs::write(folder.join(".craft-cloud/root.json"), marker).unwrap();
    for (name, data) in kept {
        std::fs::write(folder.join(name), data).unwrap();
    }
}

/// Ben syncs a team folder with six files; returns ana's folder and his.
async fn six_files(t: &Team) -> (PathBuf, PathBuf) {
    let (ana_folder, a) = both_syncing(t).await;
    for i in 0..6 {
        write(&ana_folder, &format!("f{i}.txt"), &format!("content {i}"));
    }
    eventually("ben has the files", || async { names(&a).len() == 6 }).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;
    (ana_folder, a)
}

/// F1, same path, running: the folder is replaced by a backup that holds the marker but not the newer files, while the session is
/// open, and the state file can't be removed. The old state is not trusted when the session reopens: nothing is deleted on the
/// server and the folder is filled from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_replaced_at_the_same_path_with_a_state_that_cannot_be_removed_deletes_nothing() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, a) = six_files(&t).await;
    let state_path = t.ben.data.path().join("spaces").join(space.to_string()).join("state.json");
    let generation = |t: &Team| t.ben.data.load_account().unwrap().unwrap().spaces.iter().find(|s| s.space == space).map(|s| s.generation);
    let before = generation(&t).unwrap();
    let held = Held::new(&state_path);
    restore_backup(&a, &["f0.txt", "f1.txt"]);
    write(&ana_folder, "poke.txt", "asks for a pass");
    eventually("the folder is filled from the server", || async { names(&a).len() == 7 }).await;
    drop(held);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(names(&ana_folder).len(), 7, "nothing was deleted on the server: {:?}", names(&ana_folder));
    assert!(generation(&t) > Some(before), "the generation was raised when the folder was found replaced");
}

/// F1, same path, across a restart: the agent is stopped while the folder is replaced by a backup with the marker but without the
/// newer files. The state records the old folder's ID, so it is not used for the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_replaced_at_the_same_path_while_the_agent_is_stopped_deletes_nothing() {
    let mut t = team(SpaceRole::Editor).await;
    let (ana_folder, a) = six_files(&t).await;
    let (exit, out) = t.ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    restore_backup(&a, &["f0.txt", "f1.txt"]);
    t.ben.run();
    eventually("the folder is filled from the server", || async { names(&a).len() == 6 }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(names(&ana_folder).len(), 6, "nothing was deleted on the server: {:?}", names(&ana_folder));
}

/// F1, same path, across a pass that needs the user: the folder goes (the pass stops, the session is dropped) and comes back
/// at the same path as a backup with the marker but without the newer files. Nothing is deleted on the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_that_goes_and_comes_back_replaced_deletes_nothing() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, a) = six_files(&t).await;
    let marker = std::fs::read(a.join(".craft-cloud/root.json")).unwrap();
    let kept: Vec<Vec<u8>> = ["f0.txt", "f1.txt"].iter().map(|n| std::fs::read(a.join(n)).unwrap()).collect();
    std::fs::remove_dir_all(&a).unwrap();
    write(&ana_folder, "poke.txt", "asks for a pass");
    eventually("the pass needs the user", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == space && matches!(s.run_state, SpaceRun::Paused { .. }))
    })
    .await;
    std::fs::create_dir_all(a.join(".craft-cloud")).unwrap();
    std::fs::write(a.join(".craft-cloud/root.json"), marker).unwrap();
    std::fs::write(a.join("f0.txt"), &kept[0]).unwrap();
    std::fs::write(a.join("f1.txt"), &kept[1]).unwrap();
    eventually("the folder is filled from the server", || async { names(&a).len() == 7 }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(names(&ana_folder).len(), 7, "nothing was deleted on the server: {:?}", names(&ana_folder));
}

/// F3: a link is pointed at another folder (with the same marker, without the files) in the middle of a pass, after the pass began
/// and before it sends its deletions. The pass stops without deleting anything on the server, and the next one starts afresh in the
/// new folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_pointed_at_another_folder_during_a_pass_deletes_nothing() {
    let (t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let space = t.space;
    eventually("the invitation is listed", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(space).await;
    eventually("ana's agent lists the space", || async { t.ana.space(space).await.is_some_and(|s| s.state == "active") }).await;
    let ana_folder = t.ana.sync_on(space).await;
    let (a, b, link) = (t.ben.dir.path().join("A"), t.ben.dir.path().join("B"), t.ben.dir.path().join("Link"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    link_dir(&link, &a);
    t.ben.sync_on_in(space, &link).await;
    for i in 0..6 {
        write(&ana_folder, &format!("f{i}.txt"), &format!("content {i}"));
    }
    eventually("the folder behind the link has the files", || async { names(&a).len() == 6 }).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;

    // A pass begins (a local change) and waits at the server; meanwhile the link is pointed at folder B.
    front.stall.store(true, Ordering::SeqCst);
    write(&link, "local.txt", "a local change");
    eventually("the pass is running", || async { t.ben.status().await.spaces.iter().any(|s| s.space == space && s.run_state == SpaceRun::Syncing) })
        .await;
    std::fs::create_dir_all(b.join(".craft-cloud")).unwrap();
    std::fs::copy(a.join(".craft-cloud/root.json"), b.join(".craft-cloud/root.json")).unwrap();
    unlink_dir(&link);
    link_dir(&link, &b);
    front.stall.store(false, Ordering::SeqCst);
    eventually("the folder behind the link is filled from the server", || async { names(&b).len() == 6 }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(names(&ana_folder).len(), 6, "nothing was deleted on the server: {:?}", names(&ana_folder));
}

/// F4: losing access raises the space's generation (and saves it); when account.json cannot be written at that moment, the raise
/// is kept and saved at a later look at the list, so a restart still finds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn losing_access_raises_the_generation_even_when_it_cannot_be_saved_at_once() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (_, a) = six_files(&t).await;
    let generation = |t: &Team| t.ben.data.load_account().unwrap().unwrap().spaces.iter().find(|s| s.space == space).map(|s| s.generation);
    let before = generation(&t).unwrap();
    let account_path = t.ben.data.path().join("account.json");

    // account.json can't be replaced while it is held: the raise is made in memory but not saved.
    let held = Held::new(&account_path);
    t.world.meta.remove_member(&SpaceActor::Cli, space, t.world.ben.id, 1).await.unwrap();
    eventually("the space reports no access", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == space && matches!(s.run_state, SpaceRun::NoAccess { .. }))
    })
    .await;
    assert_eq!(generation(&t), Some(before), "account.json could not be written");
    drop(held);
    eventually("the raised generation is saved later", || async { generation(&t) > Some(before) }).await;
    assert_eq!(names(&a).len(), 6, "the local copy is kept");
}

// ---------------------------------------------------------------------------------------------------------------------
// The sixth review round of P1-5 (N1 to N4 of the re-review of the fifth)

/// N1: a link is pointed at another folder in the middle of a pass; the pass takes in the server's newest file there, stops at
/// its first checkpoint, and the link is pointed back before the next pass. The session that took in part of the other folder is
/// not used again (nor its state saved): the file is not taken for deleted in the first folder and deleted on the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_pointed_elsewhere_and_back_around_a_pass_deletes_nothing() {
    let (t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let space = t.space;
    eventually("the invitation is listed", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(space).await;
    eventually("ana's agent lists the space", || async { t.ana.space(space).await.is_some_and(|s| s.state == "active") }).await;
    let ana_folder = t.ana.sync_on(space).await;
    let (a, b, link) = (t.ben.dir.path().join("A"), t.ben.dir.path().join("B"), t.ben.dir.path().join("Link"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    link_dir(&link, &a);
    t.ben.sync_on_in(space, &link).await;
    for i in 0..6 {
        write(&ana_folder, &format!("f{i}.txt"), &format!("content {i}"));
    }
    eventually("the folder behind the link has the files", || async { names(&a).len() == 6 }).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;

    // A pass begins and waits at the server. The server gets a new file; the link is pointed at B; syncing is paused so no pass
    // follows at once; the pass goes on, takes the new file into B and stops at its first checkpoint.
    front.stall.store(true, Ordering::SeqCst);
    write(&link, "local.txt", "a local change");
    eventually("the pass is running", || async { t.ben.status().await.spaces.iter().any(|s| s.space == space && s.run_state == SpaceRun::Syncing) })
        .await;
    write(&ana_folder, "newest.txt", "from the server");
    tokio::time::sleep(Duration::from_secs(3)).await;
    std::fs::create_dir_all(b.join(".craft-cloud")).unwrap();
    std::fs::copy(a.join(".craft-cloud/root.json"), b.join(".craft-cloud/root.json")).unwrap();
    unlink_dir(&link);
    link_dir(&link, &b);
    t.ben.api(|c| c.pause()).await.unwrap();
    front.stall.store(false, Ordering::SeqCst);
    eventually("the pass took the new file into the other folder", || async { read(&b, "newest.txt").as_deref() == Some("from the server") }).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Pointed back before the next pass: that pass must not trust what the first one took in.
    unlink_dir(&link);
    link_dir(&link, &a);
    t.ben.api(|c| c.resume()).await.unwrap();
    eventually("the first folder gets the server's file", || async { read(&a, "newest.txt").as_deref() == Some("from the server") }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(read(&ana_folder, "newest.txt").as_deref(), Some("from the server"), "the server's file was not deleted");
    for i in 0..6 {
        assert_eq!(read(&ana_folder, &format!("f{i}.txt")).as_deref(), Some(format!("content {i}").as_str()), "f{i} is still on the server");
    }
}

/// N3: the nonce in a team folder's `.craft-cloud` is rewritten at every checkpoint and recorded in the state saved with it, so a
/// folder restored from an older copy is told apart even where a file system reuses folder IDs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_folder_nonce_is_rewritten_and_recorded_in_the_state() {
    let t = team(SpaceRole::Editor).await;
    let space = t.space;
    let (ana_folder, a) = six_files(&t).await;
    let nonce_of = |folder: &Path| std::fs::read_to_string(folder.join(".craft-cloud/nonce")).ok();
    let before = nonce_of(&a).expect("the folder has a nonce once a pass has run");
    write(&ana_folder, "more.txt", "another file");
    eventually("the nonce was rewritten", || async { nonce_of(&a).is_some_and(|n| n != before) }).await;
    eventually("nothing runs", || async { t.ben.status().await.spaces.iter().all(|s| s.run_state == SpaceRun::Idle) }).await;
    let nonce = nonce_of(&a).unwrap();
    let recorded = t.ben.space_state(space).and_then(|s| s["folder_nonce"].as_str().map(str::to_owned));
    assert_eq!(recorded, Some(nonce), "the state records the nonce the folder carries");
}

/// N4: while a raised generation of one space cannot be saved, only that space is held back from starting, and its state says so;
/// another space goes on syncing. Once account.json can be written the space starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_space_with_an_unsaved_generation_is_held_back_and_says_so() {
    let t = team(SpaceRole::Editor).await;
    let a = t.space;
    let b = t.world.space("Archive").await;
    t.world.invite_ben(b, SpaceRole::Editor).await;
    let (_, _) = both_syncing(&t).await;
    eventually("the second invitation is listed", || async { t.ben.space(b).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(b).await;
    eventually("ana's agent lists the second space", || async { t.ana.space(b).await.is_some_and(|s| s.state == "active") }).await;
    let (ana_b, ben_b) = (t.ana.sync_on(b).await, t.ben.sync_on(b).await);
    let account_path = t.ben.data.path().join("account.json");

    // account.json can't be written: the first space loses access (its raise can't be saved), then gets it back.
    let held = Held::new(&account_path);
    t.world.meta.remove_member(&SpaceActor::Cli, a, t.world.ben.id, 1).await.unwrap();
    eventually("the first space reports no access", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == a && matches!(s.run_state, SpaceRun::NoAccess { .. }))
    })
    .await;
    t.world.invite_ben(a, SpaceRole::Editor).await;
    t.world.meta.accept_invitation(t.world.ben.id, None, a, 2).await.unwrap();
    eventually("the first space is held back and says why", || async {
        t.ben
            .status()
            .await
            .spaces
            .iter()
            .any(|s| s.space == a && matches!(&s.run_state, SpaceRun::Paused { reason, .. } if reason.contains("could not be saved")))
    })
    .await;
    // The second space goes on.
    write(&ana_b, "meanwhile.txt", "the other space is not held back");
    eventually("the second space still syncs", || has_file(ben_b.clone(), "meanwhile.txt", "the other space is not held back")).await;

    drop(held);
    eventually("the first space starts once the settings can be saved", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == a && !matches!(s.run_state, SpaceRun::Paused { .. } | SpaceRun::NoAccess { .. }))
    })
    .await;
}

// ---------------------------------------------------------------------------------------------------------------------
// Live documents in team spaces

/// Blocking work off the test's runtime.
async fn off<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

fn text_snapshot(text: &str) -> Vec<u8> {
    craft_cloud_agent::doc::plain_text_snapshot(text).unwrap()
}

/// An app's Loro document for an open live document, and the events it was sent that are not changes.
struct AppDoc {
    loro: LoroDoc,
    doc: Document,
    seen: Mutex<Vec<Event>>,
}

impl AppDoc {
    fn new(doc: Document, peer: u64) -> Self {
        let loro = LoroDoc::new();
        loro.set_peer_id(peer).unwrap();
        loro.import(doc.state()).unwrap();
        for u in doc.state_updates() {
            loro.import(u).unwrap();
        }
        Self { loro, doc, seen: Mutex::new(Vec::new()) }
    }

    fn text(&self) -> String {
        self.loro.get_text("text").to_string()
    }

    /// Applies what came and keeps the other events; every event seen so far.
    fn pump(&self) -> Vec<Event> {
        let mut seen = self.seen.lock().unwrap();
        while let Some(event) = self.doc.try_recv() {
            match event {
                Event::Update(bytes) => {
                    self.loro.import(&bytes).unwrap();
                }
                other => seen.push(other),
            }
        }
        seen.clone()
    }

    /// Types at the end and sends it: its number.
    fn append(&self, s: &str) -> u64 {
        let before = self.loro.oplog_vv();
        let t = self.loro.get_text("text");
        t.insert(t.len_unicode(), s).unwrap();
        self.loro.commit();
        self.doc.send(&self.loro.export(ExportMode::updates(&before)).unwrap()).unwrap()
    }

    /// Types at the end and hands the change to the agent as a held one (an app that ends with it unsent).
    fn append_held(&self, s: &str) -> u64 {
        let before = self.loro.oplog_vv();
        let t = self.loro.get_text("text");
        t.insert(t.len_unicode(), s).unwrap();
        self.loro.commit();
        self.doc.hold(&self.loro.export(ExportMode::updates(&before)).unwrap()).unwrap()
    }

    /// Types at the end and sends the whole history (for a document whose earlier changes the agent refused).
    fn append_all(&self, s: &str) -> u64 {
        let t = self.loro.get_text("text");
        t.insert(t.len_unicode(), s).unwrap();
        self.loro.commit();
        self.doc.send(&self.loro.export(ExportMode::all_updates()).unwrap()).unwrap()
    }
}

/// Opens `path` as an app would, importing `content` if the file has never been live.
fn open_doc(docs: &Documents, path: &Path, content: &str, identity: bool) -> Opened {
    let options = OpenOptions { identity, ..OpenOptions::default() };
    match docs.open_with(&path.display().to_string(), "wordcraft", &options).unwrap() {
        Opened::NeedsImport(importer) => Opened::Live(importer.import(&text_snapshot(content)).unwrap()),
        other => other,
    }
}

fn live(opened: Opened) -> Document {
    match opened {
        Opened::Live(doc) => doc,
        other => panic!("not live: {other:?}"),
    }
}

/// An app on `person`'s computer with `path` open (by its absolute path): its connection and its document.
async fn app_opens(person: &Person, path: PathBuf, content: &'static str, identity: bool, peer: u64) -> (Documents, AppDoc) {
    let client = person.client();
    off(move || {
        let docs = client.documents().unwrap();
        let doc = live(open_doc(&docs, &path, content, identity));
        (docs, AppDoc::new(doc, peer))
    })
    .await
}

/// A team where ana and ben both sync the space and `plan.txt` is in both folders.
async fn team_with_plan(role: SpaceRole) -> (Team, PathBuf, PathBuf) {
    let t = team(role).await;
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    write(&ana_folder, "plan.txt", "the plan");
    eventually("ben has the plan", || has_file(ben_folder.clone(), "plan.txt", "the plan")).await;
    (t, ana_folder, ben_folder)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_document_in_a_team_folder_is_live_across_accounts_and_identity_is_only_for_apps_that_ask() {
    let (t, ana_folder, ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let space = t.space;
    // Ana's app asks for identity and opens first (the file has never been live: it imports it); ben's does not ask.
    let (_docs_ana, ana) = app_opens(&t.ana, ana_folder.join("plan.txt"), "the plan", true, 11).await;
    let (_docs_ben, ben) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 12).await;
    assert_eq!(ana.text(), "the plan");
    assert_eq!(ben.text(), "the plan");
    // The document says which space it is in, and that it may be changed.
    assert_eq!((ana.doc.space(), ana.doc.read_only()), (Some(space), false));
    assert_eq!((ben.doc.space(), ben.doc.read_only()), (Some(space), false));

    // A change goes across the accounts, both ways.
    let seq = ben.append(", edited by ben");
    eventually("ben's change is acked", || async { ben.pump().contains(&Event::Acked(seq)) }).await;
    eventually("ana shows it", || async {
        ana.pump();
        ana.text() == "the plan, edited by ben"
    })
    .await;
    ana.append(" and ana");
    eventually("ben shows it", || async {
        ben.pump();
        ben.text() == "the plan, edited by ben and ana"
    })
    .await;

    // Presence: ana's app asked for identity and hears who is there, stamped by the server; ben's did not ask and hears kind 3
    // only (no identity, and no member list).
    ben.doc.presence(b"ben's cursor").unwrap();
    ana.doc.presence(b"ana's cursor").unwrap();
    let ben_id = t.world.ben.id.0;
    eventually("ana hears ben, with his account", || async {
        ana.pump().contains(&Event::PresenceFrom { user: ben_id, bytes: b"ben's cursor".to_vec() })
    })
    .await;
    eventually("ben hears ana's presence as plain presence", || async { ben.pump().contains(&Event::Presence(b"ana's cursor".to_vec())) }).await;
    let heard_by_ben = ben.pump();
    assert!(
        !heard_by_ben.iter().any(|e| matches!(e, Event::PresenceFrom { .. } | Event::Members(_))),
        "an app that does not ask for identity gets neither identity nor names: {heard_by_ben:?}"
    );
    assert!(!ana.pump().iter().any(|e| matches!(e, Event::Presence(_))), "an app that asked never gets the plain kind");

    // Ana's app is told who is in the space: names and palette colours, itself marked.
    eventually("ana is told the members", || async { ana.pump().iter().any(|e| matches!(e, Event::Members(m) if m.len() == 2)) }).await;
    let members = ana.pump().into_iter().rev().find_map(|e| if let Event::Members(m) = e { Some(m) } else { None }).unwrap();
    let mut names: Vec<(&str, bool)> = members.iter().map(|m| (m.name.as_str(), m.is_self)).collect();
    names.sort_unstable();
    assert_eq!(names, vec![("ana", true), ("ben", false)]);
    for member in &members {
        assert_eq!(member.colour, craft_cloud_proto::colour::member_colour(member.user), "the palette index every machine derives");
    }
    assert!(members.iter().any(|m| m.user == ben_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_viewer_opens_read_only_and_what_it_types_is_neither_kept_nor_sent() {
    let (t, ana_folder, ben_folder) = team_with_plan(SpaceRole::Viewer).await;
    let (_docs_ana, ana) = app_opens(&t.ana, ana_folder.join("plan.txt"), "the plan", false, 11).await;
    let (docs_ben, ben) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 12).await;
    assert_eq!((ben.doc.read_only(), ben.doc.space()), (true, Some(t.space)));
    assert!(!ana.doc.read_only());
    assert_eq!(ben.text(), "the plan", "a viewer reads the document");

    // What the viewer types is refused with `read_only`: never acked, and the agent's copy is as it was.
    let seq = ben.append(", typed by the viewer");
    eventually("the agent refuses the change", || async {
        ben.pump().iter().any(|e| matches!(e, Event::Refused { seq: s, code, .. } if *s == seq && code == "read_only"))
    })
    .await;
    assert!(!ben.pump().contains(&Event::Acked(seq)));
    tokio::time::sleep(Duration::from_secs(2)).await;
    ana.pump();
    assert_eq!(ana.text(), "the plan", "nothing of it reached the server");
    drop(docs_ben);
    let (_docs_again, again) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 13).await;
    assert_eq!(again.text(), "the plan", "the agent's copy is unchanged");
    assert_eq!(again.doc.opened_state().waiting, 0, "and nothing waits to be sent");

    // The editor's changes still reach the viewer.
    ana.append(" - by ana");
    eventually("the viewer shows the editor's change", || async {
        again.pump();
        again.text() == "the plan - by ana"
    })
    .await;

    // A blob for a file of a space the account may not change is refused.
    let path = ben_folder.join("plan.txt").display().to_string();
    let refused = t.ben.api(move |c| c.blobs().put_for(b"image bytes", &path)).await.unwrap_err();
    assert!(matches!(&refused, craft_cloud_app::Error::Refused { code, .. } if code == "read_only"), "{refused:?}");

    // So is a recents entry for a file of the space: it could never go up, so it is not kept to be acked.
    let path = ben_folder.join("plan.txt").display().to_string();
    let refused = t.ben.api(move |c| c.add_recent("wordcraft", NewRecent { path, title: None, saved: false })).await.unwrap_err();
    assert!(matches!(&refused, craft_cloud_app::Error::Refused { code, .. } if code == "read_only"), "{refused:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_role_changed_while_connected_is_acted_on_in_both_directions() {
    let (t, ana_folder, ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let (space, ben_id) = (t.space, t.world.ben.id);
    let (_docs_ana, ana) = app_opens(&t.ana, ana_folder.join("plan.txt"), "the plan", false, 11).await;
    let (_docs_ben, ben) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 12).await;
    let seq = ben.append(", as an editor");
    eventually("the editor's change is acked", || async { ben.pump().contains(&Event::Acked(seq)) }).await;
    eventually("ana shows it", || async {
        ana.pump();
        ana.text() == "the plan, as an editor"
    })
    .await;

    // Lowered to viewer while connected: his next changes are refused as soon as the agent knows (the server ends his
    // connection, or the agent's list of spaces says so).
    t.world.meta.change_member_role(&SpaceActor::Cli, space, ben_id, SpaceRole::Viewer, 1).await.unwrap();
    let (app, tries) = (&ben, std::cell::Cell::new(0u32));
    let tries = &tries;
    eventually("a change of the lowered account is refused", || {
        tries.set(tries.get() + 1);
        let seq = app.append(&format!(" [{}]", tries.get()));
        async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            app.pump().iter().any(|e| matches!(e, Event::Refused { seq: s, code, .. } if *s == seq && code == "read_only"))
        }
    })
    .await;
    eventually("ben's space is read-only on his computer", || async { t.ben.space(space).await.is_some_and(|s| s.read_only) }).await;

    // Raised again: changes are taken again. (The refused ones were never in the agent's copy; the app sends its whole history.)
    t.world.meta.change_member_role(&SpaceActor::Cli, space, ben_id, SpaceRole::Editor, 2).await.unwrap();
    let n = std::cell::Cell::new(0u32);
    let n = &n;
    eventually("a change of the raised account is taken", || {
        n.set(n.get() + 1);
        let seq = app.append_all(&format!(" <{}>", n.get()));
        async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            app.pump().contains(&Event::Acked(seq))
        }
    })
    .await;
    let last = n.get();
    eventually("ana shows what the raised editor typed", || async {
        ana.pump();
        ana.text().contains(&format!("<{last}>"))
    })
    .await;
}

/// The agent tells every open document of a space when the account's right to change it changes, both ways, with no new
/// open. A viewer's app that ends with changes it could not send hands them to the agent: they are kept, never sent while
/// the account is a viewer, and go up when the role is raised, with no app open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_role_is_pushed_to_open_documents_and_what_a_viewers_app_hands_over_goes_up_when_it_may_write() {
    let (t, ana_folder, ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let (space, ben_id) = (t.space, t.world.ben.id);
    let (_docs_ana, ana) = app_opens(&t.ana, ana_folder.join("plan.txt"), "the plan", false, 11).await;
    let (docs_ben, ben) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 12).await;
    assert!(!ben.doc.read_only());

    // Lowered: the open document is told, and it did not try anything.
    t.world.meta.change_member_role(&SpaceActor::Cli, space, ben_id, SpaceRole::Viewer, 1).await.unwrap();
    eventually("the open document is told it is read-only", || async { ben.pump().contains(&Event::Role { read_only: true }) }).await;

    // The app ends with a change it could not send. The agent keeps it and, as long as the account is a viewer, never sends it.
    let first = ben.append_held(", written as a viewer");
    let second = ben.append_held(" and again");
    // The agent says it has them (an app that ends waits for that), then the document's close, then the socket's.
    eventually("the agent acks the held changes", || async {
        let seen = ben.pump();
        seen.contains(&Event::Acked(first)) && seen.contains(&Event::Acked(second))
    })
    .await;
    drop(ben);
    drop(docs_ben);
    tokio::time::sleep(Duration::from_secs(2)).await;
    ana.pump();
    assert_eq!(ana.text(), "the plan", "nothing of it reached the server");

    // Raised with no app open: the held change goes up.
    t.world.meta.change_member_role(&SpaceActor::Cli, space, ben_id, SpaceRole::Editor, 2).await.unwrap();
    eventually("ana shows what the viewer's app handed over", || async {
        ana.pump();
        ana.text() == "the plan, written as a viewer and again"
    })
    .await;

    // Opened again, the document is writable, and the told role follows the next change too.
    let (_docs_again, again) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 13).await;
    assert!(!again.doc.read_only());
    assert_eq!(again.text(), "the plan, written as a viewer and again");
    t.world.meta.change_member_role(&SpaceActor::Cli, space, ben_id, SpaceRole::Viewer, 3).await.unwrap();
    eventually("the second document is told too", || async { again.pump().contains(&Event::Role { read_only: true }) }).await;
}

/// What a viewer's app handed over is in the document's journal: it survives a restart of the agent while the account is
/// still a viewer (nothing is sent), and goes up once the role is raised.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_changes_survive_a_restart_of_the_agent_and_go_up_when_the_role_is_raised() {
    let (mut t, ana_folder, ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let (space, ben_id) = (t.space, t.world.ben.id);
    let (_docs_ana, ana) = app_opens(&t.ana, ana_folder.join("plan.txt"), "the plan", false, 11).await;
    let (docs_ben, ben) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 12).await;
    t.world.meta.change_member_role(&SpaceActor::Cli, space, ben_id, SpaceRole::Viewer, 1).await.unwrap();
    eventually("the open document is told it is read-only", || async { ben.pump().contains(&Event::Role { read_only: true }) }).await;
    let seq = ben.append_held(", kept across a restart");
    eventually("the agent acks the held change", || async { ben.pump().contains(&Event::Acked(seq)) }).await;
    drop(ben);
    drop(docs_ben);

    // Ben's agent is stopped and started again while he is still a viewer: the held change is in the journal, and is not sent.
    let (exit, out) = t.ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    t.ben.run();
    tokio::time::sleep(Duration::from_secs(3)).await;
    ana.pump();
    assert_eq!(ana.text(), "the plan", "a viewer's held change is not sent");
    let (_docs_again, again) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 13).await;
    assert_eq!(again.text(), "the plan, kept across a restart", "the journal brought it back");

    // Raised: it goes up.
    t.world.meta.change_member_role(&SpaceActor::Cli, space, ben_id, SpaceRole::Editor, 2).await.unwrap();
    eventually("ana shows the held change", || async {
        ana.pump();
        ana.text() == "the plan, kept across a restart"
    })
    .await;
}

/// Journals with unacked updates survive a revocation: removed while offline with a change that was never acked, the account
/// keeps it on disk; invited again as a viewer it is reported and not sent, and once the account may edit again it goes up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_waiting_in_a_journal_survives_a_revocation_and_goes_up_when_the_account_may_edit_again() {
    let (t, ana_folder, ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let (space, ben_id) = (t.space, t.world.ben.id);
    let (_docs_ana, ana) = app_opens(&t.ana, ana_folder.join("plan.txt"), "the plan", false, 11).await;
    let (docs_ben, ben) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 12).await;

    // Ben's agent is held (paused): offline, it keeps his change in the document's journal without a server to ack it.
    t.ben.api(|c| c.pause()).await.unwrap();
    eventually("ben's document is offline", || async { ben.pump().iter().any(|e| matches!(e, Event::Status { online: false, .. })) }).await;
    let seq = ben.append(", written offline");
    eventually("the agent keeps it on its disk", || async { ben.pump().contains(&Event::Acked(seq)) }).await;
    let live_dir = t.ben.data.path().join("live").join(space.to_string());
    // What the agent keeps of the space's documents: their files and journals (the hub writes a document whole, and empties its
    // journal, when it stops; the unacked change is in either).
    let kept = || -> usize {
        std::fs::read_dir(&live_dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        name != "saves.json" && e.metadata().is_ok_and(|m| m.is_file() && m.len() > 0)
                    })
                    .count()
            })
            .unwrap_or(0)
    };
    assert!(kept() >= 1, "the unacked change is on disk");

    // Ben is removed while his change waits; the agent resumes, finds it has no access and lets the space go.
    t.world.meta.remove_member(&SpaceActor::Cli, space, ben_id, 1).await.unwrap();
    t.ben.api(|c| c.resume()).await.unwrap();
    eventually("the space reports no access", || async {
        t.ben.status().await.spaces.iter().any(|s| s.space == space && matches!(s.run_state, SpaceRun::NoAccess { .. }))
    })
    .await;
    drop((docs_ben, ben));
    assert!(kept() >= 1, "the document and its unacked change are kept through the revocation");
    tokio::time::sleep(Duration::from_secs(1)).await;
    ana.pump();
    assert_eq!(ana.text(), "the plan", "nothing of it reached the server");

    // Invited again, as a viewer: the change is reported (the document opens with it waiting) and is not sent.
    t.world.invite_ben(space, SpaceRole::Viewer).await;
    eventually("the invitation is listed again", || async { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(space).await;
    eventually("the space syncs again, read-only", || async { t.ben.space(space).await.is_some_and(|s| s.read_only) }).await;
    let (_docs_again, again) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 13).await;
    assert!(again.doc.read_only());
    assert_eq!(again.doc.opened_state().waiting, 1, "the change made offline is reported as waiting");
    assert!(again.text().ends_with(", written offline"), "and it is in the agent's copy: {:?}", again.text());
    tokio::time::sleep(Duration::from_secs(2)).await;
    ana.pump();
    assert_eq!(ana.text(), "the plan", "a viewer's connection never sends it");
    assert!(kept() >= 1);

    // Made an editor again: it goes up.
    t.world.meta.change_member_role(&SpaceActor::Cli, space, ben_id, SpaceRole::Editor, 2).await.unwrap();
    eventually("the change made offline reaches ana", || async {
        ana.pump();
        ana.text() == "the plan, written offline"
    })
    .await;
    eventually("nothing waits any more", || async { again.pump().iter().any(|e| matches!(e, Event::Status { waiting: 0, .. })) }).await;
    // And the app's own changes are taken again.
    let seq = again.append(", and edited on");
    eventually("the app's change is acked", || async { again.pump().contains(&Event::Acked(seq)) }).await;
    eventually("it reaches ana", || async {
        ana.pump();
        ana.text() == "the plan, written offline, and edited on"
    })
    .await;
}

/// A space's live folder is one hub's at a time: sync turned off and on while changes wait in a journal (the old hub is still
/// closing when the space is turned on again) loses none of them, also across a restart of the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_a_space_off_and_on_with_changes_waiting_loses_none() {
    let (mut t, ana_folder, ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let space = t.space;
    let (_docs_ana, ana) = app_opens(&t.ana, ana_folder.join("plan.txt"), "the plan", false, 11).await;
    let (docs_ben, ben) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 12).await;
    t.ben.api(|c| c.pause()).await.unwrap();
    eventually("ben's document is offline", || async { ben.pump().iter().any(|e| matches!(e, Event::Status { online: false, .. })) }).await;
    for n in 1..=3 {
        let seq = ben.append(&format!(" [{n}]"));
        eventually("the agent keeps the change", || async { ben.pump().contains(&Event::Acked(seq)) }).await;
    }
    drop((docs_ben, ben));

    // Off and at once on again, in the same folder, and the hub of the first run is still ending.
    t.ben.sync_off(space).await;
    t.ben.sync_on_in(space, &ben_folder).await;
    t.ben.api(|c| c.resume()).await.unwrap();
    let (_docs_again, again) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 13).await;
    let seq = again.append(" [4]");
    eventually("the new hub takes a change", || async { again.pump().contains(&Event::Acked(seq)) }).await;

    // The agent is stopped and started again (what was acked and not yet written is either in the journal or in the document).
    let (exit, out) = t.ben.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    t.ben.run();
    eventually("every change reaches ana", || async {
        ana.pump();
        let text = ana.text();
        (1..=4).all(|n| text.contains(&format!("[{n}]")))
    })
    .await;
}

/// An app with documents of two spaces open is told when one space stops syncing: the document is `stopped`, a change to it is
/// refused with `not_found` (not dropped), and the other space's document goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_app_is_told_when_the_space_of_an_open_document_stops_syncing() {
    let (t, ana_folder, ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let space = t.space;
    write(&t.ben.personal, "mine.txt", "my own");
    let (_docs_ana, _ana) = app_opens(&t.ana, ana_folder.join("plan.txt"), "the plan", false, 11).await;
    let client = t.ben.client();
    let (team_path, personal_path) = (ben_folder.join("plan.txt"), t.ben.personal.join("mine.txt"));
    let (docs_ben, team_doc, personal_doc) = off(move || {
        let docs = client.documents().unwrap();
        let team = AppDoc::new(live(open_doc(&docs, &team_path, "unused", false)), 21);
        let personal = AppDoc::new(live(open_doc(&docs, &personal_path, "my own", false)), 22);
        (docs, team, personal)
    })
    .await;
    t.ben.sync_off(space).await;
    eventually("the team document is told it is stopped", || async {
        team_doc.pump().iter().any(|e| matches!(e, Event::Status { stopped: true, .. }))
    })
    .await;
    let seq = team_doc.append(", typed after the space stopped");
    eventually("the change is refused, not dropped", || async {
        team_doc.pump().iter().any(|e| matches!(e, Event::Refused { seq: s, code, .. } if *s == seq && code == "not_found"))
    })
    .await;
    // The personal document is not affected.
    let seq = personal_doc.append(" + more");
    eventually("the personal document takes changes", || async { personal_doc.pump().contains(&Event::Acked(seq)) }).await;
    drop(docs_ben);
}

/// Changes an app sends while the space is being turned off are each either kept (acked) or refused with `not_found`: none is
/// dropped without an answer. (After the first refusal the app has been told the space stopped, and the connection lets
/// further changes of its documents be.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changes_sent_while_a_space_is_turned_off_are_each_answered() {
    const SENT: u64 = 400;
    let (t, _ana_folder, ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let space = t.space;
    let client = t.ben.client();
    let path = ben_folder.join("plan.txt");
    let (docs_ben, team_doc) = off(move || {
        let docs = client.documents().unwrap();
        let doc = AppDoc::new(live(open_doc(&docs, &path, "unused", false)), 21);
        (docs, doc)
    })
    .await;
    // The app types as fast as it can, from a thread of its own, while the space is turned off.
    let typing = tokio::task::spawn_blocking(move || {
        for n in 0..SENT {
            team_doc.append(&format!(" {n}"));
        }
        team_doc
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    t.ben.sync_off(space).await;
    let team_doc = typing.await.unwrap();
    eventually("every change sent before the first refusal is answered", || async {
        let events = team_doc.pump();
        let refused = events
            .iter()
            .filter_map(|e| match e {
                Event::Refused { seq, code, .. } if code == "not_found" => Some(*seq),
                _ => None,
            })
            .min();
        let upto = refused.unwrap_or(SENT + 1);
        (1..upto).all(|seq| events.iter().filter(|e| **e == Event::Acked(seq)).count() == 1)
    })
    .await;
    drop(docs_ben);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_team_spaces_at_once_each_have_their_own_link_and_documents_are_routed_by_path() {
    let t = team(SpaceRole::Editor).await;
    let second = t.world.space("Archive").await;
    t.world.invite_ben(second, SpaceRole::Editor).await;
    let (first_ana, first_ben) = both_syncing(&t).await;
    eventually("the second invitation is listed", || async { t.ben.space(second).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(second).await;
    eventually("ana lists the second space", || async { t.ana.space(second).await.is_some_and(|s| s.state == "active") }).await;
    let (second_ana, second_ben) = (t.ana.sync_on(second).await, t.ben.sync_on(second).await);
    assert_ne!(first_ben, second_ben);
    write(&first_ana, "plan.txt", "plan one");
    write(&second_ana, "plan.txt", "plan two");
    eventually("ben has both plans", || async {
        read(&first_ben, "plan.txt").as_deref() == Some("plan one") && read(&second_ben, "plan.txt").as_deref() == Some("plan two")
    })
    .await;

    // One connection of ben's app opens a document of each space (and the personal space is untouched).
    let (_a1, ana_one) = app_opens(&t.ana, first_ana.join("plan.txt"), "plan one", false, 11).await;
    let (_a2, ana_two) = app_opens(&t.ana, second_ana.join("plan.txt"), "plan two", false, 12).await;
    let client = t.ben.client();
    let (one_path, two_path) = (first_ben.join("plan.txt"), second_ben.join("plan.txt"));
    let (docs_ben, ben_one, ben_two) = off(move || {
        let docs = client.documents().unwrap();
        let one = AppDoc::new(live(open_doc(&docs, &one_path, "unused", false)), 21);
        let two = AppDoc::new(live(open_doc(&docs, &two_path, "unused", false)), 22);
        (docs, one, two)
    })
    .await;
    assert_eq!((ben_one.doc.space(), ben_two.doc.space()), (Some(t.space), Some(second)));
    assert_ne!(ben_one.doc.id(), ben_two.doc.id());
    assert_eq!((ben_one.text(), ben_two.text()), ("plan one".to_owned(), "plan two".to_owned()));

    // Each document's changes go to its own space.
    ben_one.append(" + one");
    ben_two.append(" + two");
    eventually("each reaches ana in its own space", || async {
        ana_one.pump();
        ana_two.pump();
        ana_one.text() == "plan one + one" && ana_two.text() == "plan two + two"
    })
    .await;
    // A document of a space that does not sync here, or a path in no synced folder, is not live.
    let outside = t.ben.dir.path().join("elsewhere.txt");
    std::fs::write(&outside, "x").unwrap();
    let docs_ben = off(move || {
        match open_doc(&docs_ben, &outside, "x", false) {
            Opened::NotLive(reason) => assert_eq!(reason, NotLiveReason::NotInFolder),
            other => panic!("{other:?}"),
        }
        docs_ben
    })
    .await;
    drop(docs_ben);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blob_put_for_a_team_file_lands_in_the_space_and_another_members_agent_reads_it() {
    let (t, ana_folder, ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let (ana_path, ben_path) = (ana_folder.join("plan.txt").display().to_string(), ben_folder.join("plan.txt").display().to_string());

    let (put, path) = (bytes.clone(), ana_path.clone());
    let stored = t.ana.api(move |c| c.blobs().put_for(&put, &path)).await.unwrap();
    assert_eq!(stored.len, bytes.len() as u64);
    // Ben's agent reads it from the space (it goes up in the background, so it may take a moment).
    let (hash, len, path) = (stored.hash, stored.len, ben_path.clone());
    let got = Mutex::new(None);
    let got = &got;
    eventually("ben's agent gets the blob from the team space", || {
        let (client, path) = (t.ben.client(), path.clone());
        async move {
            let answer = off(move || client.blobs().get_for(&hash, len, &path)).await;
            if let Ok(found) = answer {
                *got.lock().unwrap() = Some(found);
            }
            got.lock().unwrap().is_some()
        }
    })
    .await;
    assert_eq!(got.lock().unwrap().as_deref(), Some(bytes.as_slice()));

    // It is in the space, not in the personal space: without `for`, ben's agent does not have it.
    let missing = t.ben.api(move |c| c.blobs().get(&hash, len)).await.unwrap_err();
    assert!(matches!(&missing, craft_cloud_app::Error::Refused { code, .. } if code == "blob_not_found"), "{missing:?}");
    // And a path in no synced folder is refused.
    let outside = t.ben.dir.path().join("elsewhere.txt").display().to_string();
    let refused = t.ben.api(move |c| c.blobs().put_for(b"x", &outside)).await.unwrap_err();
    assert!(matches!(&refused, craft_cloud_app::Error::Refused { code, .. } if code == "invalid"), "{refused:?}");
    // A path that climbs out of the team folder is not the team space's, whatever it starts with.
    let climbing = ben_folder.join("..").join("elsewhere.txt").display().to_string();
    let refused = t.ben.api(move |c| c.blobs().put_for(b"x", &climbing)).await.unwrap_err();
    assert!(matches!(&refused, craft_cloud_app::Error::Refused { code, .. } if code == "invalid"), "{refused:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_recents_of_a_team_space_reach_the_other_members_and_not_the_personal_lists() {
    let (t, ana_folder, _ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let space = t.space;
    // Ben follows the space's list; ana adds a file of the team folder (an absolute path) to hers.
    let client = t.ben.client();
    let watch = off(move || client.watch_recents_in("wordcraft", space).unwrap()).await;
    assert!(watch.entries().is_empty());
    let path = ana_folder.join("plan.txt").display().to_string();
    let entry = t.ana.api(move |c| c.add_recent("wordcraft", NewRecent { path, title: Some("The plan".into()), saved: true })).await.unwrap();
    assert_eq!(entry.path, "plan.txt", "kept relative to the team folder");
    eventually("the entry reaches ben", || async { watch.entries().iter().any(|e| e.path == "plan.txt" && e.title == "The plan") }).await;
    // Ana's own list of the space has it too; neither personal list does.
    let client = t.ana.client();
    let own = off(move || client.watch_recents_in("wordcraft", space).unwrap()).await;
    assert!(own.entries().iter().any(|e| e.path == "plan.txt"));
    assert!(t.ana.api(|c| c.recents("wordcraft")).await.unwrap().is_empty(), "not in ana's personal list");
    assert!(t.ben.api(|c| c.recents("wordcraft")).await.unwrap().is_empty(), "nor in ben's");
    // A space that does not sync here has no list to follow.
    let other = SpaceId::generate();
    let missing = t.ben.api(move |c| c.watch_recents_in("wordcraft", other).map(|_| ())).await.unwrap_err();
    assert!(matches!(&missing, craft_cloud_app::Error::Refused { code, .. } if code == "not_found"), "{missing:?}");
}

/// The recent files of a team space say who else has a file open: the accounts whose presence the agent hears for a document an
/// app on this computer has open, never the account itself, and only while they stay there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recent_file_of_a_team_space_says_who_else_has_it_open_and_never_the_account_itself() {
    let (t, ana_folder, ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let space = t.space;
    let (_docs_ana, ana) = app_opens(&t.ana, ana_folder.join("plan.txt"), "the plan", false, 11).await;
    let (_docs_ben, ben) = app_opens(&t.ben, ben_folder.join("plan.txt"), "unused", false, 12).await;
    let path = ana_folder.join("plan.txt").display().to_string();
    let entry = t.ana.api(move |c| c.add_recent("wordcraft", NewRecent { path, title: None, saved: false })).await.unwrap();
    assert_eq!(entry.path, "plan.txt");
    assert!(entry.open_now.is_empty(), "an answer to an add says nobody");

    // Nobody has sent presence yet: nobody is there.
    let own = t.ana.api(move |c| c.recents_in("wordcraft", space)).await.unwrap();
    assert!(own.iter().all(|e| e.open_now.is_empty()), "{own:?}");

    // Ben's app shows presence (and ana's too, which never comes back to ana): ana's list names ben, ben's list names ana.
    let names = |list: Vec<craft_cloud_app::RecentEntry>| -> Vec<String> {
        list.into_iter().find(|e| e.path == "plan.txt").map(|e| e.open_now.into_iter().map(|p| p.name).collect()).unwrap_or_default()
    };
    eventually("ana's list names ben", || async {
        ben.doc.presence(b"ben's cursor").unwrap();
        ana.doc.presence(b"ana's cursor").unwrap();
        names(t.ana.api(move |c| c.recents_in("wordcraft", space)).await.unwrap()) == ["ben"]
    })
    .await;
    eventually("ben's list names ana", || async {
        ben.doc.presence(b"ben's cursor").unwrap();
        ana.doc.presence(b"ana's cursor").unwrap();
        names(t.ben.api(move |c| c.recents_in("wordcraft", space)).await.unwrap()) == ["ana"]
    })
    .await;
    let colour = t.ana.api(move |c| c.recents_in("wordcraft", space)).await.unwrap().into_iter().find(|e| e.path == "plan.txt").unwrap().open_now;
    assert_eq!(colour[0].colour, craft_cloud_proto::colour::member_colour(t.world.ben.id.0), "the palette index every machine derives");

    // The personal lists say nothing of them.
    assert!(t.ana.api(|c| c.recents("wordcraft")).await.unwrap().is_empty());

    // A follower of the list is told when ben fades, with no new presence and no new look: ana follows the space's list, ben
    // shows presence once more, and then nothing is sent for ten seconds.
    let client = t.ana.client();
    let follower = off(move || client.watch_recents_in("wordcraft", space).unwrap()).await;
    ben.doc.presence(b"ben's cursor").unwrap();
    eventually("the follower is told ben is there", || async { names(follower.entries()) == ["ben"] }).await;
    eventually("the follower is told ben has faded", || async { names(follower.entries()).is_empty() }).await;
    // The same through a call that follows once more: nobody is there.
    assert!(names(t.ana.api(move |c| c.recents_in("wordcraft", space)).await.unwrap()).is_empty());
}

/// The lists of several apps and spaces in one connection: each at its own app and space, the personal list apart, a space that
/// does not sync here refused for that list alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn several_recents_lists_come_over_one_connection() {
    let (t, ana_folder, _ben_folder) = team_with_plan(SpaceRole::Editor).await;
    let space = t.space;
    let path = ana_folder.join("plan.txt").display().to_string();
    t.ana.api(move |c| c.add_recent("wordcraft", NewRecent { path, title: Some("Team plan".into()), saved: false })).await.unwrap();
    t.ana.api(|c| c.add_recent("wordcraft", NewRecent { path: "personal.txt".into(), title: Some("Mine".into()), saved: false })).await.unwrap();
    let other = SpaceId::generate();
    let wanted: Vec<(String, Option<SpaceId>)> =
        vec![("wordcraft".into(), None), ("wordcraft".into(), Some(space)), ("photocraft".into(), Some(space)), ("wordcraft".into(), Some(other))];
    let answers = t.ana.api(move |c| c.recents_of(&wanted)).await.unwrap();
    assert_eq!(answers.len(), 4);
    let list = |i: usize| answers[i].2.as_ref().map(|l| l.iter().map(|e| e.title.clone()).collect::<Vec<_>>());
    assert_eq!(list(0).unwrap(), ["Mine"], "the personal list is not the space's");
    assert_eq!(list(1).unwrap(), ["Team plan"]);
    assert_eq!(list(2).unwrap(), Vec::<String>::new(), "an app with no files has an empty list");
    assert!(matches!(&answers[3].2, Err(craft_cloud_app::Error::Refused { code, .. }) if code == "not_found"), "{:?}", answers[3].2);
    assert_eq!((answers[1].0.as_str(), answers[1].1), ("wordcraft", Some(space)));
}

/// A server whose live service does not serve team spaces (it answers the upgrade with `404`) is asked once and then left alone
/// for a long pause: no loop. The space's files sync on, and its documents open as plain files.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_that_cannot_serve_the_team_link_is_asked_once_and_documents_open_as_plain_files() {
    let (t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    front.live_404.store(true, Ordering::SeqCst);
    let (ana_folder, ben_folder) = both_syncing(&t).await;
    write(&ana_folder, "plan.txt", "the plan");
    eventually("ben has the plan", || has_file(ben_folder.clone(), "plan.txt", "the plan")).await;
    // His link is refused; it is not dialed again within seconds, and the file sync is unaffected.
    eventually("the link was asked for", || async { front.seen.lock().unwrap().iter().any(|(p, named)| p == "/api/v1/live" && *named) }).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let asked = front.seen.lock().unwrap().iter().filter(|(p, named)| p == "/api/v1/live" && *named).count();
    assert!(asked <= 2, "the team link was asked for {asked} times in a few seconds");
    write(&ana_folder, "later.txt", "still syncs");
    eventually("the file sync goes on", || has_file(ben_folder.clone(), "later.txt", "still syncs")).await;
    let client = t.ben.client();
    let path = ben_folder.join("plan.txt");
    let opened = off(move || open_doc(&client.documents().unwrap(), &path, "x", false)).await;
    assert!(matches!(opened, Opened::NotLive(NotLiveReason::Offline)), "{opened:?}");
}

/// One Ctrl-C with an idle personal space and a team pass that hangs: the loop waits for the pass, and a second Ctrl-C, read
/// meanwhile, ends the wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_ctrl_c_is_read_while_the_first_waits_for_a_hung_team_pass() {
    let (mut t, front, _task) = team_via_front(SpaceRole::Editor, Front::default()).await;
    let (_, ben_folder) = both_syncing(&t).await;
    write(&ben_folder, "first.txt", "first");
    eventually("the first file went up", || async { t.ben.status().await.spaces.iter().any(|s| s.last_sync_ms.is_some()) }).await;

    front.stall.store(true, Ordering::SeqCst);
    write(&ben_folder, "stuck.txt", "never answered");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut running = t.ben.running.take().unwrap();
    let thread = running.thread.take().unwrap();
    // The first stop: the personal space is idle, so the loop ends at once and waits for the team pass that hangs.
    let _ = running.stop.send(());
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!thread.is_finished(), "the first stop waits for the pass under way");
    // The second stop is read while it waits, and ends it.
    let _ = running.stop.send(());
    let started = Instant::now();
    let ended = tokio::time::timeout(Duration::from_secs(8), tokio::task::spawn_blocking(move || thread.join().unwrap())).await;
    front.stall.store(false, Ordering::SeqCst);
    let (exit, out) = ended.expect("the second stop ends the wait within seconds").unwrap();
    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(out.contains("stopping at once"), "{out}");
}
