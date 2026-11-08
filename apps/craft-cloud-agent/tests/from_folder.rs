//! "Make a team space from this folder" against a real server on a loopback port: the happy path with no byte uploaded again (a
//! counting layer in front of the server counts the chunks it is sent), the folder's checks, the administrator's confirmation, a
//! claim the server refuses (the files are uploaded), a failure and a crash at each step (the old folder is untouched, the half-made
//! team space is reported and gone on with, never made twice), a file that changes under the copy, and the mass-delete guard
//! seeing the old folder go.
//! Every wait is for a condition, with a timeout; the loops get short timings so the tests stay quick.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use craft_cloud_agent::account::DataDir;
use craft_cloud_agent::cli::{self, AgentError, Exit, LoginArgs};
use craft_cloud_agent::daemon::{RunOptions, run_loop};
use craft_cloud_agent::from_folder::hook::{self, Hook};
use craft_cloud_agent::schedule::Timing;
use craft_cloud_app::{Client, FromFolderRequest, FromFolderStatus, FromFolderStep, SpaceConfirm, SpaceSyncRequest};
use craft_cloud_meta::{Meta, Role, UserRecord};
use craft_cloud_proto::{NodeKind, NodeState, SpaceId, SpaceName};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use tokio::sync::mpsc;

const PASSWORD: &str = "correct horse battery";
/// Longest wait for anything to show up.
const PATIENCE: Duration = Duration::from_secs(60);

/// What the layer in front of the server saw.
#[derive(Default)]
struct Counts {
    /// `PUT /chunks/{id}` requests, and the bytes of their bodies.
    chunk_puts: AtomicU64,
    chunk_bytes: AtomicU64,
    /// `POST /chunks/claim` requests.
    claims: AtomicU64,
    /// Answer `404 no such endpoint` to claims, as a server from before them does.
    no_claim: AtomicBool,
}

async fn count(State(counts): State<Arc<Counts>>, request: Request<Body>, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    if request.method() == Method::PUT && path.contains("/chunks/") {
        counts.chunk_puts.fetch_add(1, Ordering::SeqCst);
        let len = request.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        counts.chunk_bytes.fetch_add(len, Ordering::SeqCst);
    }
    if request.method() == Method::POST && path.ends_with("/chunks/claim") {
        counts.claims.fetch_add(1, Ordering::SeqCst);
        if counts.no_claim.load(Ordering::SeqCst) {
            return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({ "error": "not_found", "message": "no such endpoint" }))).into_response();
        }
    }
    next.run(request).await
}

/// A server on a loopback port with an administrator `ana` and a member `bob`.
struct World {
    url: String,
    meta: Meta,
    ana: UserRecord,
    bob: UserRecord,
    counts: Arc<Counts>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dirs: Vec<tempfile::TempDir>,
}

async fn world() -> World {
    let (db, blobs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let state = Arc::new(AppState::new(meta.clone(), store, vec![]).unwrap());
    let counts = Arc::new(Counts::default());
    let app = craft_cloud_server::api::router(state).layer(axum::middleware::from_fn_with_state(counts.clone(), count));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    let hash = hash_password(PASSWORD).unwrap();
    let ana = meta.create_user("ana", &hash, Role::Admin, 0).await.unwrap();
    let bob = meta.create_user("bob", &hash, Role::Member, 0).await.unwrap();
    World { url, meta, ana, bob, counts, _shutdown: tx, _dirs: vec![db, blobs] }
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
    /// Two stops: the second ends the loop at once, as a user who stops it twice does.
    async fn crash(mut self) -> LoopResult {
        let _ = self.stop.send(());
        let _ = self.stop.send(());
        let thread = self.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap()).await.unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// An account on a computer: its data folder, its personal folder, and the loop that runs there.
struct Person {
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
        let (stop, stops) = mpsc::unbounded_channel();
        let data = self.data.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut out = Vec::new();
            let exit = runtime.block_on(run_loop(&data, quick(), stops, &mut out));
            (exit, String::from_utf8_lossy(&out).into_owned())
        });
        self.running = Some(Running { stop, thread: Some(thread) });
    }

    async fn ready(&self) {
        eventually("the local API answers", || async { self.api(|c| c.status()).await.is_ok() }).await;
    }

    async fn crash(&mut self) -> LoopResult {
        self.running.take().unwrap().crash().await
    }

    fn shared(&self) -> PathBuf {
        self.dir.path().join("Craft Cloud Shared")
    }

    async fn api<T: Send + 'static>(
        &self,
        call: impl FnOnce(Client) -> Result<T, craft_cloud_app::Error> + Send + 'static,
    ) -> Result<T, craft_cloud_app::Error> {
        let dir = self.data.path().to_owned();
        tokio::task::spawn_blocking(move || call(Client::connect_in(&dir)?)).await.unwrap()
    }

    async fn work(&self) -> Option<FromFolderStatus> {
        self.api(|c| c.status()).await.ok()?.from_folder
    }

    /// Asks to make a team space from `folder` (inside the personal folder), confirmed with the right password.
    async fn make(&self, folder: &Path, name: &str) -> Result<FromFolderStatus, craft_cloud_app::Error> {
        self.make_with(folder, name, Some(PASSWORD)).await
    }

    async fn make_with(&self, folder: &Path, name: &str, password: Option<&str>) -> Result<FromFolderStatus, craft_cloud_app::Error> {
        let request = FromFolderRequest {
            folder: folder.display().to_string(),
            name: name.to_owned(),
            quota_bytes: None,
            confirm: SpaceConfirm { password: password.map(str::to_owned), totp_code: None, recovery_code: None },
        };
        self.api(move |c| c.make_space_from_folder(&request)).await
    }

    /// Waits until the work reaches `step`.
    async fn until(&self, step: FromFolderStep) -> FromFolderStatus {
        let mut last = None;
        let deadline = Instant::now() + PATIENCE;
        loop {
            let status = self.work().await;
            if let Some(status) = status.clone().filter(|s| s.step == step) {
                return status;
            }
            assert!(Instant::now() < deadline, "timed out waiting until the work is at {step:?}; it is at {status:?} (before: {last:?})");
            last = status;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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

/// A repeatable pseudo-random file of `len` bytes that no other file shares chunks with (`seed` tells them apart).
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

fn write(folder: &Path, rel: &str, data: &[u8]) {
    let path = folder.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

/// The files under `folder` as (relative path with `/`, content), the agent's own folder left out.
fn files_under(folder: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![folder.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name() == ".craft-cloud" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(folder)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// A folder of the kinds of things people have: a few files, a subfolder, a deeper one, an empty folder, and a file of several chunks.
fn sample(folder: &Path) -> BTreeMap<String, Vec<u8>> {
    write(folder, "brief.txt", b"the brief");
    write(folder, "Shots/010.psd", &noise(1, 70_000));
    write(folder, "Shots/020.psd", &noise(2, 90_000));
    write(folder, "Shots/Raw/take-1.bin", &noise(3, 40_000));
    write(folder, "Renders/final.mov", &noise(4, 3 * 1024 * 1024));
    std::fs::create_dir_all(folder.join("Empty")).unwrap();
    files_under(folder)
}

/// The paths of the live nodes of a space on the server, with their file size (`None` for a folder).
async fn server_tree(meta: &Meta, space: SpaceId) -> BTreeMap<String, Option<u64>> {
    let mut nodes: BTreeMap<craft_cloud_proto::NodeId, NodeState> = BTreeMap::new();
    let mut after = None;
    loop {
        let page = meta.tree(space, after, 5000).await.unwrap();
        for node in page.nodes {
            nodes.insert(node.id, node);
        }
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    let mut out = BTreeMap::new();
    for node in nodes.values().filter(|n| !n.deleted) {
        let mut names = Vec::new();
        let mut at = Some(node);
        while let Some(current) = at {
            let Some(name) = &current.name else { break };
            names.push(name.as_str().to_owned());
            at = current.parent.and_then(|p| nodes.get(&p));
        }
        if names.is_empty() {
            continue;
        }
        names.reverse();
        out.insert(
            names.join("/"),
            match &node.kind {
                NodeKind::File(file) => Some(file.size),
                NodeKind::Dir => None,
            },
        );
    }
    out
}

/// Waits until the personal space on the server has every file of `files` under `prefix`.
async fn personal_has(w: &World, prefix: &str, files: &BTreeMap<String, Vec<u8>>) {
    let space = w.ana.space;
    eventually("the personal space on the server has the folder's files", || async {
        let tree = server_tree(&w.meta, space).await;
        files.iter().all(|(rel, data)| tree.get(&format!("{prefix}/{rel}")) == Some(&Some(data.len() as u64)))
    })
    .await;
}

/// Ana with her agent running and the folder `Project` of [`sample`] synced into her personal space.
async fn ana_with_project() -> (World, Person, PathBuf, BTreeMap<String, Vec<u8>>) {
    let w = world().await;
    let mut ana = Person::signed_in(&w.url, "ana").await;
    let project = ana.personal.join("Project");
    let files = sample(&project);
    ana.run();
    ana.ready().await;
    personal_has(&w, "Project", &files).await;
    (w, ana, project, files)
}

/// The team space `name` on the server, if there is one.
async fn space_named(w: &World, name: &str) -> Option<SpaceId> {
    w.meta.team_space_by_name(&SpaceName::new(name).unwrap()).await.unwrap()
}

/// Everything the old folder had is still there, byte for byte.
fn untouched(project: &Path, files: &BTreeMap<String, Vec<u8>>) {
    assert_eq!(&files_under(project), files, "the old folder must be exactly as it was");
}

/// Waits for the end of the work and asserts it is done.
async fn done(ana: &Person) -> FromFolderStatus {
    let status = ana.until(FromFolderStep::Done).await;
    assert_eq!(status.failed_at, None, "{status:?}");
    status
}

/// The team space holds exactly `files` (under the team folder on disk and on the server).
async fn team_holds(w: &World, status: &FromFolderStatus, files: &BTreeMap<String, Vec<u8>>) {
    team_holds_dirs(w, status, files, true).await;
}

/// As [`team_holds`]; `empty`: the sample's empty folder is in the team space too.
async fn team_holds_dirs(w: &World, status: &FromFolderStatus, files: &BTreeMap<String, Vec<u8>>, empty: bool) {
    let space = status.space.unwrap();
    let folder = PathBuf::from(status.team_folder.clone().unwrap());
    assert_eq!(&files_under(&folder), files, "the team folder");
    let tree = server_tree(&w.meta, space).await;
    for (rel, data) in files {
        assert_eq!(tree.get(rel), Some(&Some(data.len() as u64)), "{rel} on the server");
    }
    assert!(!empty || tree.contains_key("Empty"), "the empty folder is in the team space too: {tree:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_becomes_a_team_space_and_no_byte_is_uploaded_again() {
    let (w, ana, project, files) = ana_with_project().await;
    let (puts, bytes) = (w.counts.chunk_puts.load(Ordering::SeqCst), w.counts.chunk_bytes.load(Ordering::SeqCst));
    assert!(puts > 0 && bytes >= 3 * 1024 * 1024, "the personal upload happened: {puts} chunks, {bytes} bytes");

    let started = ana.make(&project, "Project team").await.unwrap();
    assert_eq!((started.name.as_str(), started.step), ("Project team", FromFolderStep::SettingUp), "{started:?}");
    let status = done(&ana).await;
    assert_eq!(status.kept, 0, "{status:?}");

    // The team space has everything, in the folder beside the personal one; the old folder is gone from the disk.
    team_holds(&w, &status, &files).await;
    assert_eq!(PathBuf::from(status.team_folder.clone().unwrap()), ana.shared().join("Project team"));
    assert!(!project.exists(), "the old folder was deleted after the team space held everything");
    // Nothing travelled again: the team space took its chunks from the personal space.
    assert_eq!(w.counts.chunk_puts.load(Ordering::SeqCst), puts, "no chunk was uploaded again");
    assert_eq!(w.counts.chunk_bytes.load(Ordering::SeqCst), bytes);
    assert!(w.counts.claims.load(Ordering::SeqCst) >= 1, "the chunks were claimed");
    // The account owns the space, as the only member.
    let space = status.space.unwrap();
    assert_eq!(w.meta.team_space_by_name(&SpaceName::new("Project team").unwrap()).await.unwrap(), Some(space));
    let members = w.meta.list_members(space).await.unwrap();
    assert_eq!(members.iter().map(|m| (m.user, m.role)).collect::<Vec<_>>(), vec![(w.ana.id, craft_cloud_app::SpaceRole::Owner)]);
    // The space is listed as syncing here; the personal sync moves the folder to the server's trash (12 files or fewer: not held).
    let info = ana.api(move |c| c.spaces()).await.unwrap().spaces.into_iter().find(|s| s.space == space).unwrap();
    assert_eq!(info.sync, craft_cloud_app::SpaceSync::On);
    eventually("the personal space has the old folder in its trash", || async {
        let tree = server_tree(&w.meta, w.ana.space).await;
        !tree.keys().any(|k| k.starts_with("Project/"))
    })
    .await;
    let trash = w.meta.trash(w.ana.space, None, 200, 1).await.unwrap();
    assert!(!trash.entries.is_empty(), "the old folder is recoverable from the personal trash");
    // The journal says done, and the status still has it.
    assert_eq!(ana.work().await.map(|s| s.step), Some(FromFolderStep::Done));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_the_server_does_not_know_falls_back_to_the_upload() {
    let (w, ana, project, files) = ana_with_project().await;
    w.counts.no_claim.store(true, Ordering::SeqCst);
    let (puts, bytes) = (w.counts.chunk_puts.load(Ordering::SeqCst), w.counts.chunk_bytes.load(Ordering::SeqCst));
    ana.make(&project, "Fallback").await.unwrap();
    let status = done(&ana).await;
    team_holds(&w, &status, &files).await;
    assert!(!project.exists());
    // Claimed once and refused, then uploaded: the bytes travelled again.
    assert_eq!(w.counts.claims.load(Ordering::SeqCst), 1, "the refused source is not asked again");
    assert!(w.counts.chunk_puts.load(Ordering::SeqCst) > puts, "the chunks were uploaded");
    assert!(w.counts.chunk_bytes.load(Ordering::SeqCst) >= bytes + 3 * 1024 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_administrators_confirmation_is_checked_before_anything_is_made() {
    let (w, ana, project, files) = ana_with_project().await;
    // A wrong password, no password, and a name that is not a name: refused, and no team space exists.
    let wrong = ana.make_with(&project, "Nope", Some("not the password")).await.unwrap_err();
    assert_eq!(wrong.code(), Some("wrong_password"), "{wrong:?}");
    let none = ana.make_with(&project, "Nope", None).await.unwrap_err();
    assert_eq!(none.code(), Some("password_required"), "{none:?}");
    let bad = ana.make(&project, "  ").await.unwrap_err();
    assert_eq!(bad.code(), Some("invalid"), "{bad:?}");
    assert!(space_named(&w, "Nope").await.is_none());
    assert_eq!(ana.work().await, None, "a refused try leaves no work behind");
    untouched(&project, &files);

    // A name another team space has.
    let first = ana.make(&project, "Taken").await.unwrap();
    assert!(first.space.is_some());
    done(&ana).await;
    let other = ana.personal.join("Other");
    let other_files = sample(&other);
    let clash = ana.make(&other, "taken").await.unwrap_err();
    assert_eq!(clash.code(), Some("conflict"), "{clash:?}");
    assert_eq!(files_under(&other), other_files);
    // The finished work is still the status after the refused try.
    assert_eq!(ana.work().await.map(|s| (s.name, s.step)), Some(("Taken".to_owned(), FromFolderStep::Done)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_who_is_not_an_administrator_is_refused_by_the_server() {
    let w = world().await;
    let mut bob = Person::signed_in(&w.url, "bob").await;
    let folder = bob.personal.join("Mine");
    let files = sample(&folder);
    bob.run();
    bob.ready().await;
    let refused = bob.make(&folder, "Mine team").await.unwrap_err();
    assert_eq!(refused.code(), Some("forbidden"), "{refused:?}");
    assert!(space_named(&w, "Mine team").await.is_none());
    assert_eq!(files_under(&folder), files);
    assert_eq!(bob.work().await, None);
    let _ = w.bob;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folders_that_cannot_be_made_into_a_team_space_are_refused_before_anything_is_made() {
    let (w, ana, project, files) = ana_with_project().await;
    let refused = |what: &'static str, folder: PathBuf| {
        let ana = &ana;
        async move {
            let e = ana.make(&folder, "Never made").await.unwrap_err();
            assert_eq!(e.code(), Some("invalid"), "{what}: {e:?}");
            e.to_string()
        }
    };
    // The personal folder itself, a folder outside it, a relative path, a folder that is not there, a file.
    refused("the personal folder", ana.personal.clone()).await;
    refused("outside", ana.dir.path().join("Elsewhere")).await;
    std::fs::create_dir_all(ana.dir.path().join("Elsewhere")).unwrap();
    refused("outside, existing", ana.dir.path().join("Elsewhere")).await;
    refused("not there", ana.personal.join("Ghost")).await;
    refused("a file", project.join("brief.txt")).await;
    let relative = ana.make_with(Path::new("Project"), "Never made", Some(PASSWORD)).await.unwrap_err();
    assert_eq!(relative.code(), Some("invalid"), "{relative:?}");
    // An empty folder.
    std::fs::create_dir_all(ana.personal.join("Nothing")).unwrap();
    refused("empty", ana.personal.join("Nothing")).await;
    // A folder with a `Craft Cloud Shared` folder in it, or another synced folder (it has its own `.craft-cloud`).
    write(&ana.personal, "Wrapper/Craft Cloud Shared/x.txt", b"x");
    let words = refused("holds the shared folder", ana.personal.join("Wrapper")).await;
    assert!(words.contains("Craft Cloud Shared"), "{words}");
    write(&ana.personal, "Wrapper2/other/.craft-cloud/root.json", b"{}");
    write(&ana.personal, "Wrapper2/a.txt", b"a");
    let words = refused("holds a synced folder", ana.personal.join("Wrapper2")).await;
    assert!(words.contains(".craft-cloud"), "{words}");
    // A link is not followed (Unix only: making one on Windows needs a privilege).
    #[cfg(unix)]
    {
        write(&ana.personal, "Linked/a.txt", b"a");
        std::os::unix::fs::symlink(&project, ana.personal.join("Linked/link")).unwrap();
        let words = refused("holds a link", ana.personal.join("Linked")).await;
        assert!(words.contains("link"), "{words}");
    }
    // Nothing was made, and nothing touched.
    assert!(space_named(&w, "Never made").await.is_none());
    assert_eq!(ana.work().await, None);
    untouched(&project, &files);

    // A folder that already is a team space's, and one inside it.
    let status = {
        ana.make(&project, "Real").await.unwrap();
        done(&ana).await
    };
    let team_folder = PathBuf::from(status.team_folder.unwrap());
    // The team folder lies outside the personal folder, so it is "not inside the personal folder"; a folder inside it too.
    let e = ana.make(&team_folder, "Again").await.unwrap_err();
    assert_eq!(e.code(), Some("invalid"), "{e:?}");
    assert!(space_named(&w, "Again").await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failure_at_any_step_leaves_the_old_folder_and_the_work_goes_on_with_the_same_space() {
    for step in [FromFolderStep::SettingUp, FromFolderStep::Copying, FromFolderStep::Syncing, FromFolderStep::Verifying, FromFolderStep::Trashing] {
        let (w, ana, project, files) = ana_with_project().await;
        hook::set(ana.data.path(), Some(Hook::Fail(step)));
        ana.make(&project, "Hooked").await.unwrap();
        let failed = ana.until(FromFolderStep::Failed).await;
        assert_eq!(failed.failed_at, Some(step), "{failed:?}");
        let space = failed.space.expect("the half-made team space is reported");
        // Whatever the step, nothing of the old folder is gone, and the team space is kept (reported, not deleted).
        untouched(&project, &files);
        assert_eq!(space_named(&w, "Hooked").await, Some(space), "{step:?}");

        // Asked again without the password (the space is made): it goes on with the same space and finishes.
        hook::set(ana.data.path(), None);
        let again = ana.make_with(&project, "Hooked", None).await.unwrap();
        assert_eq!(again.space, Some(space), "{step:?}");
        let status = done(&ana).await;
        assert_eq!(status.space, Some(space));
        team_holds(&w, &status, &files).await;
        assert!(!project.exists(), "{step:?}");
        let spaces = ana.api(|c| c.spaces()).await.unwrap().spaces.into_iter().filter(|s| s.kind == "team").count();
        assert_eq!(spaces, 1, "one team space, not two ({step:?})");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_at_any_step_leaves_the_old_folder_and_the_next_start_reports_it() {
    for step in [FromFolderStep::SettingUp, FromFolderStep::Copying, FromFolderStep::Syncing, FromFolderStep::Verifying, FromFolderStep::Trashing] {
        let (w, mut ana, project, files) = ana_with_project().await;
        hook::set(ana.data.path(), Some(Hook::Pause(step)));
        ana.make(&project, "Crashed").await.unwrap();
        let under_way = ana.until(step).await;
        let space = under_way.space.unwrap();
        // The agent is killed in the middle of the step (two stops, no waiting for the work).
        let (exit, out) = ana.crash().await;
        let _ = (exit, out);
        untouched(&project, &files);
        assert_eq!(space_named(&w, "Crashed").await, Some(space));

        // Started again: the work is reported as interrupted, and says where.
        hook::set(ana.data.path(), None);
        ana.run();
        ana.ready().await;
        let status = ana.work().await.expect("the interrupted work is reported");
        assert_eq!((status.step, status.failed_at), (FromFolderStep::Interrupted, Some(step)), "{status:?}");
        assert_eq!(status.space, Some(space));
        assert!(status.message.as_deref().is_some_and(|m| !m.is_empty()), "{status:?}");
        untouched(&project, &files);

        // Asked again: the same space, finished.
        let again = ana.make_with(&project, "Crashed", None).await.unwrap();
        assert_eq!(again.space, Some(space), "{step:?}");
        let status = done(&ana).await;
        team_holds(&w, &status, &files).await;
        assert!(!project.exists(), "{step:?}");
        assert_eq!(ana.api(|c| c.spaces()).await.unwrap().spaces.iter().filter(|s| s.kind == "team").count(), 1, "{step:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_changed_after_it_was_copied_is_copied_again_before_anything_is_deleted() {
    let (w, ana, project, files) = ana_with_project().await;
    hook::set(ana.data.path(), Some(Hook::Pause(FromFolderStep::Verifying)));
    ana.make(&project, "Changing").await.unwrap();
    let under_way = ana.until(FromFolderStep::Verifying).await;
    // After the copy and the sync, before the check: the file is edited, and a new one is added.
    let edited = noise(99, 50_000);
    write(&project, "Shots/010.psd", &edited);
    write(&project, "late.txt", b"made late");
    hook::set(ana.data.path(), None);
    let status = done(&ana).await;
    assert_eq!(status.space, under_way.space);
    let mut expected = files.clone();
    expected.insert("Shots/010.psd".into(), edited);
    expected.insert("late.txt".into(), b"made late".to_vec());
    team_holds(&w, &status, &expected).await;
    assert!(!project.exists(), "the changed and the new file were in the team space before the folder went");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_changed_after_the_check_stays_in_the_old_folder() {
    let (w, ana, project, files) = ana_with_project().await;
    hook::set(ana.data.path(), Some(Hook::Pause(FromFolderStep::Trashing)));
    ana.make(&project, "Late edit").await.unwrap();
    let under_way = ana.until(FromFolderStep::Trashing).await;
    // After the check, before the deletion: an edit the team space does not have.
    write(&project, "brief.txt", b"edited at the last moment");
    hook::set(ana.data.path(), None);
    let status = done(&ana).await;
    assert_eq!((status.space, status.kept), (under_way.space, 1), "{status:?}");
    assert!(status.message.as_deref().is_some_and(|m| m.contains("stayed")), "{status:?}");
    // That file stays, as it is; everything else is gone.
    let left = files_under(&project);
    assert_eq!(left.keys().collect::<Vec<_>>(), vec!["brief.txt"], "{left:?}");
    assert_eq!(left["brief.txt"], b"edited at the last moment");
    let mut copied = files.clone();
    copied.insert("brief.txt".into(), files["brief.txt"].clone());
    team_holds(&w, &status, &copied).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_team_folder_takes_a_free_name_and_the_mass_delete_guard_sees_the_old_folder_go() {
    let w = world().await;
    let mut ana = Person::signed_in(&w.url, "ana").await;
    // A folder already stands where the team folder would: the team space gets "(2)".
    std::fs::create_dir_all(ana.shared().join("Big")).unwrap();
    write(&ana.shared().join("Big"), "mine.txt", b"somebody's");
    let big = ana.personal.join("Big");
    let mut files = BTreeMap::new();
    for n in 0..14 {
        let data = noise(100 + n, 5_000);
        write(&big, &format!("part-{n:02}.bin"), &data);
        files.insert(format!("part-{n:02}.bin"), data);
    }
    ana.run();
    ana.ready().await;
    personal_has(&w, "Big", &files).await;

    ana.make(&big, "Big").await.unwrap();
    let status = done(&ana).await;
    assert_eq!(PathBuf::from(status.team_folder.clone().unwrap()), ana.shared().join("Big (2)"));
    assert_eq!(std::fs::read(ana.shared().join("Big/mine.txt")).unwrap(), b"somebody's", "the other folder is untouched");
    team_holds_dirs(&w, &status, &files, false).await;
    assert!(!big.exists());
    // Fourteen files gone at once is what the guard holds: the personal space on the server still has them, and the state says
    // they wait for approval (visible, nothing deleted behind the person's back).
    eventually("the personal pass holds the deletion", || async {
        let text = std::fs::read_to_string(ana.data.path().join("state.json")).unwrap_or_default();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let held = json["last_pass"]["summary"]["pending_deletes"].as_array().map_or(0, Vec::len);
        held >= 14
    })
    .await;
    let tree = server_tree(&w.meta, w.ana.space).await;
    assert_eq!(tree.keys().filter(|k| k.starts_with("Big/")).count(), 14, "the server still has the old folder until the deletion is approved");
    let _: BTreeSet<_> = tree.keys().collect();
    let _ = SpaceSyncRequest::default();
}
