//! The agent with several spaces, against a real server on a loopback port: an invitation reaches the local API, is
//! accepted there, the team folder appears beside the personal one and syncs both ways between two accounts; a role
//! lowered to viewer makes the folder read-only; a removed member keeps the local copy, which stops syncing and is reported;
//! a renamed space keeps its folder; folder names clash and are made safe; an answer without the space's echo is refused;
//! an old server shows no team spaces and is never sent the `craft-space` header; and a Phase 1 data folder starts unchanged.
//! Every wait is for a condition, with a timeout; the loops get short timings so the tests stay quick.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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

/// Ben accepts, and both turn the space's sync on. Returns ana's and ben's team folders.
async fn both_syncing(t: &Team) -> (PathBuf, PathBuf) {
    let space = t.space;
    eventually("the invitation is listed", || async move { t.ben.space(space).await.is_some_and(|s| s.state == "invited") }).await;
    t.ben.accept(space).await;
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
    front.seen.lock().unwrap().push((path.clone(), parts.headers.contains_key(&space_header)));
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
