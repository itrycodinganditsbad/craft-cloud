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
use craft_cloud_app::{Client, SpaceInfo, SpaceList, SpaceRole, SpaceRun, SpaceSync, SpaceSyncRequest};
use craft_cloud_meta::{Meta, Role, SpaceActor, UserRecord};
use craft_cloud_proto::{SpaceId, SpaceName};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_sync::Transport;
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
    let last = t.ana.api(move |c| c.leave_space(space)).await.unwrap_err();
    assert!(matches!(&last, craft_cloud_app::Error::Refused { code, .. } if code == "last_owner"), "{last:?}");
}
