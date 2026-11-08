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
use craft_cloud_meta::{Meta, Role, SpaceActor, UserRecord};
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
    /// How long a `PUT /chunks/{id}` is held before the server goes on with it.
    chunk_delay_ms: AtomicU64,
}

async fn count(State(counts): State<Arc<Counts>>, request: Request<Body>, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    if request.method() == Method::PUT && path.contains("/chunks/") {
        counts.chunk_puts.fetch_add(1, Ordering::SeqCst);
        let len = request.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        counts.chunk_bytes.fetch_add(len, Ordering::SeqCst);
        let delay = counts.chunk_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
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
    assert!(status.message.as_deref().is_some_and(|m| m.contains("changed after the copy")), "{status:?}");
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

// ---- the review round ----

/// Writes a journal as a crashed call would have left it, for `folder` and `name`.
fn crashed_creating_journal(ana: &Person, personal: SpaceId, folder: &Path, name: &str, started_ms: Option<i64>) {
    let status = FromFolderStatus {
        folder: std::fs::canonicalize(folder).unwrap().display().to_string(),
        name: name.to_owned(),
        step: FromFolderStep::Interrupted,
        space: None,
        team_folder: None,
        done: 0,
        total: 0,
        failed_at: Some(FromFolderStep::Creating),
        message: Some("stopped".into()),
        kept: 0,
    };
    let journal = serde_json::json!({ "schema": 1, "personal": personal, "status": status, "started_ms": started_ms });
    std::fs::write(ana.data.path().join("from-folder.json"), serde_json::to_vec(&journal).unwrap()).unwrap();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_team_space_is_checked_again_right_before_the_old_folder_is_deleted() {
    for what in ["the space is archived", "a file is deleted in the team space"] {
        let (w, ana, project, files) = ana_with_project().await;
        hook::set(ana.data.path(), Some(Hook::Pause(FromFolderStep::Trashing)));
        ana.make(&project, "Recheck").await.unwrap();
        let under_way = ana.until(FromFolderStep::Trashing).await;
        let space = under_way.space.unwrap();
        // Between the check and the deletion the team space changes under the work.
        if what == "the space is archived" {
            w.meta.set_space_archived(&SpaceActor::Cli, space, true, 1).await.unwrap();
        } else {
            let page = w.meta.tree(space, None, 5000).await.unwrap();
            let node = page.nodes.iter().find(|n| n.name.as_ref().is_some_and(|name| name.as_str() == "brief.txt")).unwrap();
            let craft_cloud_proto::NodeKind::File(file) = &node.kind else { panic!("a file") };
            let device = w.meta.create_device(w.ana.id, "other", &[7; 32], 0).await.unwrap();
            let op = craft_cloud_proto::Op::Delete { node: node.id, expected: Some(file.version) };
            w.meta.commit(space, device, &[op], 2).await.unwrap();
        }
        hook::set(ana.data.path(), None);
        let stopped = ana.until(FromFolderStep::Failed).await;
        assert_eq!(stopped.failed_at, Some(FromFolderStep::Trashing), "{what}: {stopped:?}");
        assert!(stopped.message.as_deref().is_some_and(|m| m.contains("nothing more was deleted")), "{what}: {stopped:?}");
        untouched(&project, &files);
    }
}

/// Makes `link` a folder link to `target` (a junction on Windows, which needs no privilege).
fn make_link(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(link).arg(target).output().unwrap();
        assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stdout));
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_swapped_for_a_link_after_the_check_is_not_followed() {
    let (w, ana, project, files) = ana_with_project().await;
    hook::set(ana.data.path(), Some(Hook::Pause(FromFolderStep::Trashing)));
    ana.make(&project, "Swapped").await.unwrap();
    ana.until(FromFolderStep::Trashing).await;
    // `Shots` is moved aside (its files keep their content and times) and a link to it takes its place.
    let backup = ana.dir.path().join("Backup");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::rename(project.join("Shots"), backup.join("Shots")).unwrap();
    make_link(&project.join("Shots"), &backup.join("Shots"));
    hook::set(ana.data.path(), None);
    let status = done(&ana).await;
    // The three files behind the link were not touched, and are counted as kept; everything else went.
    let shots: BTreeMap<String, Vec<u8>> =
        files.iter().filter(|(k, _)| k.starts_with("Shots/")).map(|(k, v)| (k["Shots/".len()..].to_owned(), v.clone())).collect();
    assert_eq!(files_under(&backup.join("Shots")), shots, "what the link points to is untouched");
    assert_eq!(status.kept, 3, "{status:?}");
    assert!(!project.join("brief.txt").exists() && !project.join("Renders").exists());
    let _ = w;
}

fn append_to(held: &Path) {
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(held).unwrap().write_all(b" and more").unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_that_gets_in_after_the_file_was_taken_out_keeps_its_content() {
    let (w, ana, project, files) = ana_with_project().await;
    // Every file is appended to after it was taken out of the old folder and before it is read again (as a program that
    // already held it open would): the read finds another content, and the file goes back with it.
    hook::set_after_quarantine(ana.data.path(), Some(append_to));
    ana.make(&project, "Appending").await.unwrap();
    let status = done(&ana).await;
    assert_eq!(status.kept, files.len() as u64, "{status:?}");
    let mut grown = files.clone();
    for data in grown.values_mut() {
        data.extend_from_slice(b" and more");
    }
    assert_eq!(files_under(&project), grown, "every file is back, with what was written after the check");
    // The quarantine is empty and gone.
    assert!(
        !ana.personal.join(".craft-cloud").join("from-folder").exists()
            || std::fs::read_dir(ana.personal.join(".craft-cloud").join("from-folder")).unwrap().count() == 0
    );
    // The team space has the content from before.
    team_holds(&w, &status, &files).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ending_the_session_ends_the_work_and_its_status_is_never_shown_to_another_account() {
    let (w, ana, project, files) = ana_with_project().await;
    hook::set(ana.data.path(), Some(Hook::Pause(FromFolderStep::Syncing)));
    ana.make(&project, "Session").await.unwrap();
    ana.until(FromFolderStep::Syncing).await;
    // Signed out in the middle: the call answers once the work has ended.
    ana.api(|c| c.logout(false)).await.unwrap();
    hook::set(ana.data.path(), None);
    assert_eq!(ana.work().await, None, "signed out: no work to show");
    // Another account signs in on this agent: it sees nothing of the first one's work, and the journal of the first is gone.
    let url = w.url.clone();
    let folder = ana.dir.path().join("Bob's folder").display().to_string();
    let login = craft_cloud_app::LoginRequest {
        server: url,
        user: "bob".into(),
        password: PASSWORD.into(),
        folder: Some(folder),
        device_name: Some("bob-pc".into()),
        adopt: false,
    };
    ana.api(move |c| c.login(&login)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(ana.work().await, None, "the status of the first account is not shown under the second");
    assert!(!ana.data.path().join("from-folder.json").exists(), "the first account's journal is dropped");
    untouched(&project, &files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_big_file_that_takes_long_to_go_up_is_no_stall() {
    let w = world().await;
    let mut ana = Person::signed_in(&w.url, "ana").await;
    let big = ana.personal.join("Big");
    let mut files = BTreeMap::new();
    let data = noise(77, 5 * 1024 * 1024);
    write(&big, "video.bin", &data);
    files.insert("video.bin".to_owned(), data);
    ana.run();
    ana.ready().await;
    personal_has(&w, "Big", &files).await;
    // Nothing is claimed, so the team space takes the file chunk by chunk, each chunk slow. Together they take far longer than the
    // stall time, but the space stores something new all the time.
    w.counts.no_claim.store(true, Ordering::SeqCst);
    w.counts.chunk_delay_ms.store(1_000, Ordering::SeqCst);
    hook::set_stall(ana.data.path(), Some(Duration::from_secs(3)));
    let started = Instant::now();
    ana.make(&big, "Slow").await.unwrap();
    let status = done(&ana).await;
    assert!(started.elapsed() > Duration::from_secs(4), "the upload was slow: {:?}", started.elapsed());
    team_holds_dirs(&w, &status, &files, false).await;
    assert!(!big.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_stored_for_the_stall_time_stops_the_wait_and_leaves_the_old_folder() {
    let (w, ana, project, files) = ana_with_project().await;
    w.counts.no_claim.store(true, Ordering::SeqCst);
    // Every chunk takes longer than the stall time.
    w.counts.chunk_delay_ms.store(6_000, Ordering::SeqCst);
    hook::set_stall(ana.data.path(), Some(Duration::from_secs(2)));
    ana.make(&project, "Stalled").await.unwrap();
    let stopped = ana.until(FromFolderStep::Failed).await;
    assert_eq!(stopped.failed_at, Some(FromFolderStep::Syncing), "{stopped:?}");
    assert!(stopped.message.as_deref().is_some_and(|m| m.contains("stored nothing more")), "{stopped:?}");
    untouched(&project, &files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_in_the_team_folder_is_not_trusted_for_its_size_and_time() {
    let (w, ana, project, files) = ana_with_project().await;
    hook::set(ana.data.path(), Some(Hook::Fail(FromFolderStep::Syncing)));
    ana.make(&project, "Stale").await.unwrap();
    let failed = ana.until(FromFolderStep::Failed).await;
    assert_eq!(failed.failed_at, Some(FromFolderStep::Syncing));
    // After the copy was made, the file is changed by a tool that keeps its size and its time.
    let brief = project.join("brief.txt");
    let time = std::fs::metadata(&brief).unwrap().modified().unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&brief).unwrap();
    use std::io::Write;
    (&file).write_all(b"THE BRIEF").unwrap();
    file.set_modified(time).unwrap();
    drop(file);
    hook::set(ana.data.path(), None);
    ana.make_with(&project, "Stale", None).await.unwrap();
    let status = done(&ana).await;
    let mut expected = files.clone();
    expected.insert("brief.txt".into(), b"THE BRIEF".to_vec());
    team_holds(&w, &status, &expected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_space_made_by_someone_else_is_not_adopted_after_a_crash() {
    let (w, ana, project, files) = ana_with_project().await;
    let personal = w.ana.space;
    let cy = w.meta.create_user("cy", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
    let name = |n: &str| SpaceName::new(n).unwrap();
    // The same name, this account as owner, but another member too: not ours.
    let shared = w.meta.create_team_space(&SpaceActor::Cli, &name("Studio"), w.ana.id, None, now_ms()).await.unwrap();
    w.meta.invite_member(&SpaceActor::Cli, shared, cy.id, craft_cloud_app::SpaceRole::Editor, now_ms()).await.unwrap();
    crashed_creating_journal(&ana, personal, &project, "Studio", Some(now_ms() - 60_000));
    let refused = ana.make(&project, "Studio").await.unwrap_err();
    assert_eq!(refused.code(), Some("conflict"), "{refused:?}");
    // A refused retry does not move the start time the journal holds.
    let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(ana.data.path().join("from-folder.json")).unwrap()).unwrap();
    assert!(kept["started_ms"].as_i64().is_some_and(|t| t < now_ms() - 30_000), "{kept}");
    // One made long before the call started is not ours either.
    w.meta.create_team_space(&SpaceActor::Cli, &name("Old one"), w.ana.id, None, now_ms() - 3_600_000).await.unwrap();
    crashed_creating_journal(&ana, personal, &project, "Old one", Some(now_ms()));
    let refused = ana.make(&project, "Old one").await.unwrap_err();
    assert_eq!(refused.code(), Some("conflict"), "{refused:?}");
    untouched(&project, &files);
    // One made after the call started, alone, empty: the one the crashed call made.
    let mine = w.meta.create_team_space(&SpaceActor::Cli, &name("Mine"), w.ana.id, None, now_ms()).await.unwrap();
    crashed_creating_journal(&ana, personal, &project, "Mine", Some(now_ms() - 60_000));
    let adopted = ana.make(&project, "Mine").await.unwrap();
    assert_eq!(adopted.space, Some(mine));
    done(&ana).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_stopped_after_the_old_folder_was_removed_finishes_as_done() {
    let (_w, mut ana, project, _files) = ana_with_project().await;
    hook::set(ana.data.path(), Some(Hook::Pause(FromFolderStep::Trashing)));
    ana.make(&project, "Gone").await.unwrap();
    ana.until(FromFolderStep::Trashing).await;
    let _ = ana.crash().await;
    // The old folder went (as the last deletion's end would have left it), but "done" was never written.
    std::fs::remove_dir_all(&project).unwrap();
    hook::set(ana.data.path(), None);
    ana.run();
    ana.ready().await;
    let status = ana.work().await.expect("the work is reported");
    assert_eq!((status.step, status.failed_at), (FromFolderStep::Done, None), "{status:?}");
}

// ---- the second review round ----

/// A quarantine folder with `files` (relative path, content) for the old folder `root`, as a crashed deletion leaves it.
fn leave_in_quarantine(personal: &Path, _key: &str, root: &Path, files: &[(&str, &[u8])]) -> PathBuf {
    let dir = quarantine_of(personal, root);
    std::fs::create_dir_all(&dir).unwrap();
    let mut name = dir.file_name().unwrap().to_owned();
    name.push(".root");
    std::fs::write(dir.with_file_name(name), root.to_string_lossy().as_bytes()).unwrap();
    for (rel, data) in files {
        write(&dir, rel, data);
    }
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_a_crash_left_in_the_quarantine_is_put_back_at_the_next_start_with_its_folders() {
    let (_w, mut ana, project, files) = ana_with_project().await;
    let _ = ana.crash().await;
    // A file was taken out of `Shots/New`, the crash came before it was deleted, and the folder was deleted meanwhile.
    let canonical = std::fs::canonicalize(&project).unwrap();
    let dir = leave_in_quarantine(&ana.personal, "0123456789abcdef", &canonical, &[("Shots/New/lost.psd", b"the only copy")]);
    ana.run();
    ana.ready().await;
    assert_eq!(std::fs::read(project.join("Shots/New/lost.psd")).unwrap(), b"the only copy");
    assert!(!dir.exists(), "the quarantine folder is gone");
    assert_eq!(ana.work().await, None);
    let mut expected = files.clone();
    expected.insert("Shots/New/lost.psd".into(), b"the only copy".to_vec());
    assert_eq!(files_under(&project), expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_that_cannot_be_put_back_is_reported_with_its_folder_and_tried_again_at_the_next_start() {
    let (_w, mut ana, project, _files) = ana_with_project().await;
    let _ = ana.crash().await;
    let canonical = std::fs::canonicalize(&project).unwrap();
    // `Blocked` is a file where the file's folder should be: it can't go back.
    write(&project, "Blocked", b"in the way");
    let dir = leave_in_quarantine(&ana.personal, "fedcba9876543210", &canonical, &[("Blocked/lost.psd", b"kept safe")]);
    ana.run();
    ana.ready().await;
    let status = ana.work().await.expect("the leftover is reported");
    assert_eq!((status.step, status.failed_at), (FromFolderStep::Failed, Some(FromFolderStep::Trashing)), "{status:?}");
    assert!(status.message.as_deref().is_some_and(|m| m.contains(&dir.display().to_string())), "{status:?}");
    assert_eq!(std::fs::read(dir.join("Blocked/lost.psd")).unwrap(), b"kept safe", "the file is still in the quarantine");
    // The way is cleared and the agent starts again: the file goes back, and nothing is reported.
    let _ = ana.crash().await;
    std::fs::remove_file(project.join("Blocked")).unwrap();
    ana.run();
    ana.ready().await;
    assert_eq!(std::fs::read(project.join("Blocked/lost.psd")).unwrap(), b"kept safe");
    assert_eq!(ana.work().await, None);
    assert!(!dir.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quarantine_is_put_back_whatever_the_journal_says_and_whoever_signed_in_since() {
    let (_w, mut ana, project, _files) = ana_with_project().await;
    let _ = ana.crash().await;
    let canonical = std::fs::canonicalize(&project).unwrap();
    leave_in_quarantine(&ana.personal, "aaaaaaaaaaaaaaaa", &canonical, &[("left.txt", b"left behind")]);
    // The journal is another account's (as if another one had used the agent meanwhile).
    let foreign = FromFolderStatus {
        folder: "/elsewhere".into(),
        name: "Other".into(),
        step: FromFolderStep::Trashing,
        space: None,
        team_folder: None,
        done: 0,
        total: 0,
        failed_at: None,
        message: None,
        kept: 0,
    };
    let journal = serde_json::json!({ "schema": 1, "personal": SpaceId::generate(), "status": foreign });
    std::fs::write(ana.data.path().join("from-folder.json"), serde_json::to_vec(&journal).unwrap()).unwrap();
    ana.run();
    ana.ready().await;
    assert_eq!(std::fs::read(project.join("left.txt")).unwrap(), b"left behind");
    assert!(!ana.data.path().join("from-folder.json").exists(), "the foreign journal is dropped");
    assert_eq!(ana.work().await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_folder_swapped_for_a_link_to_an_identical_copy_is_not_deleted_from() {
    let (_w, ana, project, files) = ana_with_project().await;
    hook::set(ana.data.path(), Some(Hook::Pause(FromFolderStep::Trashing)));
    ana.make(&project, "Root swapped").await.unwrap();
    ana.until(FromFolderStep::Trashing).await;
    // The whole old folder is moved aside (the copy has the same files with the same times) and a link takes its place.
    let backup = ana.dir.path().join("Backup");
    std::fs::rename(&project, &backup).unwrap();
    make_link(&project, &backup);
    hook::set(ana.data.path(), None);
    let stopped = ana.until(FromFolderStep::Failed).await;
    assert_eq!(stopped.failed_at, Some(FromFolderStep::Trashing), "{stopped:?}");
    assert!(stopped.message.as_deref().is_some_and(|m| m.contains("moved or replaced")), "{stopped:?}");
    assert_eq!(files_under(&backup), files, "the copy behind the link is untouched");
}

fn slow_quarantine(_held: &Path) {
    std::thread::sleep(Duration::from_millis(500));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_team_space_is_asked_again_before_every_batch_of_deletions() {
    let (w, ana, project, files) = ana_with_project().await;
    hook::set_trash_batch(ana.data.path(), Some(2));
    hook::set_after_quarantine(ana.data.path(), Some(slow_quarantine));
    let started = ana.make(&project, "Batches").await.unwrap();
    let space = started.space.unwrap();
    // The first batch is under way (its first file is gone): the space is archived before the second one starts.
    eventually("the first file is deleted", || async { !project.join("Renders/final.mov").exists() }).await;
    w.meta.set_space_archived(&SpaceActor::Cli, space, true, 1).await.unwrap();
    let stopped = ana.until(FromFolderStep::Failed).await;
    assert_eq!(stopped.failed_at, Some(FromFolderStep::Trashing), "{stopped:?}");
    // What was not deleted is still there, as it was.
    let left = files_under(&project);
    assert!(left.contains_key("brief.txt") && !left.is_empty(), "{left:?}");
    for (rel, data) in &left {
        assert_eq!(files.get(rel), Some(data), "{rel}");
    }
    assert!(left.len() < files.len(), "the first batch went");
}

async fn session_ends_during(step: FromFolderStep) {
    let (w, ana, project, files) = ana_with_project().await;
    hook::set(ana.data.path(), Some(Hook::Pause(step)));
    let make = ana.make(&project, "Ended");
    let end = async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        ana.api(|c| c.logout(false)).await.unwrap();
        hook::set(ana.data.path(), None);
    };
    let (made, ()) = tokio::join!(make, end);
    let refused = made.unwrap_err();
    assert_eq!(refused.code(), Some("not_signed_in"), "{step:?}: {refused:?}");
    // Another account signs in: it sees nothing of the call, and no journal of the first is left under it.
    let folder = ana.dir.path().join("Bob's folder").display().to_string();
    let login = craft_cloud_app::LoginRequest {
        server: w.url.clone(),
        user: "bob".into(),
        password: PASSWORD.into(),
        folder: Some(folder),
        device_name: Some("bob-pc".into()),
        adopt: false,
    };
    ana.api(move |c| c.login(&login)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(ana.work().await, None, "{step:?}: nothing is shown under the second account");
    assert!(!ana.data.path().join("from-folder.json").exists(), "{step:?}: no journal");
    untouched(&project, &files);
    // The space of a call that had got as far as the server stays there, nothing in it.
    assert_eq!(space_named(&w, "Ended").await.is_some(), step == FromFolderStep::Creating, "{step:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_whose_session_ends_while_the_folder_is_checked_writes_nothing() {
    session_ends_during(FromFolderStep::Checking).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_whose_session_ends_while_the_space_is_made_writes_nothing() {
    session_ends_during(FromFolderStep::Creating).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_work_of_a_session_ends_with_it_even_in_the_middle_of_a_slow_copy() {
    let w = world().await;
    let mut ana = Person::signed_in(&w.url, "ana").await;
    let big = ana.personal.join("Big");
    let data = noise(31, 6 * 1024 * 1024);
    write(&big, "video.bin", &data);
    let files = BTreeMap::from([("video.bin".to_owned(), data)]);
    ana.run();
    ana.ready().await;
    personal_has(&w, "Big", &files).await;
    hook::set_copy_delay(ana.data.path(), Some(Duration::from_millis(400)));
    ana.make(&big, "Slow copy").await.unwrap();
    ana.until(FromFolderStep::Copying).await;
    // Signed out in the middle of the file, and another account signed in at once: the job is gone before the new session starts.
    ana.api(|c| c.logout(false)).await.unwrap();
    let folder = ana.dir.path().join("Bob's folder").display().to_string();
    let login = craft_cloud_app::LoginRequest {
        server: w.url.clone(),
        user: "bob".into(),
        password: PASSWORD.into(),
        folder: Some(folder),
        device_name: Some("bob-pc".into()),
        adopt: false,
    };
    ana.api(move |c| c.login(&login)).await.unwrap();
    for _ in 0..30 {
        assert_eq!(ana.work().await, None, "the work of the first session writes nothing in the second");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(files_under(&big), files);
}

// ---- the third review round ----

/// The quarantine folder `key` of the old folder `root` is named after (as the agent names it).
fn quarantine_of(personal: &Path, root: &Path) -> PathBuf {
    let key: String = blake3::hash(root.to_string_lossy().as_bytes()).to_hex().chars().take(16).collect();
    personal.join(".craft-cloud").join("from-folder").join(key)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_called_root_txt_is_moved_and_deleted_like_any_other() {
    let w = world().await;
    let mut ana = Person::signed_in(&w.url, "ana").await;
    let folder = ana.personal.join("Notes");
    let mut files = BTreeMap::new();
    for (name, data) in [("root.txt", b"my own file".to_vec()), ("a.txt", b"another".to_vec())] {
        write(&folder, name, &data);
        files.insert(name.to_owned(), data);
    }
    ana.run();
    ana.ready().await;
    personal_has(&w, "Notes", &files).await;
    ana.make(&folder, "Notes team").await.unwrap();
    let status = done(&ana).await;
    assert_eq!((status.kept, status.message.clone()), (0, None), "{status:?}");
    team_holds_dirs(&w, &status, &files, false).await;
    assert!(!folder.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_moved_aside_when_the_marker_cannot_be_written() {
    let (_w, ana, project, files) = ana_with_project().await;
    // A folder stands where the marker file should be: it can't be written.
    let marker = quarantine_of(&ana.personal, &std::fs::canonicalize(&project).unwrap());
    let mut name = marker.file_name().unwrap().to_owned();
    name.push(".root");
    let blocker = marker.with_file_name(name);
    std::fs::create_dir_all(&blocker).unwrap();
    std::fs::write(blocker.join("in the way"), b"x").unwrap();
    ana.make(&project, "No marker").await.unwrap();
    let status = done(&ana).await;
    // Every file stayed where it was, and said so; the quarantine got nothing.
    assert_eq!(status.kept, files.len() as u64, "{status:?}");
    assert_eq!(files_under(&project), files);
    assert!(!marker.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quarantine_with_an_empty_or_unsound_marker_is_left_alone_and_reported() {
    let (_w, mut ana, project, _files) = ana_with_project().await;
    let _ = ana.crash().await;
    let canonical = std::fs::canonicalize(&project).unwrap();
    let dir = quarantine_of(&ana.personal, &canonical);
    write(&dir, "Shots/a.psd", b"must not go anywhere else");
    let mut name = dir.file_name().unwrap().to_owned();
    name.push(".root");
    let marker = dir.with_file_name(name);
    let here = std::env::current_dir().unwrap();
    for text in ["", "relative/place", "C:\\elsewhere\\Craft"] {
        std::fs::write(&marker, text).unwrap();
        ana.run();
        ana.ready().await;
        let status = ana.work().await.expect("the quarantine is reported");
        assert_eq!((status.step, status.failed_at), (FromFolderStep::Failed, Some(FromFolderStep::Trashing)), "{text:?}: {status:?}");
        assert_eq!(std::fs::read(dir.join("Shots/a.psd")).unwrap(), b"must not go anywhere else", "{text:?}");
        assert!(!here.join("Shots").exists() && !here.join("relative").exists(), "{text:?}: nothing was written outside");
        let _ = ana.crash().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_old_message_about_files_that_could_not_be_put_back_goes_when_they_are() {
    let (_w, mut ana, project, _files) = ana_with_project().await;
    let personal = ana_personal(&ana).await;
    let _ = ana.crash().await;
    let status = FromFolderStatus {
        folder: std::fs::canonicalize(&project).unwrap().display().to_string(),
        name: "Stale".into(),
        step: FromFolderStep::Failed,
        space: Some(SpaceId::generate()),
        team_folder: None,
        done: 0,
        total: 0,
        failed_at: Some(FromFolderStep::Trashing),
        message: Some("Some files could not be put back into the old folder and are kept in X. Move them back yourself; the agent tries again at its next start.".into()),
        kept: 0,
    };
    let journal = serde_json::json!({ "schema": 1, "personal": personal, "status": status });
    std::fs::write(ana.data.path().join("from-folder.json"), serde_json::to_vec(&journal).unwrap()).unwrap();
    ana.run();
    ana.ready().await;
    let shown = ana.work().await.unwrap();
    assert_eq!(shown.step, FromFolderStep::Interrupted, "{shown:?}");
    assert!(shown.message.as_deref().is_some_and(|m| !m.contains("Move them back yourself")), "{shown:?}");
}

/// The personal space ID of the account on this computer.
async fn ana_personal(ana: &Person) -> SpaceId {
    let list = ana.api(|c| c.spaces()).await.unwrap();
    list.spaces.into_iter().find(|s| s.kind == "personal").unwrap().space
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journal_with_no_server_time_adopts_nothing() {
    let (w, ana, project, files) = ana_with_project().await;
    let name = |n: &str| SpaceName::new(n).unwrap();
    w.meta.create_team_space(&SpaceActor::Cli, &name("Orphan"), w.ana.id, None, now_ms()).await.unwrap();
    crashed_creating_journal(&ana, w.ana.space, &project, "Orphan", None);
    let refused = ana.make(&project, "Orphan").await.unwrap_err();
    assert_eq!(refused.code(), Some("conflict"), "{refused:?}");
    untouched(&project, &files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_that_cannot_be_looked_at_is_not_taken_for_gone() {
    let (_w, mut ana, project, _files) = ana_with_project().await;
    let _ = ana.crash().await;
    // Not "not found": the path can't even be looked at (a NUL in it).
    let status = FromFolderStatus {
        folder: "/bad\0path".into(),
        name: "Unreachable".into(),
        step: FromFolderStep::Trashing,
        space: Some(SpaceId::generate()),
        team_folder: None,
        done: 0,
        total: 0,
        failed_at: None,
        message: None,
        kept: 0,
    };
    let journal = serde_json::json!({ "schema": 1, "personal": SpaceId::generate(), "status": status });
    // The journal must be this account's: read the personal space from a running agent first.
    ana.run();
    ana.ready().await;
    let personal = ana_personal(&ana).await;
    let _ = ana.crash().await;
    let journal = serde_json::json!({ "schema": 1, "personal": personal, "status": journal["status"] });
    std::fs::write(ana.data.path().join("from-folder.json"), serde_json::to_vec(&journal).unwrap()).unwrap();
    ana.run();
    ana.ready().await;
    let shown = ana.work().await.unwrap();
    assert_eq!((shown.step, shown.failed_at), (FromFolderStep::Interrupted, Some(FromFolderStep::Trashing)), "{shown:?}");
    let _ = project;
}

// ---- the independent review: the copy's destination ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_team_subfolder_swapped_for_a_link_is_not_written_through() {
    let (_w, ana, project, files) = ana_with_project().await;
    hook::set(ana.data.path(), Some(Hook::Fail(FromFolderStep::Syncing)));
    ana.make(&project, "Linked team").await.unwrap();
    let failed = ana.until(FromFolderStep::Failed).await;
    let team = PathBuf::from(failed.team_folder.clone().unwrap());
    // Another project with a file of the same name lies elsewhere; the team folder's `Shots` becomes a link to it.
    let other = ana.dir.path().join("OtherProject");
    write(&other, "010.psd", b"somebody else's file");
    std::fs::remove_dir_all(team.join("Shots")).unwrap();
    make_link(&team.join("Shots"), &other);
    hook::set(ana.data.path(), None);
    ana.make_with(&project, "Linked team", None).await.unwrap();
    let stopped = ana.until(FromFolderStep::Failed).await;
    assert_eq!(stopped.failed_at, Some(FromFolderStep::Copying), "{stopped:?}");
    assert!(stopped.message.as_deref().is_some_and(|m| m.contains("moved or replaced by a link")), "{stopped:?}");
    assert_eq!(files_under(&other), BTreeMap::from([("010.psd".to_owned(), b"somebody else's file".to_vec())]), "the other folder is untouched");
    untouched(&project, &files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_team_file_edited_after_the_copy_is_kept_and_the_old_version_goes_beside_it() {
    let (w, ana, project, files) = ana_with_project().await;
    hook::set(ana.data.path(), Some(Hook::Fail(FromFolderStep::Syncing)));
    ana.make(&project, "Edited there").await.unwrap();
    let failed = ana.until(FromFolderStep::Failed).await;
    let team = PathBuf::from(failed.team_folder.clone().unwrap());
    // Someone edits the file in the team folder; meanwhile the old folder's file changed too.
    std::fs::write(team.join("brief.txt"), b"edited in the team space").unwrap();
    std::fs::write(project.join("brief.txt"), b"a newer version here").unwrap();
    hook::set(ana.data.path(), None);
    ana.make_with(&project, "Edited there", None).await.unwrap();
    let status = done(&ana).await;
    // The team version stays; the old version is beside it under a conflict name; the old file is not deleted.
    assert_eq!(std::fs::read(team.join("brief.txt")).unwrap(), b"edited in the team space");
    let beside: Vec<String> = std::fs::read_dir(&team)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("brief") && n != "brief.txt")
        .collect();
    assert_eq!(beside.len(), 1, "{beside:?}");
    assert_eq!(std::fs::read(team.join(&beside[0])).unwrap(), b"a newer version here");
    assert_eq!(std::fs::read(project.join("brief.txt")).unwrap(), b"a newer version here", "the old file stays");
    assert_eq!(status.kept, 1, "{status:?}");
    assert!(status.message.as_deref().is_some_and(|m| m.contains("edited in the team space")), "{status:?}");
    // Every other file went.
    let mut left = files_under(&project);
    left.remove("brief.txt");
    assert!(left.is_empty(), "{left:?}");
    let _ = w;
}
