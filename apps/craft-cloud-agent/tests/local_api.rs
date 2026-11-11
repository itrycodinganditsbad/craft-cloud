//! The local API of a running agent (`run`), against a real server on a loopback port: its hardening against requests
//! that don't come from a local program with the token, and signing in and out through it while `run` runs.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use craft_cloud_agent::account::{DataDir, LoopState};
use craft_cloud_agent::cli::{self, AgentError, Exit};
use craft_cloud_agent::daemon::{RunOptions, run_loop};
use craft_cloud_agent::schedule::Timing;
use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::api::{DeviceInfo, ErrorBody, HistoryResponse, PairingState, PairingStatus};
use craft_cloud_proto::local::{JoinRequest, LOCAL_API_FILE, LocalApiFile, LoginRequest, PairingInfo, RunState, Status, codes};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use tokio::sync::mpsc;

const PASSWORD: &str = "correct horse battery";
const PATIENCE: Duration = Duration::from_secs(30);

struct Server {
    url: String,
    meta: Meta,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn server() -> Server {
    let (db, blobs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
    let state = AppState::new(meta.clone(), Store::new(DiskBackend::new(blobs.path()).unwrap()), vec![]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending()));
    Server { url, meta, task, _dirs: (db, blobs) }
}

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
        poll_wait: Duration::from_secs(1),
        poll_retry_first: Duration::from_millis(100),
        poll_retry_max: Duration::from_secs(1),
        poll_min_spacing: Duration::from_millis(100),
        poll_busy: Duration::from_millis(200),
        fail_watch: false,
        record_delay: Duration::ZERO,
        session_grace: Duration::from_secs(10),
        announce_delay: Duration::ZERO,
        // Never this machine's own start-at-login entry.
        autostart: craft_cloud_agent::autostart::Target::Off,
    }
}

/// `run` on a thread and runtime of its own, as the binary runs it.
struct Running {
    stop: mpsc::UnboundedSender<()>,
    thread: Option<std::thread::JoinHandle<(Result<Exit, AgentError>, String)>>,
}

impl Running {
    fn start(data: &DataDir) -> Self {
        Self::start_with(data, quick())
    }

    fn start_with(data: &DataDir, options: RunOptions) -> Self {
        let (stop, stops) = mpsc::unbounded_channel();
        let data = data.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut out = Vec::new();
            let exit = runtime.block_on(run_loop(&data, options, stops, &mut out));
            (exit, String::from_utf8_lossy(&out).into_owned())
        });
        Self { stop, thread: Some(thread) }
    }

    /// The loop's thread, to wait for its end without asking it to stop.
    fn thread_handle(mut self) -> std::thread::JoinHandle<(Result<Exit, AgentError>, String)> {
        self.thread.take().unwrap()
    }

    async fn stop(mut self) -> (Result<Exit, AgentError>, String) {
        let _ = self.stop.send(());
        let thread = self.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap()).await.unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // A loop whose thread was handed out ends by itself (quit): it isn't stopped here.
        if self.thread.is_some() {
            let _ = self.stop.send(());
            let _ = self.stop.send(());
        }
    }
}

async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The running agent's `local-api.json`, once it is there.
async fn api_file(data: &Path) -> LocalApiFile {
    let path = data.join(LOCAL_API_FILE);
    eventually("the local API is up", || path.exists()).await;
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

/// One raw HTTP/1.1 request, exactly as given (so a test controls every header): the status and the body.
fn raw(port: u16, head: &str, body: &str) -> (u16, String) {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let request = format!("{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let status = answer.split(' ').nth(1).unwrap().parse().unwrap();
    let body = answer.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default();
    (status, body)
}

/// A request from a well-behaved local client: the right `Host`, the token in its header.
fn call(api: &LocalApiFile, method: &str, path: &str, body: &str) -> (u16, String) {
    let head =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json", api.port, api.token);
    raw(api.port, &head, body)
}

fn status(api: &LocalApiFile) -> Status {
    let (code, body) = call(api, "GET", "/v1/status", "");
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn error_code(body: &str) -> String {
    serde_json::from_str::<ErrorBody>(body).unwrap().error
}

/// Every call of the local API, with a path that names something where it needs one.
const ROUTES: &[(&str, &str)] = &[
    ("GET", "/v1/status"),
    ("POST", "/v1/login"),
    ("POST", "/v1/logout"),
    ("GET", "/v1/recents"),
    ("POST", "/v1/pause"),
    ("POST", "/v1/resume"),
    ("POST", "/v1/quit"),
    ("POST", "/v1/join"),
    ("POST", "/v1/pairings"),
    ("GET", "/v1/pairings/0192f0c4-0000-7000-8000-000000000000"),
    ("DELETE", "/v1/pairings/0192f0c4-0000-7000-8000-000000000000"),
    ("GET", "/v1/devices"),
    ("GET", "/v1/usage"),
    ("DELETE", "/v1/devices/0192f0c4-0000-7000-8000-000000000000"),
    ("GET", "/v1/files/history?path=Notes%2Ftodo.txt"),
    ("POST", "/v1/files/restore"),
    ("GET", "/v1/trash?limit=1"),
    ("POST", "/v1/trash/restore"),
    ("POST", "/v1/trash/remove"),
    ("POST", "/v1/autostart"),
    ("POST", "/v1/login/second-factor"),
    ("GET", "/v1/account/two-factor"),
    ("POST", "/v1/account/two-factor/setup"),
    ("POST", "/v1/account/two-factor/confirm"),
    ("POST", "/v1/account/two-factor/disable"),
    ("POST", "/v1/account/two-factor/recovery-codes"),
    ("POST", "/v1/account/password"),
    ("POST", "/v1/devices/revoke-others"),
    ("PATCH", "/v1/devices/0192f0c4-0000-7000-8000-000000000000"),
    ("GET", "/v1/admin/server"),
    ("GET", "/v1/admin/storage"),
    ("GET", "/v1/admin/users"),
    ("POST", "/v1/admin/users"),
    ("PATCH", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000"),
    ("GET", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000/usage"),
    ("DELETE", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000"),
    ("POST", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000/disable"),
    ("POST", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000/enable"),
    ("POST", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000/invite"),
    ("DELETE", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000/invite"),
    ("POST", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000/password-reset"),
    ("POST", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000/reset-two-factor"),
    ("GET", "/v1/account"),
    ("GET", "/v1/account/activity"),
    ("GET", "/v1/admin/audit"),
    ("GET", "/v1/admin/audit/export"),
    ("GET", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000/devices"),
    ("DELETE", "/v1/admin/users/0192f0c4-0000-7000-8000-000000000000/devices/0192f0c4-0000-7000-8000-000000000001"),
];

/// A user session without systemd: Linux's entry is the XDG autostart file.
struct NoSystemd;

impl craft_cloud_agent::autostart::Runner for NoSystemd {
    fn run(&self, _program: &str, _args: &[&str]) -> std::io::Result<bool> {
        Ok(false)
    }
}

/// Test roots in `dir` (a temporary home, and on Windows a throwaway registry key, deleted with the guard), with
/// `default_data` as the default data folder.
fn test_roots(dir: &Path, default_data: &Path) -> (&'static craft_cloud_agent::autostart::Roots, RegistryKey) {
    let key = format!(r"Software\Craft Cloud Test\{}", uuid::Uuid::now_v7());
    let roots = craft_cloud_agent::autostart::Roots {
        home: dir.join("home"),
        run_key: key.clone(),
        runner: std::sync::Arc::new(NoSystemd),
        default_data: Some(default_data.to_owned()),
    };
    (Box::leak(Box::new(roots)), RegistryKey(key))
}

/// Deletes a test's throwaway registry key (Windows).
struct RegistryKey(#[allow(dead_code)] String);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        #[cfg(windows)]
        let _ = windows_registry::CURRENT_USER.remove_tree(&self.0);
    }
}

/// An agent with its own data folder (not the default one) never touches the start-at-login entry, which belongs to
/// the agent with the default data folder: it says nothing about it, refuses to set it, and signing out leaves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_with_its_own_data_folder_leaves_start_at_login_alone() {
    use craft_cloud_agent::autostart::{Launch, Target, is_registered, register};
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("spare"));
    let folder: PathBuf = dir.path().join("Spare folder");
    let (roots, _key) = test_roots(dir.path(), &dir.path().join("default data"));
    // The main agent's entry.
    let main = Launch { exe: dir.path().join("craft-cloud-agent-bg.exe"), log: None };
    register(roots, &main).unwrap();
    let running = Running::start_with(&data, RunOptions { autostart: Target::At(roots), ..quick() });
    let api = api_file(data.path()).await;
    assert_eq!(status(&api).autostart, None, "not this agent's to tell");
    let (code, body) = call(&api, "POST", "/v1/login", &login_body(&server, &folder, PASSWORD, false));
    assert_eq!(code, 200, "{body}");
    let (code, body) = call(&api, "POST", "/v1/autostart", r#"{"on":true}"#);
    assert_eq!(error_code(&body), codes::AUTOSTART_FAILED, "{code} {body}");
    assert!(body.contains("default data folder"), "{body}");
    let (code, body) = call(&api, "POST", "/v1/logout", "{}");
    assert_eq!(code, 200, "{body}");
    assert!(is_registered(roots).unwrap(), "the main agent's entry stays");
    let _ = running.stop().await;
}

/// Start at login is turned on through the local API by the agent with the default data folder (in temporary
/// places), the status says so, and signing out turns it off again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn autostart_is_turned_on_through_the_local_api_and_off_at_sign_out() {
    use craft_cloud_agent::autostart::{Target, is_registered};
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder: PathBuf = dir.path().join("Craft Cloud");
    let (roots, _key) = test_roots(dir.path(), data.path());
    let running = Running::start_with(&data, RunOptions { autostart: Target::At(roots), ..quick() });
    let api = api_file(data.path()).await;
    assert_eq!(status(&api).autostart, Some(false));
    let (code, body) = call(&api, "POST", "/v1/login", &login_body(&server, &folder, PASSWORD, false));
    assert_eq!(code, 200, "{body}");

    let (code, body) = call(&api, "POST", "/v1/autostart", r#"{"on":true}"#);
    if cfg!(windows) {
        // The test binary has no background build next to it: refused, and saying why.
        assert_eq!((code, error_code(&body).as_str()), (500, codes::AUTOSTART_FAILED), "{body}");
        assert!(body.contains("craft-cloud-agent-bg.exe"), "{body}");
        let _ = running.stop().await;
        return;
    }
    assert_eq!(code, 200, "{body}");
    assert_eq!(serde_json::from_str::<Status>(&body).unwrap().autostart, Some(true));
    assert!(is_registered(roots).unwrap());
    // It runs the agent as it is, with the default data folder, writing its own run.log there; on macOS what launchd
    // catches goes to launchd.log next to it.
    let entry = if cfg!(target_vendor = "apple") {
        craft_cloud_agent::autostart::macos::plist_file(roots)
    } else {
        craft_cloud_agent::autostart::linux::desktop_file(roots)
    };
    let written = std::fs::read_to_string(entry).unwrap();
    assert!(!written.contains("--data-dir"), "{written}");
    assert!(written.contains("--log"), "{written}");
    if cfg!(target_vendor = "apple") {
        assert!(written.contains(&data.path().join("launchd.log").display().to_string()), "{written}");
    }

    let (code, body) = call(&api, "POST", "/v1/logout", "{}");
    assert_eq!(code, 200, "{body}");
    assert_eq!(serde_json::from_str::<Status>(&body).unwrap().autostart, Some(false), "signing out turns it off");
    assert!(!is_registered(roots).unwrap());
    let _ = running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_local_api_answers_only_local_programs_with_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let running = Running::start(&data);
    let api = api_file(data.path()).await;
    assert_eq!(api.pid, std::process::id());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(data.path().join(LOCAL_API_FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "owner-only");
    }
    let signed_out = status(&api);
    assert!(!signed_out.signed_in && !signed_out.online);
    assert_eq!(signed_out.run_state, RunState::SignedOut);

    let (port, token) = (api.port, api.token.clone());
    let refused = |head: String| raw(port, &head, "");
    // Another Host (a web page that rebound its name to 127.0.0.1 sends its own), or none.
    for host in ["localhost", &format!("localhost:{port}"), &format!("127.0.0.1:{}", port.wrapping_add(1)), "evil.example", "127.0.0.1"] {
        let (code, body) = refused(format!("GET /v1/status HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}"));
        assert_eq!((code, error_code(&body).as_str()), (403, codes::FORBIDDEN), "Host {host}");
    }
    let (code, _) = refused(format!("GET /v1/status HTTP/1.0\r\nAuthorization: Bearer {token}"));
    assert!(code == 403 || code == 400, "no Host: {code}");
    // Any Origin, even one naming this address.
    for origin in ["https://evil.example", &format!("http://127.0.0.1:{port}"), "null"] {
        let (code, body) = refused(format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {origin}\r\nAuthorization: Bearer {token}"));
        assert_eq!((code, error_code(&body).as_str()), (403, codes::FORBIDDEN), "Origin {origin}");
    }
    // The token only counts in its header: in the query it is ignored, and without it nothing is answered.
    for query in [format!("?token={token}"), format!("?access_token={token}"), format!("?authorization=Bearer%20{token}")] {
        let (code, body) = refused(format!("GET /v1/status{query} HTTP/1.1\r\nHost: 127.0.0.1:{port}"));
        assert_eq!((code, error_code(&body).as_str()), (401, "unauthorized"), "{query}");
    }
    let (code, _) = refused(format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{port}"));
    assert_eq!(code, 401, "missing token");
    // The token with its last digit changed.
    let wrong = format!("{}{}", &token[..token.len() - 1], if token.ends_with('0') { '1' } else { '0' });
    for auth in [format!("Bearer {wrong}"), format!("Bearer {token}x"), token.clone(), format!("Basic {token}"), "Bearer ".into()] {
        let (code, _) = refused(format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: {auth}"));
        assert_eq!(code, 401, "Authorization: {auth}");
    }
    // Every call has the same checks: a wrong Host, an Origin, the token in the query, or a missing or wrong one is
    // refused before the call does anything.
    for (method, path) in ROUTES {
        let wrong_host = format!("{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nAuthorization: Bearer {token}");
        let origin = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: null\r\nAuthorization: Bearer {token}");
        let in_query = format!("{method} {path}?token={token} HTTP/1.1\r\nHost: 127.0.0.1:{port}");
        let missing = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}");
        let wrong_token = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {wrong}");
        for (head, want) in [(wrong_host, 403), (origin, 403), (in_query, 401), (missing, 401), (wrong_token, 401)] {
            let (code, body) = raw(port, &format!("{head}\r\nContent-Type: application/json"), "{}");
            assert_eq!(code, want, "{method} {path}: {head:?}: {body}");
        }
    }
    // Signed out: nothing to pause or resume.
    for path in ["/v1/pause", "/v1/resume"] {
        let (code, body) = call(&api, "POST", path, "");
        assert_eq!((code, error_code(&body).as_str()), (409, codes::NOT_SIGNED_IN), "{path}");
    }
    // A sign-in with a bad token never reaches the loop.
    let login = r#"{"server":"http://127.0.0.1:1","user":"ana","password":"x"}"#;
    let (code, _) = raw(port, &format!("POST /v1/login HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json"), login);
    assert_eq!(code, 401);
    // The token never shows in what `run` prints.
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(!out.contains(&token), "{out}");
    assert!(!data.path().join(LOCAL_API_FILE).exists(), "removed on a clean exit");
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err() || raw(port, "GET / HTTP/1.1\r\nHost: x", "").0 != 200);
}

fn files(folder: &Path) -> Vec<String> {
    let mut out: Vec<String> =
        std::fs::read_dir(folder).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).filter(|n| n != ".craft-cloud").collect();
    out.sort();
    out
}

fn login_body(server: &Server, folder: &Path, password: &str, adopt: bool) -> String {
    serde_json::to_string(&LoginRequest {
        server: server.url.clone(),
        user: "ana".into(),
        password: password.into(),
        folder: Some(folder.display().to_string()),
        device_name: Some("laptop".into()),
        adopt,
    })
    .unwrap()
}

async fn server_has(server: &Server, name: &str) -> bool {
    let ana = server.meta.user_by_name("ana").await.unwrap().unwrap();
    let tree = server.meta.tree(ana.space, None, 1000).await.unwrap();
    tree.nodes.iter().any(|n| n.name.as_ref().is_some_and(|x| x.as_str() == name) && !n.deleted)
}

/// A sign-out that revoked the device but couldn't remove the account file must not sign straight back in with
/// the dead token: the agent stays signed out (and says what to remove).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sign_out_that_cant_forget_the_account_stays_signed_out() {
    use std::os::unix::fs::PermissionsExt;
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder: PathBuf = dir.path().join("Craft Cloud");
    let running = Running::start(&data);
    let api = api_file(data.path()).await;
    let (code, body) = call(&api, "POST", "/v1/login", &login_body(&server, &folder, PASSWORD, false));
    assert_eq!(code, 200, "{body}");
    eventually("signed in and idle", || status(&api).run_state == RunState::Idle).await;
    // The account file can't be removed now.
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let (code, body) = call(&api, "POST", "/v1/logout", "{}");
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_ne!(code, 200, "the failure is told: {body}");
    assert!(data.load_account().unwrap().is_some(), "the file is still there");
    let after = status(&api);
    assert!(!after.signed_in, "signed out, not back in with the revoked device");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(status(&api).run_state, RunState::SignedOut, "and not syncing");
    let _ = running.stop().await;
}

/// Whoever finds the local API up finds `status.json` saying the loop runs (a second `run`, then `status`, relies on
/// it), even when another program holds the old `status.json` for a moment as the loop starts: the first status is
/// on disk before the API answers.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn once_the_local_api_is_up_status_json_says_the_loop_runs() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    std::fs::create_dir_all(data.path()).unwrap();
    // A status an agent left long ago, held (for reading only, no delete sharing) for the first second.
    let file = data.path().join("status.json");
    std::fs::write(&file, b"{}").unwrap();
    let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&file).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        drop(held);
    });
    let running = Running::start(&data);
    api_file(data.path()).await;
    let status = data.load_status();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert!(matches!(&status, Ok(Some(s)) if s.running(now)), "{status:?}");
    release.join().unwrap();
    let _ = running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_starts_signed_out_and_signs_in_and_out_through_the_local_api() {
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder: PathBuf = dir.path().join("Craft Cloud");
    let running = Running::start(&data);
    let api = api_file(data.path()).await;
    eventually("the loop says it waits for a sign-in", || data.load_status().unwrap().is_some_and(|s| s.state == LoopState::SignedOut)).await;
    let mut shown = Vec::new();
    cli::status(&data, &mut shown).unwrap();
    let shown = String::from_utf8(shown).unwrap();
    assert!(shown.contains("not signed in") && shown.contains("waiting for a sign-in"), "{shown}");

    // Not signed in: nothing to sign out of.
    let (code, body) = call(&api, "POST", "/v1/logout", "{}");
    assert_eq!((code, error_code(&body).as_str()), (409, codes::NOT_SIGNED_IN));
    // A wrong password changes nothing.
    let (code, body) = call(&api, "POST", "/v1/login", &login_body(&server, &folder, "wrong password", false));
    assert_eq!((code, error_code(&body).as_str()), (401, codes::WRONG_PASSWORD), "{body}");
    assert!(data.load_account().unwrap().is_none());

    let (code, body) = call(&api, "POST", "/v1/login", &login_body(&server, &folder, PASSWORD, false));
    assert_eq!(code, 200, "{body}");
    let signed_in: Status = serde_json::from_str(&body).unwrap();
    assert!(signed_in.signed_in);
    assert_eq!(signed_in.account.as_deref(), Some("ana"));
    assert_eq!(signed_in.server.as_deref(), Some(server.url.as_str()));
    assert_eq!(signed_in.machine.as_deref(), Some("laptop"));
    assert!(!body.contains(PASSWORD));
    assert!(data.load_account().unwrap().is_some());
    // Signed in, the loop syncs.
    std::fs::write(folder.join("notes.txt"), "from the laptop").unwrap();
    let deadline = Instant::now() + PATIENCE;
    while !server_has(&server, "notes.txt").await {
        assert!(Instant::now() < deadline, "the file reaches the server");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eventually("the local API reports it online and idle", || {
        let s = status(&api);
        s.online && s.run_state == RunState::Idle
    })
    .await;
    // A second sign-in is refused while signed in.
    let (code, body) = call(&api, "POST", "/v1/login", &login_body(&server, &folder, PASSWORD, false));
    assert_eq!((code, error_code(&body).as_str()), (409, codes::ALREADY_SIGNED_IN));

    // Signing out revokes the device, stops syncing and forgets the account; the files stay.
    let (code, body) = call(&api, "POST", "/v1/logout", "{}");
    assert_eq!(code, 200, "{body}");
    let out: Status = serde_json::from_str(&body).unwrap();
    assert!(!out.signed_in);
    assert!(data.load_account().unwrap().is_none());
    assert_eq!(files(&folder), vec!["notes.txt".to_owned()]);
    let ana = server.meta.user_by_name("ana").await.unwrap().unwrap();
    assert!(server.meta.list_devices(ana.id).await.unwrap().iter().all(|d| d.revoked), "the device is revoked");
    std::fs::write(folder.join("later.txt"), "after signing out").unwrap();
    eventually("the loop waits for a sign-in again", || data.load_status().unwrap().is_some_and(|s| s.state == LoopState::SignedOut)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!server_has(&server, "later.txt").await, "nothing syncs while signed out");

    // And in again: the same folder (it has files now, so it is adopted), syncing picks up what came meanwhile.
    let (code, body) = call(&api, "POST", "/v1/login", &login_body(&server, &folder, PASSWORD, true));
    assert_eq!(code, 200, "{body}");
    let deadline = Instant::now() + PATIENCE;
    while !server_has(&server, "later.txt").await {
        assert!(Instant::now() < deadline, "the file made while signed out reaches the server");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(out.contains("signed in as ana") && out.contains("signed out of"), "{out}");
    assert!(!out.contains(PASSWORD), "{out}");
}

/// Signs `run` in through its local API and waits until it is idle.
async fn signed_in(server: &Server, data: &DataDir, folder: &Path) -> (Running, LocalApiFile) {
    let running = Running::start(data);
    let api = api_file(data.path()).await;
    let (code, body) = call(&api, "POST", "/v1/login", &login_body(server, folder, PASSWORD, false));
    assert_eq!(code, 200, "{body}");
    eventually("signed in and idle", || status(&api).run_state == RunState::Idle).await;
    (running, api)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_reads_through_to_the_server_without_caching() {
    let server = server().await;
    let data = DataDir::new(tempfile::tempdir().unwrap().keep());
    let folder = tempfile::tempdir().unwrap();
    let (_running, api) = signed_in(&server, &data, folder.path()).await;
    let account = data.load_account().unwrap().unwrap();
    let transport = craft_cloud_agent::http::HttpTransport::new(account.server.clone(), &account.token).unwrap();
    std::fs::write(folder.path().join("usage-first.bin"), b"first payload").unwrap();
    reaches_the_server(&server, "usage-first.bin").await;
    let server_first = transport.usage().await.unwrap();
    let (code, body) = call(&api, "GET", "/v1/usage", "");
    assert_eq!(code, 200, "{body}");
    let local_first: craft_cloud_proto::api::AccountUsage = serde_json::from_str(&body).unwrap();
    assert_eq!(local_first, server_first, "the local API passes nonzero numbers through unchanged");
    assert!(local_first.stored_bytes > 0);

    std::fs::write(folder.path().join("usage-second.bin"), b"a different payload for fresh usage").unwrap();
    reaches_the_server(&server, "usage-second.bin").await;
    let server_second = transport.usage().await.unwrap();
    let (code, body) = call(&api, "GET", "/v1/usage", "");
    assert_eq!(code, 200, "{body}");
    let local_second: craft_cloud_proto::api::AccountUsage = serde_json::from_str(&body).unwrap();
    assert_eq!(local_second, server_second, "a second request sees server changes; no cache");
    assert!(local_second.stored_bytes > local_first.stored_bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_refuses_signed_out_and_reports_server_unreachable() {
    let server = server().await;
    let data = DataDir::new(tempfile::tempdir().unwrap().keep());
    let running = Running::start(&data);
    let api = api_file(data.path()).await;
    let (code, body) = call(&api, "GET", "/v1/usage", "");
    assert_eq!(code, 409, "{body}");
    assert_eq!(error_code(&body), codes::NOT_SIGNED_IN);
    let _ = running.stop().await;

    let folder = tempfile::tempdir().unwrap();
    let (running, api) = signed_in(&server, &data, folder.path()).await;
    server.task.abort();
    let (code, body) = call(&api, "GET", "/v1/usage", "");
    assert_eq!(code, 503, "{body}");
    assert_eq!(error_code(&body), codes::OFFLINE);
    drop(running);
}

async fn reaches_the_server(server: &Server, name: &str) {
    let deadline = Instant::now() + PATIENCE;
    while !server_has(server, name).await {
        assert!(Instant::now() < deadline, "{name} reaches the server");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn history_page(api: &LocalApiFile, path: &str, cursor: Option<&str>, limit: u32) -> HistoryResponse {
    let mut query = format!("path={path}&limit={limit}");
    if let Some(cursor) = cursor {
        query.push_str("&cursor=");
        query.push_str(cursor);
    }
    let (code, body) = call(api, "GET", &format!("/v1/files/history?{query}"), "");
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_file_history_pages_restores_and_syncs_to_disk() {
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder = dir.path().join("Craft Cloud");
    let (running, api) = signed_in(&server, &data, &folder).await;
    let subdir = folder.join("Notes");
    std::fs::create_dir(&subdir).unwrap();
    let file = subdir.join("story.txt");

    let mut previous = None;
    for content in [b"first version".as_slice(), b"second version".as_slice(), b"third version".as_slice()] {
        std::fs::write(&file, content).unwrap();
        let deadline = Instant::now() + PATIENCE;
        loop {
            let (code, body) = call(&api, "GET", "/v1/files/history?path=Notes%2Fstory.txt&limit=1", "");
            if code == 200 {
                let page: HistoryResponse = serde_json::from_str(&body).unwrap();
                if page.versions.first().is_some_and(|version| Some(version.id) != previous) {
                    previous = page.versions.first().map(|version| version.id);
                    break;
                }
            }
            assert!(Instant::now() < deadline, "version reaches server history");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let newest = history_page(&api, "Notes%2Fstory.txt", None, 1).await;
    let cursor = newest.next.clone().expect("more than one history page");
    let older = history_page(&api, "Notes%2Fstory.txt", Some(&cursor), 1).await;
    let current = newest.versions.first().expect("current version").id;
    let restore = older.versions.first().expect("older version").id;
    let body = serde_json::json!({ "path": "Notes/story.txt", "version": restore, "expected_version": current }).to_string();
    let (code, answer) = call(&api, "POST", "/v1/files/restore", &body);
    assert_eq!(code, 200, "{answer}");
    let restored: craft_cloud_proto::api::RestoreResponse = serde_json::from_str(&answer).unwrap();
    assert_eq!(restored.parent_version, current);

    let stale = serde_json::json!({ "path": "Notes/story.txt", "version": current, "expected_version": current }).to_string();
    let (code, answer) = call(&api, "POST", "/v1/files/restore", &stale);
    assert_eq!((code, error_code(&answer).as_str()), (409, "conflict"), "{answer}");

    let deadline = Instant::now() + PATIENCE;
    loop {
        if std::fs::read(&file).is_ok_and(|bytes| bytes.as_slice() == b"second version") {
            break;
        }
        assert!(Instant::now() < deadline, "the change feed syncs restored bytes to disk");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (exit, output) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{output}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_file_history_rejects_unknown_outside_and_directory_paths() {
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder = dir.path().join("Craft Cloud");
    let (running, api) = signed_in(&server, &data, &folder).await;
    std::fs::create_dir(folder.join("Folder")).unwrap();
    let deadline = Instant::now() + PATIENCE;
    loop {
        let (code, body) = call(&api, "GET", "/v1/files/history?path=Folder", "");
        if code == 400 {
            assert_eq!(error_code(&body), "invalid");
            break;
        }
        assert!(Instant::now() < deadline, "directory enters sync state");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (code, body) = call(&api, "GET", "/v1/files/history?path=missing.txt", "");
    assert_eq!((code, error_code(&body).as_str()), (404, "not_found"), "{body}");
    let (code, body) = call(&api, "GET", "/v1/files/history?path=..%2Foutside.txt", "");
    assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "{body}");
    let _ = running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_trash_pages_restores_renames_and_removes_nodes() {
    use craft_cloud_proto::local::TrashResponse;
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder = dir.path().join("Craft Cloud");
    let (running, api) = signed_in(&server, &data, &folder).await;

    std::fs::write(folder.join("first.txt"), b"first bytes").unwrap();
    std::fs::write(folder.join("second.txt"), b"second bytes").unwrap();
    reaches_the_server(&server, "first.txt").await;
    reaches_the_server(&server, "second.txt").await;
    std::fs::remove_file(folder.join("first.txt")).unwrap();
    std::fs::remove_file(folder.join("second.txt")).unwrap();
    let deadline = Instant::now() + PATIENCE;
    let page = loop {
        let (code, body) = call(&api, "GET", "/v1/trash?limit=1", "");
        if code == 200 {
            let page: TrashResponse = serde_json::from_str(&body).unwrap();
            if page.items.len() == 1 && page.next.is_some() {
                break page;
            }
        }
        assert!(Instant::now() < deadline, "both deleted files enter the trash");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let first = page.items.first().unwrap();
    assert_eq!(first.path.as_deref(), Some(first.name.as_str()), "live root parent yields a path hint");
    let cursor = page.next.as_deref().unwrap();
    let (code, body) = call(&api, "GET", &format!("/v1/trash?limit=1&cursor={cursor}"), "");
    assert_eq!(code, 200, "{body}");
    let second_page: TrashResponse = serde_json::from_str(&body).unwrap();
    let second = second_page.items.first().unwrap();
    assert_ne!(first.id, second.id);

    let restore_body = serde_json::json!({ "id": first.id }).to_string();
    let (code, body) = call(&api, "POST", "/v1/trash/restore", &restore_body);
    assert_eq!(code, 200, "{body}");
    let restored: craft_cloud_proto::api::TrashRestoreResponse = serde_json::from_str(&body).unwrap();
    assert!(!restored.renamed);
    let restored_path = folder.join(&restored.name);
    let deadline = Instant::now() + PATIENCE;
    loop {
        if std::fs::read(&restored_path)
            .is_ok_and(|bytes| bytes.as_slice() == if first.name == "first.txt" { &b"first bytes"[..] } else { &b"second bytes"[..] })
        {
            break;
        }
        assert!(Instant::now() < deadline, "the agent syncs restored bytes to disk");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Re-delete the restored ID, create a live node with its old name, then verify conflict rename on restore.
    std::fs::remove_file(&restored_path).unwrap();
    let deadline = Instant::now() + PATIENCE;
    let mut trashed_again = false;
    while !trashed_again {
        let (code, body) = call(&api, "GET", "/v1/trash?limit=50", "");
        if code == 200 {
            let page: TrashResponse = serde_json::from_str(&body).unwrap();
            trashed_again = page.items.iter().any(|item| item.id == restored.node);
        }
        assert!(Instant::now() < deadline, "restored node is deleted again");
        if !trashed_again {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    std::fs::write(&restored_path, b"replacement bytes").unwrap();
    reaches_the_server(&server, &restored.name).await;
    let (code, body) = call(&api, "POST", "/v1/trash/restore", &serde_json::json!({ "id": restored.node }).to_string());
    assert_eq!(code, 200, "{body}");
    let renamed: craft_cloud_proto::api::TrashRestoreResponse = serde_json::from_str(&body).unwrap();
    assert!(renamed.renamed, "{renamed:?}");
    assert_ne!(renamed.name, restored.name);

    let (code, body) = call(&api, "POST", "/v1/trash/remove", &serde_json::json!({ "id": second.id }).to_string());
    assert_eq!(code, 204, "{body}");
    let (code, body) = call(&api, "GET", "/v1/trash?limit=50", "");
    assert_eq!(code, 200, "{body}");
    let remaining: TrashResponse = serde_json::from_str(&body).unwrap();
    assert!(remaining.items.iter().all(|item| item.id != second.id));
    let (code, body) = call(&api, "POST", "/v1/trash/restore", &serde_json::json!({ "id": second.id }).to_string());
    assert_eq!((code, error_code(&body).as_str()), (404, "not_found"), "{body}");
    let (code, body) = call(&api, "POST", "/v1/trash/restore", &serde_json::json!({ "id": "invalid" }).to_string());
    assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "{body}");
    let unknown = craft_cloud_proto::NodeId::generate();
    let (code, body) = call(&api, "POST", "/v1/trash/restore", &serde_json::json!({ "id": unknown }).to_string());
    assert_eq!((code, error_code(&body).as_str()), (404, "not_found"), "{body}");
    let _ = running.stop().await;
}

/// Paused, nothing syncs and the status says so (not as a pause that needs someone); it stays paused after `run`
/// starts again, until it is resumed. Quit then ends `run` cleanly, and its `local-api.json` with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pause_holds_syncing_across_a_restart_until_resume() {
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder: PathBuf = dir.path().join("Craft Cloud");
    let (running, api) = signed_in(&server, &data, &folder).await;
    std::fs::write(folder.join("before.txt"), "synced").unwrap();
    reaches_the_server(&server, "before.txt").await;

    let (code, body) = call(&api, "POST", "/v1/pause", "");
    assert_eq!(code, 200, "{body}");
    let paused: Status = serde_json::from_str(&body).unwrap();
    assert_eq!(paused.run_state, RunState::PausedByUser);
    assert!(!paused.online, "the live link holds too");
    let (code, _) = call(&api, "POST", "/v1/pause", "");
    assert_eq!(code, 200, "pausing twice is fine");
    std::fs::write(folder.join("paused.txt"), "waits").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!server_has(&server, "paused.txt").await, "nothing syncs while paused");
    assert_eq!(status(&api).run_state, RunState::PausedByUser);
    eventually("status.json says paused", || data.load_status().unwrap().is_some_and(|s| s.state == LoopState::PausedByUser)).await;
    let mut shown = Vec::new();
    cli::status(&data, &mut shown).unwrap();
    assert!(String::from_utf8(shown).unwrap().contains("paused"));

    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(data.load_state().unwrap().paused, "kept in state.json");
    let running = Running::start(&data);
    let api = api_file(data.path()).await;
    eventually("paused again after the restart", || status(&api).run_state == RunState::PausedByUser).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!server_has(&server, "paused.txt").await, "still nothing syncs");

    let (code, body) = call(&api, "POST", "/v1/resume", "");
    assert_eq!(code, 200, "{body}");
    assert_ne!(serde_json::from_str::<Status>(&body).unwrap().run_state, RunState::PausedByUser);
    reaches_the_server(&server, "paused.txt").await;
    eventually("idle and online", || {
        let s = status(&api);
        s.online && s.run_state == RunState::Idle
    })
    .await;
    assert!(!data.load_state().unwrap().paused);

    let (code, body) = call(&api, "POST", "/v1/quit", "");
    assert_eq!(code, 200, "{body}");
    let thread = { running }.thread_handle();
    let (exit, out) = tokio::task::spawn_blocking(move || thread.join().unwrap()).await.unwrap();
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(!data.path().join(LOCAL_API_FILE).exists(), "gone with the agent");
}

/// Quit while signed out ends `run` too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quit_ends_a_signed_out_agent() {
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let running = Running::start(&data);
    let api = api_file(data.path()).await;
    let (code, body) = call(&api, "POST", "/v1/quit", "");
    assert_eq!(code, 200, "{body}");
    let thread = { running }.thread_handle();
    let (exit, out) = tokio::task::spawn_blocking(move || thread.join().unwrap()).await.unwrap();
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(!data.path().join(LOCAL_API_FILE).exists());
}

/// Something only a person can put right, found as `run` starts (here a damaged `account.json`): the agent stays up,
/// serving the local API, and its status says what is wrong (`Paused`), so the app can show it; started at login, it
/// isn't restarted over and over. Signing out forgets the damaged account, and the agent waits for a sign-in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_problem_found_at_start_keeps_the_agent_up_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    std::fs::create_dir_all(data.path()).unwrap();
    std::fs::write(data.path().join("account.json"), "{not json").unwrap();
    let running = Running::start(&data);
    let api = api_file(data.path()).await;
    let now = status(&api);
    assert!(!now.signed_in, "{now:?}");
    let RunState::Paused { reason, .. } = &now.run_state else { panic!("{now:?}") };
    assert!(reason.contains("account.json"), "{reason}");
    // Nothing else can be done until it is put right.
    let (code, body) = call(&api, "POST", "/v1/join", r#"{"join":"c.example/K7M4-Q9TX"}"#);
    assert_eq!((code, error_code(&body).as_str()), (409, codes::NEEDS_YOU), "{body}");
    let (code, body) = call(&api, "POST", "/v1/pause", "");
    assert_eq!((code, error_code(&body).as_str()), (409, codes::NEEDS_YOU), "{body}");
    // Signing out forgets it.
    let (code, body) = call(&api, "POST", "/v1/logout", r#"{"local":true}"#);
    assert_eq!(code, 200, "{body}");
    assert!(!data.path().join("account.json").exists());
    eventually("it waits for a sign-in", || status(&api).run_state == RunState::SignedOut).await;
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
}

/// Apps' recents keep working while paused: an add is kept on this machine and reaches the server after resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recents_added_while_paused_reach_the_server_after_resume() {
    use craft_cloud_proto::live::{AppName, recents_doc};
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder: PathBuf = dir.path().join("Craft Cloud");
    let (running, api) = signed_in(&server, &data, &folder).await;
    eventually("online", || status(&api).online).await;
    let (code, _) = call(&api, "POST", "/v1/pause", "");
    assert_eq!(code, 200);
    let dir_path = data.path().to_owned();
    let added = tokio::task::spawn_blocking(move || {
        let client = craft_cloud_app::Client::connect_in(&dir_path).unwrap();
        client.add_recent("wordcraft", craft_cloud_app::NewRecent { path: "Letters/paused.docx".into(), title: None, saved: true }).unwrap();
        client.recents("wordcraft").unwrap()
    })
    .await
    .unwrap();
    assert_eq!(added.len(), 1, "the list here has it at once");
    let ana = server.meta.user_by_name("ana").await.unwrap().unwrap();
    let doc = recents_doc(ana.space, &AppName::new("wordcraft").unwrap());
    let on_server = || async { server.meta.live_doc(ana.space, doc).await.unwrap().is_some_and(|d| d.head > 0) };
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!on_server().await, "not sent while paused");
    let (code, _) = call(&api, "POST", "/v1/resume", "");
    assert_eq!(code, 200);
    let deadline = Instant::now() + PATIENCE;
    while !on_server().await {
        assert!(Instant::now() < deadline, "the entry reaches the server after resume");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = running.stop().await;
}

// ---- joining with a code ----

/// A second machine: its own data folder and synced folder, `run` started and signed out.
struct Machine {
    data: DataDir,
    folder: PathBuf,
    running: Option<Running>,
    api: LocalApiFile,
    _dir: tempfile::TempDir,
}

impl Machine {
    async fn signed_out() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path().join("data"));
        let folder = dir.path().join("Craft Cloud");
        let running = Running::start(&data);
        let api = api_file(data.path()).await;
        Self { data, folder, running: Some(running), api, _dir: dir }
    }

    fn join_body(&self, join: &str, adopt: bool, password: Option<&str>) -> String {
        serde_json::to_string(&JoinRequest {
            join: join.into(),
            folder: Some(self.folder.display().to_string()),
            device_name: Some("lptp1".into()),
            adopt,
            password: password.map(str::to_owned),
            totp_code: None,
            recovery_code: None,
        })
        .unwrap()
    }

    fn join(&self, join: &str, adopt: bool, password: Option<&str>) -> (u16, String) {
        call(&self.api, "POST", "/v1/join", &self.join_body(join, adopt, password))
    }

    async fn stop(mut self) {
        if let Some(running) = self.running.take() {
            let _ = running.stop().await;
        }
    }
}

fn new_pairing(api: &LocalApiFile) -> PairingInfo {
    let (code, body) = call(api, "POST", "/v1/pairings", "");
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn pairing_status(api: &LocalApiFile, id: uuid::Uuid) -> PairingStatus {
    let (code, body) = call(api, "GET", &format!("/v1/pairings/{id}"), "");
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn devices(api: &LocalApiFile) -> Vec<DeviceInfo> {
    let (code, body) = call(api, "GET", "/v1/devices", "");
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// Machine A, signed in, makes an add-a-computer code; machine B joins with it through its local API and syncs; A
/// sees who joined, lists both computers and removes B, which then shows that it was removed. A can't remove itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_computer_joins_with_a_code_from_the_first() {
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder: PathBuf = dir.path().join("Craft Cloud");
    let (running, a) = signed_in(&server, &data, &folder).await;
    std::fs::write(folder.join("from-a.txt"), "made on A").unwrap();
    reaches_the_server(&server, "from-a.txt").await;

    let pairing = new_pairing(&a);
    let parsed = craft_cloud_proto::join::JoinString::parse(&pairing.join).unwrap();
    assert_eq!(parsed.server, server.url);
    assert_eq!(craft_cloud_proto::join::JoinString::parse(&pairing.link).unwrap(), parsed);
    assert!(pairing.link.contains("/join/"), "{}", pairing.link);
    assert_eq!(pairing_status(&a, pairing.id).state, PairingState::Waiting);

    let b = Machine::signed_out().await;
    let (code, body) = b.join(&format!(" {} ", pairing.join.to_lowercase()), false, None);
    assert_eq!(code, 200, "{body}");
    let joined: Status = serde_json::from_str(&body).unwrap();
    assert!(joined.signed_in);
    assert_eq!((joined.account.as_deref(), joined.machine.as_deref()), (Some("ana"), Some("lptp1")));
    assert!(!body.contains(parsed.code.as_str()), "the code isn't echoed");
    assert_eq!(pairing_status(&a, pairing.id), PairingStatus { state: PairingState::Used, device_name: Some("lptp1".into()) });
    eventually("A's file reaches B", || b.folder.join("from-a.txt").exists()).await;
    // Signed in, B can't join again.
    let (code, body) = b.join(&pairing.join, false, None);
    assert_eq!((code, error_code(&body).as_str()), (409, codes::ALREADY_SIGNED_IN), "{body}");

    let both = devices(&a);
    assert_eq!(both.len(), 2, "{both:?}");
    let me = both.iter().find(|d| d.current).unwrap();
    let other = both.iter().find(|d| !d.current).unwrap();
    assert_eq!(other.name, "lptp1");
    let (code, body) = call(&a, "DELETE", &format!("/v1/devices/{}", me.id), "");
    assert_eq!((code, error_code(&body).as_str()), (409, codes::CANNOT_REMOVE_SELF), "{body}");
    let (code, body) = call(&a, "DELETE", &format!("/v1/devices/{}", other.id), "");
    assert_eq!(code, 200, "{body}");
    let left: Vec<DeviceInfo> = serde_json::from_str(&body).unwrap();
    assert!(left.iter().any(|d| d.id == other.id && d.revoked), "{left:?}");
    assert!(!status(&b.api).removed);
    // B finds out at its next request.
    std::fs::write(b.folder.join("from-b.txt"), "made on B").unwrap();
    eventually(
        "B says it was removed",
        || matches!(status(&b.api).run_state, RunState::Paused { ref reason, .. } if reason.contains("no longer accepts")),
    )
    .await;
    assert!(status(&b.api).removed, "told apart from other stops, for the app");
    b.stop().await;
    let _ = running.stop().await;
}

/// Renaming this computer is kept here too (the status, the saved account), so the app's title agrees with the computers
/// list; renaming another computer changes nothing here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_this_computer_updates_its_own_status_but_another_one_does_not() {
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder: PathBuf = dir.path().join("Craft Cloud");
    let (running, a) = signed_in(&server, &data, &folder).await;
    let before = status(&a).machine.unwrap();

    let pairing = new_pairing(&a);
    let b = Machine::signed_out().await;
    let (code, body) = b.join(&pairing.join, false, None);
    assert_eq!(code, 200, "{body}");
    let both = devices(&a);
    let me = both.iter().find(|d| d.current).unwrap();
    let other = both.iter().find(|d| !d.current).unwrap();

    // Another computer: the server has the name, this agent's status and account keep theirs.
    let (code, body) = call(&a, "PATCH", &format!("/v1/devices/{}", other.id), r#"{"name":"the other one"}"#);
    assert_eq!(code, 200, "{body}");
    assert_eq!(status(&a).machine.as_deref(), Some(before.as_str()));
    assert_eq!(data.load_account().unwrap().unwrap().device_name, before);

    // This computer: the status and the saved account say the new name at once.
    let (code, body) = call(&a, "PATCH", &format!("/v1/devices/{}", me.id), r#"{"name":"my new name"}"#);
    assert_eq!(code, 200, "{body}");
    assert_eq!(status(&a).machine.as_deref(), Some("my new name"));
    assert_eq!(data.load_account().unwrap().unwrap().device_name, "my new name");
    assert!(devices(&a).iter().any(|d| d.current && d.name == "my new name"));
    // A refused rename (an empty name) leaves the name as it is.
    let (code, _) = call(&a, "PATCH", &format!("/v1/devices/{}", me.id), r#"{"name":""}"#);
    assert_ne!(code, 200);
    assert_eq!(status(&a).machine.as_deref(), Some("my new name"));
    b.stop().await;
    let _ = running.stop().await;
}

/// What can be checked before the code is spent is checked first: a folder with files and no `adopt` leaves the code
/// usable. What can only fail after it (a marker for another space in the folder) removes the new device again and
/// says to make a new code.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_problems_found_before_the_code_is_spent_leave_it_usable() {
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder: PathBuf = dir.path().join("Craft Cloud");
    let (running, a) = signed_in(&server, &data, &folder).await;
    let b = Machine::signed_out().await;
    let pairing = new_pairing(&a);

    std::fs::create_dir_all(&b.folder).unwrap();
    std::fs::write(b.folder.join("mine.txt"), "already here").unwrap();
    let (code, body) = b.join(&pairing.join, false, None);
    assert_eq!((code, error_code(&body).as_str()), (400, codes::FOLDER_HAS_FILES), "{body}");
    assert!(body.contains("adopt") || body.contains("files in it"), "{body}");
    assert_eq!(pairing_status(&a, pairing.id).state, PairingState::Waiting, "the code is still good");
    // Garbage never reaches the server.
    for bad in ["cloud.example.com", "K7M4-Q9TX", "cloud.example.com/K7M4-Q9TU", ""] {
        let (code, body) = b.join(bad, true, None);
        assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "{bad:?}: {body}");
    }
    // A code that doesn't work: the server's one answer.
    let wrong = craft_cloud_proto::join::JoinString::new(server.url.clone(), craft_cloud_proto::join::Code::generate(8).unwrap());
    let (code, body) = b.join(&wrong.join(), true, None);
    assert_eq!((code, error_code(&body).as_str()), (404, codes::INVALID_CODE), "{body}");
    assert!(body.contains("Make a new one on a signed-in computer"), "{body}");

    // A marker for another space: only found once the new device knows its space.
    let other = craft_cloud_agent::root::RootMarker { space: craft_cloud_proto::SpaceId::generate(), root: craft_cloud_proto::NodeId::generate() };
    craft_cloud_agent::root::write(&b.folder, &other).unwrap();
    let (code, body) = b.join(&pairing.join, true, None);
    assert_ne!(code, 200, "{body}");
    assert!(body.contains("make a new one"), "{body}");
    assert!(b.data.load_account().unwrap().is_none());
    let ana = server.meta.user_by_name("ana").await.unwrap().unwrap();
    let list = server.meta.list_devices(ana.id).await.unwrap();
    assert!(list.iter().any(|d| d.name == "lptp1" && d.revoked), "the new device is removed again: {list:?}");
    assert!(!list.iter().any(|d| d.name == "lptp1" && !d.revoked));
    assert_eq!(pairing_status(&a, pairing.id).state, PairingState::Used, "spent");
    b.stop().await;
    let _ = running.stop().await;
}

/// An invite to a new account: the agent passes `password_required` on as it is, and joins once a password comes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invite_to_a_new_account_asks_for_a_password_through_the_agent() {
    use craft_cloud_proto::join::{Code, INVITE_CHARS, INVITE_TTL_MS, JoinString};
    let server = server().await;
    let invite = Code::generate(INVITE_CHARS).unwrap();
    let now = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap();
    server.meta.create_invite("cleo", &invite.hash(), now, INVITE_TTL_MS).await.unwrap();
    let join = JoinString::new(server.url.clone(), invite).link();
    let b = Machine::signed_out().await;
    let (code, body) = b.join(&join, false, None);
    assert_eq!((code, error_code(&body).as_str()), (400, codes::PASSWORD_REQUIRED), "{body}");
    let (code, body) = b.join(&join, false, Some("short"));
    assert_eq!((code, error_code(&body).as_str()), (400, codes::WEAK_PASSWORD), "{body}");
    assert!(b.data.load_account().unwrap().is_none());
    let (code, body) = b.join(&join, false, Some(PASSWORD));
    assert_eq!(code, 200, "{body}");
    assert!(!body.contains(PASSWORD));
    let joined: Status = serde_json::from_str(&body).unwrap();
    assert_eq!(joined.account.as_deref(), Some("cleo"));
    b.stop().await;
}

// ---- the admin calls (`/v1/admin/…`) ----

use craft_cloud_proto::api::{
    AccountUsage, AdminCode, AdminCreatedUser, AdminDevices, AdminServerInfo, AdminStorageOverview, AdminUserInfo, AdminUserList,
};

/// The server with `root`, an administrator, beside `ana`, a member.
async fn admin_server() -> Server {
    let server = server().await;
    server.meta.create_user("root", &hash_password(PASSWORD).unwrap(), Role::Admin, 0).await.unwrap();
    server
}

/// Signs `run` in as `user` through its local API and waits until it is idle.
async fn signed_in_as(server: &Server, user: &str, data: &DataDir, folder: &Path) -> (Running, LocalApiFile) {
    let running = Running::start(data);
    let api = api_file(data.path()).await;
    let body = serde_json::to_string(&LoginRequest {
        server: server.url.clone(),
        user: user.into(),
        password: PASSWORD.into(),
        folder: Some(folder.display().to_string()),
        device_name: Some(format!("{user}-pc")),
        adopt: false,
    })
    .unwrap();
    let (code, answer) = call(&api, "POST", "/v1/login", &body);
    assert_eq!(code, 200, "{answer}");
    eventually("signed in and idle", || status(&api).run_state == RunState::Idle).await;
    (running, api)
}

/// A call with the confirmation `password` and the rest of the body in `fields`.
fn confirmed(api: &LocalApiFile, method: &str, path: &str, password: &str, fields: serde_json::Value) -> (u16, String) {
    let mut body = fields;
    body["password"] = password.into();
    call(api, method, path, &body.to_string())
}

/// Whether an admin disabled the account.
async fn is_disabled(server: &Server, name: &str) -> bool {
    let user = server.meta.user_by_name(name).await.unwrap().unwrap();
    server.meta.admin_user(user.id, 0).await.unwrap().unwrap().disabled_ms.is_some()
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Every route passes the server's answer on with its status and code: reads, the changes of an account, the codes,
/// the devices; the confirmation is the admin's password in the body; a missing or wrong one is the server's `400
/// invalid` and `403 wrong_password` (never 401), and too many wrong ones its `429`. Nothing of the password or of a
/// code is kept by the agent or printed by it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_calls_reach_the_servers_admin_api_and_pass_its_answers_through() {
    let server = admin_server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder = dir.path().join("Craft Cloud");
    let (running, api) = signed_in_as(&server, "root", &data, &folder).await;
    let ana = server.meta.user_by_name("ana").await.unwrap().unwrap();
    let ana_path = format!("/v1/admin/users/{}", ana.id.0);

    // Reads.
    let (code, body) = call(&api, "GET", "/v1/admin/server", "");
    assert_eq!(code, 200, "{body}");
    let info: AdminServerInfo = serde_json::from_str(&body).unwrap();
    assert_eq!((info.users, info.devices_active, info.storage_kind.as_str()), (2, 1, "disk"));
    // The health fields pass through the agent typed.
    assert_eq!((info.database_ok, info.store_ok, info.refusing_uploads), (Some(true), Some(true), Some(false)));
    assert!(info.started_ms.is_some() && info.store_check_ms.is_some(), "{info:?}");
    let (code, body) = call(&api, "GET", "/v1/admin/storage", "");
    assert_eq!(code, 200, "{body}");
    let overview: AdminStorageOverview = serde_json::from_str(&body).unwrap();
    assert_eq!(overview.accounts, 2);
    assert_eq!(overview.stored_bytes, 0);
    // The room the server reports travels through the agent as well.
    assert_eq!(overview.store_kind.as_deref(), Some("disk"));
    let (code, body) = call(&api, "GET", &format!("{ana_path}/usage"), "");
    assert_eq!(code, 200, "{body}");
    let usage: AccountUsage = serde_json::from_str(&body).unwrap();
    assert_eq!((usage.stored_bytes, usage.quota_bytes, usage.trash_bytes, usage.history_bytes), (0, None, Some(0), Some(0)));
    let (code, body) = call(&api, "GET", "/v1/admin/users?limit=1", "");
    assert_eq!(code, 200, "{body}");
    let page: AdminUserList = serde_json::from_str(&body).unwrap();
    assert_eq!(page.users.iter().map(|u| u.username.as_str()).collect::<Vec<_>>(), ["ana"]);
    let next = page.next.expect("a second page");
    let (code, body) = call(&api, "GET", &format!("/v1/admin/users?limit=1&cursor={next}"), "");
    assert_eq!(code, 200, "{body}");
    let second: AdminUserList = serde_json::from_str(&body).unwrap();
    assert_eq!((second.users.first().map(|u| u.username.as_str()), second.next), (Some("root"), None));
    let (code, body) = call(&api, "GET", "/v1/admin/users?query=AN", "");
    assert_eq!(code, 200, "{body}");
    assert_eq!(serde_json::from_str::<AdminUserList>(&body).unwrap().users.len(), 1);
    let (code, body) = call(&api, "GET", "/v1/admin/users?limit=0", "");
    assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "the server's own refusal: {body}");
    let (code, body) = call(&api, "GET", "/v1/admin/users?limit=x", "");
    assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "{body}");
    let (code, body) = call(&api, "GET", &format!("{ana_path}/devices"), "");
    assert_eq!(code, 200, "{body}");
    assert!(serde_json::from_str::<AdminDevices>(&body).unwrap().devices.is_empty());

    // A missing confirmation is the server's `invalid`; a wrong one its `wrong_password`, a 403: never a 401.
    let (code, body) = call(&api, "POST", &format!("{ana_path}/disable"), "{}");
    assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "{body}");
    let (code, body) = confirmed(&api, "POST", &format!("{ana_path}/disable"), "not the password", serde_json::json!({}));
    assert_eq!((code, error_code(&body).as_str()), (403, "wrong_password"), "{body}");
    let (code, body) = call(&api, "POST", &format!("{ana_path}/disable"), "not json");
    assert_eq!(code, 400, "{body}");
    assert!(!is_disabled(&server, "ana").await, "nothing changed without a right confirmation");
    assert!(status(&api).signed_in && !status(&api).removed, "a 403 doesn't sign the agent out");

    // The changes: with the confirmation, each is the server's answer.
    let (code, body) = confirmed(&api, "POST", "/v1/admin/users", PASSWORD, serde_json::json!({ "username": "bo", "role": "member" }));
    assert_eq!(code, 200, "{body}");
    let created: AdminCreatedUser = serde_json::from_str(&body).unwrap();
    assert_eq!((created.user.username.as_str(), created.user.role.as_str(), created.user.invite_pending), ("bo", "member", true));
    let invite_code = created.invite.code.clone();
    assert!(!invite_code.is_empty());
    let bo_path = format!("/v1/admin/users/{}", created.user.id);
    let (code, body) = confirmed(&api, "POST", "/v1/admin/users", PASSWORD, serde_json::json!({ "username": "bo", "role": "member" }));
    assert_eq!((code, error_code(&body).as_str()), (409, "username_taken"), "{body}");
    let (code, body) = confirmed(&api, "PATCH", &ana_path, PASSWORD, serde_json::json!({ "quota_bytes": 50_000_000 }));
    assert_eq!(code, 200, "{body}");
    assert_eq!(serde_json::from_str::<AdminUserInfo>(&body).unwrap().quota_bytes, Some(50_000_000));
    let (code, body) = confirmed(&api, "PATCH", &ana_path, PASSWORD, serde_json::json!({ "quota_bytes": null, "role": "admin" }));
    assert_eq!(code, 200, "{body}");
    let changed: AdminUserInfo = serde_json::from_str(&body).unwrap();
    assert_eq!((changed.quota_bytes, changed.role.as_str()), (None, "admin"));
    let (code, body) = confirmed(&api, "PATCH", &ana_path, PASSWORD, serde_json::json!({}));
    assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "{body}");
    let (code, body) = confirmed(&api, "PATCH", &ana_path, PASSWORD, serde_json::json!({ "role": "member" }));
    assert_eq!(code, 200, "{body}");
    // The last administrator can't be demoted: the server's 409.
    let root = server.meta.user_by_name("root").await.unwrap().unwrap();
    let (code, body) = confirmed(&api, "PATCH", &format!("/v1/admin/users/{}", root.id.0), PASSWORD, serde_json::json!({ "role": "member" }));
    assert_eq!((code, error_code(&body).as_str()), (409, "last_admin"), "{body}");
    let (code, body) = confirmed(&api, "POST", &format!("{ana_path}/disable"), PASSWORD, serde_json::json!({}));
    assert_eq!((code, body.as_str()), (204, ""), "{body}");
    assert!(is_disabled(&server, "ana").await);
    let (code, body) = confirmed(&api, "POST", &format!("{ana_path}/enable"), PASSWORD, serde_json::json!({}));
    assert_eq!((code, body.as_str()), (204, ""), "{body}");
    let (code, body) = confirmed(&api, "POST", &format!("{ana_path}/password-reset"), PASSWORD, serde_json::json!({}));
    assert_eq!(code, 200, "{body}");
    let reset: AdminCode = serde_json::from_str(&body).unwrap();
    assert!(!reset.code.is_empty());
    let (code, body) = confirmed(&api, "POST", &format!("{ana_path}/invite"), PASSWORD, serde_json::json!({}));
    assert_eq!((code, error_code(&body).as_str()), (409, "conflict"), "ana has a password: the server says so: {body}");
    let (code, body) = confirmed(&api, "POST", &format!("{bo_path}/invite"), PASSWORD, serde_json::json!({}));
    assert_eq!(code, 200, "{body}");
    let second_invite: AdminCode = serde_json::from_str(&body).unwrap();
    let (code, body) = confirmed(&api, "DELETE", &format!("{bo_path}/invite"), PASSWORD, serde_json::json!({}));
    assert_eq!((code, body.as_str()), (204, ""), "{body}");
    let (code, body) = confirmed(&api, "DELETE", &format!("{bo_path}/invite"), PASSWORD, serde_json::json!({}));
    assert_eq!((code, error_code(&body).as_str()), (404, "not_found"), "nothing left to revoke: {body}");
    let unknown_device = craft_cloud_proto::DeviceId::generate();
    let (code, body) = confirmed(&api, "DELETE", &format!("{ana_path}/devices/{unknown_device}"), PASSWORD, serde_json::json!({}));
    assert_eq!((code, error_code(&body).as_str()), (404, "not_found"), "{body}");
    let (code, body) = confirmed(&api, "DELETE", &bo_path, PASSWORD, serde_json::json!({}));
    assert_eq!((code, body.as_str()), (202, ""), "the server's own status: {body}");
    let (code, body) = call(&api, "GET", "/v1/admin/users", "");
    assert_eq!(code, 200, "{body}");
    assert!(serde_json::from_str::<AdminUserList>(&body).unwrap().users.iter().all(|u| u.username != "bo"), "removed accounts are out of the list");
    let (code, body) = call(&api, "GET", "/v1/admin/users/not-an-id/devices", "");
    assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "{body}");

    // Wrong passwords are counted by the server: it says when it has had enough, and that stays a 429.
    let mut limited = None;
    for _ in 0..8 {
        let (code, body) = confirmed(&api, "POST", &format!("{ana_path}/disable"), "still wrong", serde_json::json!({}));
        if code == 429 {
            limited = Some(error_code(&body));
            break;
        }
        assert_eq!((code, error_code(&body).as_str()), (403, "wrong_password"), "{body}");
    }
    assert_eq!(limited.as_deref(), Some("rate_limited"));
    assert!(status(&api).signed_in && !status(&api).removed);

    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    // Nothing of the confirmation or the codes was kept or printed (the files are read once the agent has stopped: it
    // holds the data folder's lock while it runs).
    let mut written = Vec::new();
    files_under(data.path(), &mut written);
    for file in written.iter().filter(|f| f.file_name().is_some_and(|n| n != LOCAL_API_FILE)) {
        let text = String::from_utf8_lossy(&std::fs::read(file).unwrap()).into_owned();
        for secret in [PASSWORD, "still wrong", invite_code.as_str(), reset.code.as_str(), second_invite.code.as_str()] {
            assert!(!text.contains(secret), "{} holds a secret", file.display());
        }
    }
    for secret in [PASSWORD, "still wrong", invite_code.as_str(), reset.code.as_str(), second_invite.code.as_str()] {
        assert!(!out.contains(secret), "the agent printed a secret: {out}");
    }
}

/// A member's calls are refused by the server as `403 forbidden`, and that is what the app is told; the agent is
/// not signed out by it (a 401 would mean "this device was revoked").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_is_told_forbidden_and_stays_signed_in() {
    let server = admin_server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let (running, api) = signed_in_as(&server, "ana", &data, &dir.path().join("Craft Cloud")).await;
    let some_id = "0192f0c4-0000-7000-8000-000000000000";
    for (method, path) in
        [("GET", "/v1/admin/server".to_owned()), ("GET", "/v1/admin/users".to_owned()), ("GET", format!("/v1/admin/users/{some_id}/devices"))]
    {
        let (code, body) = call(&api, method, &path, "");
        assert_eq!((code, error_code(&body).as_str()), (403, "forbidden"), "{method} {path}: {body}");
    }
    for (method, path) in [
        ("POST", "/v1/admin/users".to_owned()),
        ("PATCH", format!("/v1/admin/users/{some_id}")),
        ("POST", format!("/v1/admin/users/{some_id}/disable")),
        ("DELETE", format!("/v1/admin/users/{some_id}")),
    ] {
        let fields = serde_json::json!({ "username": "x", "role": "admin", "quota_bytes": 5_000_000 });
        let (code, body) = confirmed(&api, method, &path, PASSWORD, fields);
        assert_eq!((code, error_code(&body).as_str()), (403, "forbidden"), "{method} {path}: {body}");
    }
    let now = status(&api);
    assert!(now.signed_in && !now.removed, "{now:?}");
    let ana = server.meta.user_by_name("ana").await.unwrap().unwrap();
    assert_eq!(ana.role, Role::Member);
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
}

/// The server's `401` for a revoked admin device is not a local `401` (which says the token is wrong): it is told as
/// every signed-in call tells it, as something that needs the person.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_admin_device_is_told_as_needs_you() {
    let server = admin_server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let (running, api) = signed_in_as(&server, "root", &data, &dir.path().join("Craft Cloud")).await;
    let root = server.meta.user_by_name("root").await.unwrap().unwrap();
    for device in server.meta.list_devices(root.id).await.unwrap() {
        server.meta.revoke_device(root.id, device.id).await.unwrap();
    }
    let (code, body) = call(&api, "GET", "/v1/admin/server", "");
    assert_eq!((code, error_code(&body).as_str()), (409, codes::NEEDS_YOU), "{body}");
    let (code, body) = confirmed(&api, "POST", "/v1/admin/users", PASSWORD, serde_json::json!({ "username": "x", "role": "member" }));
    assert_eq!((code, error_code(&body).as_str()), (409, codes::NEEDS_YOU), "{body}");
    running.stop().await.0.unwrap();
}

/// Signed out there is no server to ask; the bodies are small; the checks on every call are the other routes'.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_calls_need_a_sign_in_and_small_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let running = Running::start(&data);
    let api = api_file(data.path()).await;
    let id = "0192f0c4-0000-7000-8000-000000000000";
    let (code, body) = call(&api, "GET", "/v1/admin/server", "");
    assert_eq!((code, error_code(&body).as_str()), (409, codes::NOT_SIGNED_IN), "{body}");
    let (code, body) = confirmed(&api, "POST", &format!("/v1/admin/users/{id}/disable"), PASSWORD, serde_json::json!({}));
    assert_eq!((code, error_code(&body).as_str()), (409, codes::NOT_SIGNED_IN), "{body}");
    let (code, body) = call(&api, "GET", "/v1/admin/users?limit=9999999999", "");
    assert_eq!(code, 400, "a limit that is no number: {body}");
    let long = serde_json::json!({ "username": "a".repeat(9 * 1024), "role": "member", "password": PASSWORD }).to_string();
    let (code, body) = call(&api, "POST", "/v1/admin/users", &long);
    assert_eq!((code, error_code(&body).as_str()), (413, "too_large"), "{body}");
    let (code, _) = call(&api, "POST", "/v1/admin/users", "{\"username\": 5}");
    assert_eq!(code, 422, "the shape of the body is checked here too");
    running.stop().await.0.unwrap();
}

// ---- the account, its activity and the audit log ----

use craft_cloud_proto::api::{AccountInfo, AuditEvent, AuditPage};
use craft_cloud_server::audit::AuditExportLimits;

/// [`admin_server`] with the export cut as `limits` say.
async fn admin_server_limited(limits: AuditExportLimits) -> Server {
    let (db, blobs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
    meta.create_user("root", &hash_password(PASSWORD).unwrap(), Role::Admin, 0).await.unwrap();
    let state = AppState::new(meta.clone(), Store::new(DiskBackend::new(blobs.path()).unwrap()), vec![]).unwrap().with_audit_export(limits);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending()));
    Server { url, meta, task, _dirs: (db, blobs) }
}

fn page_of(body: &str) -> AuditPage {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not an audit page ({e}): {body}"))
}

/// The account, its activity, the audit list and its export pass through the agent with the server's statuses and codes: a
/// member's `403 forbidden` is never a signed-out agent; filters are checked before they are sent; the export is the server's
/// stream, whole, and `truncated` when the server cuts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_account_its_activity_and_the_audit_log_pass_through() {
    let server = admin_server_limited(AuditExportLimits::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let root_data = DataDir::new(dir.path().join("root"));
    let ana_data = DataDir::new(dir.path().join("ana"));
    let (root_run, root) = signed_in_as(&server, "root", &root_data, &dir.path().join("Root Cloud")).await;
    let (ana_run, ana) = signed_in_as(&server, "ana", &ana_data, &dir.path().join("Ana Cloud")).await;
    let ana_user = server.meta.user_by_name("ana").await.unwrap().unwrap();

    // Some more events: the admin creates an account, disables it and enables it again (not ana: that would sign her out).
    let (code, body) = confirmed(&root, "POST", "/v1/admin/users", PASSWORD, serde_json::json!({ "username": "bo", "role": "member" }));
    assert_eq!(code, 200, "{body}");
    let bo = serde_json::from_str::<craft_cloud_proto::api::AdminCreatedUser>(&body).unwrap().user.id;
    for what in ["disable", "enable"] {
        let (code, body) = confirmed(&root, "POST", &format!("/v1/admin/users/{bo}/{what}"), PASSWORD, serde_json::json!({}));
        assert_eq!(code, 204, "{body}");
    }

    // The signed-in account, as the server says it.
    let (code, body) = call(&root, "GET", "/v1/account", "");
    assert_eq!(code, 200, "{body}");
    assert_eq!(serde_json::from_str::<AccountInfo>(&body).unwrap().role, "admin");
    let (code, body) = call(&ana, "GET", "/v1/account", "");
    assert_eq!((code, serde_json::from_str::<AccountInfo>(&body).unwrap().role.as_str()), (200, "member"));

    // The admin's list: pages with a cursor given back as it came, filters, the server's own refusals.
    let (code, body) = call(&root, "GET", "/v1/admin/audit?limit=2", "");
    assert_eq!(code, 200, "{body}");
    let first = page_of(&body);
    assert_eq!(first.events.len(), 2);
    assert!(first.events[0].at_ms >= first.events[1].at_ms, "newest first");
    let next = first.next.clone().expect("more events than a page");
    let (code, body) = call(&root, "GET", &format!("/v1/admin/audit?limit=2&cursor={next}"), "");
    assert_eq!(code, 200, "{body}");
    let second = page_of(&body);
    assert!(second.events.iter().all(|e| first.events.iter().all(|f| f.id != e.id)), "no event twice");
    let (code, body) = call(&root, "GET", "/v1/admin/audit?action=admin.", "");
    assert_eq!(code, 200, "{body}");
    let admin_events = page_of(&body).events;
    assert!(admin_events.len() >= 3 && admin_events.iter().all(|e| e.action.starts_with("admin.")), "{admin_events:?}");
    let (code, body) = call(&root, "GET", "/v1/admin/audit?action=auth.login", "");
    let logins = page_of(&body).events;
    assert!(code == 200 && logins.len() == 2 && logins.iter().all(|e| e.action == "auth.login"), "{body}");
    let (code, body) = call(&root, "GET", &format!("/v1/admin/audit?user={}", ana_user.id.0), "");
    assert_eq!(code, 200, "{body}");
    let about_ana = page_of(&body).events;
    assert!(!about_ana.is_empty() && about_ana.iter().all(|e| e.actor_user == Some(ana_user.id.0) || e.target_id == Some(ana_user.id.0)));
    let (code, body) = call(&root, "GET", "/v1/admin/audit?before_ms=1", "");
    assert!(code == 200 && page_of(&body).events.is_empty(), "{body}");
    // What is wrong is said before anything is sent, or by the server (a cursor it didn't make).
    for query in [
        "action=nope.",
        "action=auth.log",
        "action=x%27%20OR%201=1",
        "user=not-an-id",
        "limit=0",
        "limit=201",
        "limit=x",
        "before_ms=-1",
        "cursor=a%20b",
        "cursor=AAAA",
    ] {
        let (code, body) = call(&root, "GET", &format!("/v1/admin/audit?{query}"), "");
        assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "{query}: {body}");
    }

    // A member's own activity: the events about her, whatever the admin's filters say; a member can't read the log.
    let (code, body) = call(&ana, "GET", "/v1/account/activity", "");
    assert_eq!(code, 200, "{body}");
    let own = page_of(&body).events;
    assert!(!own.is_empty(), "{own:?}");
    assert!(own.iter().all(|e| e.actor_user == Some(ana_user.id.0) || e.target_id == Some(ana_user.id.0)), "{own:?}");
    let (code, body) =
        call(&ana, "GET", &format!("/v1/account/activity?user={}&action=admin.", server.meta.user_by_name("root").await.unwrap().unwrap().id.0), "");
    assert_eq!(code, 200, "{body}");
    assert_eq!(page_of(&body).events.len(), own.len(), "an account's activity has no admin filters");
    let (code, body) = call(&ana, "GET", "/v1/account/activity?limit=101", "");
    assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "{body}");
    for path in ["/v1/admin/audit", "/v1/admin/audit/export"] {
        let (code, body) = call(&ana, "GET", path, "");
        assert_eq!((code, error_code(&body).as_str()), (403, "forbidden"), "{path}: {body}");
    }
    let now = status(&ana);
    assert!(now.signed_in && !now.removed, "a 403 doesn't sign the agent out: {now:?}");

    // The export: the server's lines, then its last line; whole through the typed client, which writes as it reads.
    let client = craft_cloud_app::Client::connect_in(root_data.path()).unwrap();
    let all = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let outcome = client.admin_audit_export(&craft_cloud_app::AuditQuery::default(), &mut out);
        (outcome, out)
    })
    .await
    .unwrap();
    let (outcome, out) = all;
    let outcome = outcome.unwrap();
    assert!(!outcome.truncated);
    let lines: Vec<AuditEvent> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len() as u64, outcome.events);
    let (code, body) = call(&root, "GET", "/v1/admin/audit?limit=200", "");
    assert_eq!(code, 200, "{body}");
    assert!(page_of(&body).events.iter().all(|e| lines.iter().any(|l| l.id == e.id)), "the export holds every event of the list");
    // A filtered export; a refusal before the stream; a filter that is wrong.
    let client = craft_cloud_app::Client::connect_in(root_data.path()).unwrap();
    let filtered = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let query = craft_cloud_app::AuditQuery { action: Some("auth.".into()), ..Default::default() };
        let outcome = client.admin_audit_export(&query, &mut out).unwrap();
        (outcome, out)
    })
    .await
    .unwrap();
    assert_eq!(filtered.0.events, 2, "the two sign-ins");
    let ana_client = craft_cloud_app::Client::connect_in(ana_data.path()).unwrap();
    let refused = tokio::task::spawn_blocking(move || ana_client.admin_audit_export(&Default::default(), &mut Vec::new())).await.unwrap();
    assert_eq!(refused.unwrap_err().code(), Some("forbidden"));
    let (code, body) = call(&root, "GET", "/v1/admin/audit/export?action=nope.", "");
    assert_eq!((code, error_code(&body).as_str()), (400, "invalid"), "{body}");

    // Nothing of any of it was printed.
    let (exit, out) = root_run.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(!out.contains(PASSWORD), "{out}");
    ana_run.stop().await.0.unwrap();
}

/// A server that holds more events than an export may: the last line is `truncated`, and the client says so (not whole).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_truncated_export_is_said_to_be() {
    let server = admin_server_limited(AuditExportLimits { page: 1, channel: 1, max_rows: 2 }).await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("root"));
    let (running, api) = signed_in_as(&server, "root", &data, &dir.path().join("Root Cloud")).await;
    for name in ["bo", "cy", "di"] {
        let (code, body) = confirmed(&api, "POST", "/v1/admin/users", PASSWORD, serde_json::json!({ "username": name, "role": "member" }));
        assert_eq!(code, 200, "{body}");
    }
    let client = craft_cloud_app::Client::connect_in(data.path()).unwrap();
    let (outcome, out) = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        (client.admin_audit_export(&Default::default(), &mut out), out)
    })
    .await
    .unwrap();
    let outcome = outcome.unwrap();
    assert_eq!((outcome.events, outcome.truncated), (2, true));
    assert_eq!(String::from_utf8(out).unwrap().lines().count(), 2, "the newest two, and no last line in what is written");
    running.stop().await.0.unwrap();
}

/// The copies the agent lists, through the local API.
fn conflict_list(api: &LocalApiFile) -> craft_cloud_proto::local::ConflictsResponse {
    let (code, body) = call(api, "GET", "/v1/conflicts", "");
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn folder_sizes(api: &LocalApiFile) -> craft_cloud_proto::local::FolderUsageResponse {
    let (code, body) = call(api, "GET", "/v1/usage/folders", "");
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// Waits until the agent has the file in its saved state (history is answered only for such a file).
async fn in_saved_state(api: &LocalApiFile, path: &str) {
    let query = format!("/v1/files/history?path={}", path.replace('/', "%2F").replace(' ', "%20"));
    eventually(&format!("{path} is in the saved state"), || call(api, "GET", &query, "").0 == 200).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflict_routes_refuse_signed_out() {
    let data = DataDir::new(tempfile::tempdir().unwrap().keep());
    let running = Running::start(&data);
    let api = api_file(data.path()).await;
    for path in ["/v1/conflicts", "/v1/usage/folders"] {
        let (code, body) = call(&api, "GET", path, "");
        assert_eq!((code, error_code(&body).as_str()), (409, codes::NOT_SIGNED_IN), "{path}: {body}");
    }
    let _ = running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflict_made_by_two_computers_is_listed() {
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder = dir.path().join("Craft Cloud");
    let (running, api) = signed_in(&server, &data, &folder).await;
    assert_eq!(conflict_list(&api), Default::default());

    std::fs::create_dir(folder.join("Notes")).unwrap();
    std::fs::write(folder.join("Notes/doc.txt"), "base").unwrap();
    reaches_the_server(&server, "doc.txt").await;

    // A second computer joins, edits the file, and loses the race against this one.
    let other = DataDir::new(dir.path().join("other-data"));
    let other_folder = dir.path().join("Other Cloud");
    let args = cli::LoginArgs {
        url: server.url.clone(),
        user: "ana".into(),
        folder: Some(other_folder.clone()),
        device_name: Some("desktop".into()),
        adopt: false,
    };
    cli::login(&other, &args, PASSWORD, &mut Vec::new()).await.unwrap();
    cli::sync(&other, false, &mut Vec::new()).await.unwrap();
    assert_eq!(std::fs::read_to_string(other_folder.join("Notes/doc.txt")).unwrap(), "base");
    std::fs::write(other_folder.join("Notes/doc.txt"), "edited on the other computer!").unwrap();
    std::fs::write(folder.join("Notes/doc.txt"), "edited here").unwrap();
    eventually("the edit made here is on the server", || {
        let (code, body) = call(&api, "GET", "/v1/files/history?path=Notes%2Fdoc.txt&limit=5", "");
        code == 200 && serde_json::from_str::<HistoryResponse>(&body).is_ok_and(|page| page.versions.len() >= 2)
    })
    .await;
    let (_, summary) = cli::sync(&other, false, &mut Vec::new()).await.unwrap();
    assert_eq!(summary.conflict_copies.len(), 1, "{summary:?}");
    let copy = summary.conflict_copies[0].clone();
    assert!(copy.starts_with("Notes/doc (conflict, desktop, ") && copy.ends_with(").txt"), "{copy}");

    // It reaches this computer through the change feed and shows up in the list.
    eventually("the copy is listed", || !conflict_list(&api).copies.is_empty()).await;
    let list = conflict_list(&api);
    assert!(!list.truncated);
    assert_eq!(list.copies.len(), 1, "{list:?}");
    let found = &list.copies[0];
    assert_eq!(found.path, copy);
    assert_eq!(found.original_path.as_deref(), Some("Notes/doc.txt"));
    assert_eq!(found.device.as_deref(), Some("desktop"));
    assert!(found.at.is_some());
    assert_eq!(found.size, "edited on the other computer!".len() as u64);

    // The same through the app's client; the folder sizes count both files under Notes.
    let client = craft_cloud_app::Client::connect_in(data.path()).unwrap();
    let (listed, sizes) = tokio::task::spawn_blocking(move || (client.conflicts().unwrap(), client.folder_usage().unwrap())).await.unwrap();
    assert_eq!(listed, list);
    let notes = sizes.folders.iter().find(|f| f.name == "Notes").expect("Notes is listed");
    assert_eq!(notes.files, 2);
    assert_eq!(notes.bytes, "edited on the other computer!".len() as u64 + "edited here".len() as u64);
    running.stop().await.0.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookalike_without_an_original_is_not_a_conflict_copy() {
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder = dir.path().join("Craft Cloud");
    let (running, api) = signed_in(&server, &data, &folder).await;
    let ghost = "ghost (conflict, pc, 2026-10-11 14-03).txt";
    let copy = "kept (conflict, pc, 2026-10-11 14-03).txt";
    for (name, content) in [(ghost, "no original"), (copy, "has one"), ("kept.txt", "the original")] {
        std::fs::write(folder.join(name), content).unwrap();
    }
    for name in [ghost, copy, "kept.txt"] {
        in_saved_state(&api, name).await;
    }
    let list = conflict_list(&api);
    let paths: Vec<&str> = list.copies.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, [copy], "{list:?}");
    assert_eq!(list.copies[0].original_path.as_deref(), Some("kept.txt"));
    running.stop().await.0.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_conflict_list_is_capped_at_1000() {
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder = dir.path().join("Craft Cloud");
    let (running, api) = signed_in(&server, &data, &folder).await;
    std::fs::write(folder.join("a.txt"), "x").unwrap();
    for n in 2..=1002 {
        std::fs::write(folder.join(format!("a (conflict, pc, 2026-10-11 14-03, {n}).txt")), "y").unwrap();
    }
    eventually("the list is full", || conflict_list(&api).truncated).await;
    let list = conflict_list(&api);
    assert_eq!(list.copies.len(), 1000);
    assert_eq!(list.copies.first().map(|c| c.path.as_str()), Some("a (conflict, pc, 2026-10-11 14-03, 1002).txt"));
    running.stop().await.0.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folder_usage_sums_local_logical_bytes_by_folder_and_kind() {
    use craft_cloud_proto::local::FileKind;
    let server = server().await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder = dir.path().join("Craft Cloud");
    let (running, api) = signed_in(&server, &data, &folder).await;
    assert_eq!(folder_sizes(&api).total_files, 0);
    for (path, size) in [("root.txt", 5usize), ("Art/a.png", 100), ("Art/deep/b.PNG", 50), ("Art/c.mp4", 1000), ("Docs/d.pdf", 20), ("Docs/noext", 3)]
    {
        let file = folder.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, vec![b'x'; size]).unwrap();
    }
    eventually("all six files are in the saved state", || folder_sizes(&api).total_files == 6).await;
    let usage = folder_sizes(&api);
    assert_eq!(usage.total_bytes, 1178);
    let folders: Vec<(&str, u64, u64)> = usage.folders.iter().map(|f| (f.name.as_str(), f.bytes, f.files)).collect();
    assert_eq!(folders, [("Art", 1150, 3), ("Docs", 23, 2), ("", 5, 1)]);
    assert_eq!(usage.other, None);
    let kind = |k| usage.kinds.iter().find(|u| u.kind == k).map(|u| (u.bytes, u.files)).unwrap();
    assert_eq!(kind(FileKind::Image), (150, 2));
    assert_eq!(kind(FileKind::Video), (1000, 1));
    assert_eq!(kind(FileKind::Document), (20, 1));
    assert_eq!(kind(FileKind::Code), (5, 1));
    assert_eq!(kind(FileKind::Other), (3, 1));
    assert_eq!(kind(FileKind::Audio), (0, 0));
    running.stop().await.0.unwrap();
}
