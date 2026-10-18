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
use craft_cloud_proto::api::ErrorBody;
use craft_cloud_proto::local::{LOCAL_API_FILE, LocalApiFile, LoginRequest, RunState, Status, codes};
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
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn server() -> Server {
    let (db, blobs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
    let state = AppState::new(meta.clone(), Store::new(DiskBackend::new(blobs.path()).unwrap()), vec![]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending()));
    Server { url, meta, _dirs: (db, blobs) }
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
    }
}

/// `run` on a thread and runtime of its own, as the binary runs it.
struct Running {
    stop: mpsc::UnboundedSender<()>,
    thread: Option<std::thread::JoinHandle<(Result<Exit, AgentError>, String)>>,
}

impl Running {
    fn start(data: &DataDir) -> Self {
        let (stop, stops) = mpsc::unbounded_channel();
        let data = data.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut out = Vec::new();
            let exit = runtime.block_on(run_loop(&data, quick(), stops, &mut out));
            (exit, String::from_utf8_lossy(&out).into_owned())
        });
        Self { stop, thread: Some(thread) }
    }

    async fn stop(mut self) -> (Result<Exit, AgentError>, String) {
        let _ = self.stop.send(());
        let thread = self.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap()).await.unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        let _ = self.stop.send(());
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
