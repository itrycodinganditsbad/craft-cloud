//! Two-factor sign-in through the agent: the second step of a sign-in and the code of a join, the account's two-factor calls
//! passed on to the server, and the command line's prompts, against a real server (with a key for two-factor secrets) on a
//! loopback port and the server's clock held still, so that codes don't change under a test.
//!
//! What they pin down: a challenge, its expiry and a lock's `Retry-After` reach the app typed; a wrong code, a wrong
//! password or a lock never sign the agent out (a 403 or 429 on the account's calls, a 401 at the second step), while a real
//! 401 still does; nothing secret is kept on disk or printed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;
use craft_cloud_agent::account::DataDir;
use craft_cloud_agent::cli::{self, AgentError, Exit};
use craft_cloud_agent::daemon::{RunOptions, run_loop};
use craft_cloud_agent::schedule::Timing;
use craft_cloud_app::{Client, Error, JoinRequest, LoginRequest, SecondFactorLoginRequest, TwoFactorProofRequest};
use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::join::{Code, INVITE_CHARS, INVITE_TTL_MS};
use craft_cloud_proto::local::{LOCAL_API_FILE, LocalApiFile};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_server::secretbox::{SecretBox, TwoFactorKey};
use craft_cloud_server::totp;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use tokio::sync::mpsc;

const PASSWORD: &str = "correct horse battery";
const PATIENCE: Duration = Duration::from_secs(30);

struct Server {
    url: String,
    meta: Meta,
    clock: Arc<AtomicI64>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Server {
    async fn start() -> Self {
        let (db, blobs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
        meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
        // The clock stands still at the start of a TOTP step, until a test moves it.
        let clock = Arc::new(AtomicI64::new(totp::step_at(craft_cloud_server::state::now_ms()) * 30_000 + 1_000));
        let ticking = clock.clone();
        let state = AppState::new(meta.clone(), Store::new(DiskBackend::new(blobs.path()).unwrap()), vec![])
            .unwrap()
            .with_two_factor(Some(SecretBox::new(&TwoFactorKey::from_bytes([9; 32]), None).unwrap()))
            .with_clock(Arc::new(move || ticking.load(Ordering::SeqCst)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending()));
        Server { url, meta, clock, _dirs: (db, blobs) }
    }

    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
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
        autostart: craft_cloud_agent::autostart::Target::Off,
    }
}

/// A running agent (`run` on a thread of its own), signed out, and a client of its local API.
struct Machine {
    data: DataDir,
    folder: PathBuf,
    stop: mpsc::UnboundedSender<()>,
    thread: Option<std::thread::JoinHandle<(Result<Exit, AgentError>, String)>>,
    _dir: tempfile::TempDir,
}

impl Machine {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path().join("data"));
        let (stop, stops) = mpsc::unbounded_channel();
        let loop_data = data.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut out = Vec::new();
            let exit = runtime.block_on(run_loop(&loop_data, quick(), stops, &mut out));
            (exit, String::from_utf8_lossy(&out).into_owned())
        });
        let file = data.path().join(LOCAL_API_FILE);
        let deadline = Instant::now() + PATIENCE;
        while !file.exists() {
            assert!(Instant::now() < deadline, "the local API never came up");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self { folder: dir.path().join("Craft Cloud"), data, stop, thread: Some(thread), _dir: dir }
    }

    fn client(&self) -> Client {
        Client::connect_in(self.data.path()).unwrap()
    }

    fn api(&self) -> LocalApiFile {
        serde_json::from_slice(&std::fs::read(self.data.path().join(LOCAL_API_FILE)).unwrap()).unwrap()
    }

    fn login_request(&self, server: &Server) -> LoginRequest {
        LoginRequest {
            server: server.url.clone(),
            user: "ana".into(),
            password: PASSWORD.into(),
            folder: Some(self.folder.display().to_string()),
            device_name: Some("desktop".into()),
            adopt: true,
        }
    }

    fn second_step(&self, server: &Server, challenge: &str, totp_code: Option<&str>, recovery_code: Option<&str>) -> SecondFactorLoginRequest {
        SecondFactorLoginRequest {
            server: server.url.clone(),
            user: "ana".into(),
            folder: Some(self.folder.display().to_string()),
            device_name: Some("desktop".into()),
            adopt: true,
            challenge: challenge.into(),
            totp_code: totp_code.map(str::to_owned),
            recovery_code: recovery_code.map(str::to_owned),
        }
    }

    async fn stop(mut self) -> (Result<Exit, AgentError>, String) {
        let _ = self.stop.send(());
        let thread = self.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap()).await.unwrap()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.stop.send(());
            let _ = self.stop.send(());
        }
    }
}

/// A blocking call of the app client, off the runtime.
async fn off<T: Send + 'static>(f: impl FnOnce() -> Result<T, Error> + Send + 'static) -> Result<T, Error> {
    tokio::task::spawn_blocking(f).await.unwrap()
}

fn code_for(secret: &[u8], at_ms: i64) -> String {
    totp::code_for(secret, totp::step_at(at_ms)).unwrap()
}

fn wrong_code(code: &str) -> String {
    format!("{:06}", (code.parse::<u32>().unwrap() + 1) % 1_000_000)
}

/// Signs the machine in and turns two-factor sign-in on through the agent. The secret and the recovery codes come back.
async fn protect(server: &Server, machine: &Machine) -> (Vec<u8>, Vec<String>) {
    let client = machine.client();
    let request = machine.login_request(server);
    let c = client.clone();
    assert!(off(move || c.login(&request)).await.unwrap().signed_in);
    let c = client.clone();
    let setup = off(move || c.two_factor_setup(PASSWORD)).await.unwrap();
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let code = code_for(&secret, server.now());
    let c = client.clone();
    let codes = off(move || c.two_factor_confirm(&code)).await.unwrap();
    assert_eq!(codes.recovery_codes.len(), 10);
    (secret, codes.recovery_codes)
}

async fn sign_out(machine: &Machine) {
    let c = machine.client();
    assert!(!off(move || c.logout(false)).await.unwrap().signed_in);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_second_step_of_a_sign_in_goes_through_the_agent_typed() {
    let server = Server::start().await;
    let machine = Machine::start().await;
    let (secret, recovery) = protect(&server, &machine).await;
    sign_out(&machine).await;
    let client = machine.client();

    // The first step: a challenge, typed, with its expiry; the agent is still signed out and keeps nothing.
    let request = machine.login_request(&server);
    let c = client.clone();
    let refused = off(move || c.login(&request)).await.unwrap_err();
    assert_eq!(refused.code(), Some("two_factor_required"));
    let challenge = refused.challenge().cloned().expect("a challenge");
    assert_eq!(challenge.challenge.len(), 43);
    assert_eq!(challenge.expires_ms, server.now() + 5 * 60_000);
    assert!(!client.status().unwrap().signed_in);
    // A wrong password is still a wrong password, with nothing about two-factor sign-in.
    let wrong = LoginRequest { password: "wrong password".into(), ..machine.login_request(&server) };
    let c = client.clone();
    let refused = off(move || c.login(&wrong)).await.unwrap_err();
    assert!(matches!(&refused, Error::Refused { code, .. } if code == "wrong_password"), "{refused:?}");

    // A wrong code is `two_factor_invalid` and signs nothing in or out; a challenge takes five.
    let right = code_for(&secret, server.now() + 30_000);
    server.advance(30_000);
    let step = machine.second_step(&server, &challenge.challenge, Some(&wrong_code(&right)), None);
    let c = client.clone();
    let invalid = off(move || c.login_second_factor(&step)).await.unwrap_err();
    assert!(matches!(&invalid, Error::Refused { code, .. } if code == "two_factor_invalid"), "{invalid:?}");
    assert!(!client.status().unwrap().signed_in);
    // The right code finishes it, and the agent syncs as after a password sign-in.
    let step = machine.second_step(&server, &challenge.challenge, Some(&right), None);
    let c = client.clone();
    let status = off(move || c.login_second_factor(&step)).await.unwrap();
    assert!(status.signed_in && status.account.as_deref() == Some("ana"), "{status:?}");
    // The challenge is used up: the same step again is `two_factor_expired`, never a signed-out agent.
    let step = machine.second_step(&server, &challenge.challenge, Some(&right), None);
    let c = client.clone();
    let again = off(move || c.login_second_factor(&step)).await.unwrap_err();
    assert!(client.status().unwrap().signed_in);
    assert!(matches!(&again, Error::Refused { .. } | Error::RefusedWith { .. }), "{again:?}");

    // A recovery code, once.
    sign_out(&machine).await;
    let request = machine.login_request(&server);
    let c = client.clone();
    let challenge = off(move || c.login(&request)).await.unwrap_err().challenge().cloned().unwrap();
    let step = machine.second_step(&server, &challenge.challenge, None, Some(&recovery[0].to_lowercase().replace('-', " ")));
    let c = client.clone();
    assert!(off(move || c.login_second_factor(&step)).await.unwrap().signed_in);
    sign_out(&machine).await;
    let request = machine.login_request(&server);
    let c = client.clone();
    let challenge = off(move || c.login(&request)).await.unwrap_err().challenge().cloned().unwrap();
    let step = machine.second_step(&server, &challenge.challenge, None, Some(&recovery[0]));
    let c = client.clone();
    let used = off(move || c.login_second_factor(&step)).await.unwrap_err();
    assert!(matches!(&used, Error::Refused { code, .. } if code == "two_factor_invalid"), "{used:?}");

    // A challenge that ran out is `two_factor_expired` (a 401 here would read as a revoked device).
    server.advance(5 * 60_000 + 1);
    let step = machine.second_step(&server, &challenge.challenge, Some(&code_for(&secret, server.now())), None);
    let c = client.clone();
    let expired = off(move || c.login_second_factor(&step)).await.unwrap_err();
    assert!(matches!(&expired, Error::Refused { code, .. } if code == "two_factor_expired"), "{expired:?}");
    let (exit, out) = machine.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_accounts_calls_are_passed_on_and_a_refusal_never_signs_the_agent_out() {
    let server = Server::start().await;
    let machine = Machine::start().await;
    let client = machine.client();
    let request = machine.login_request(&server);
    let c = client.clone();
    assert!(off(move || c.login(&request)).await.unwrap().signed_in);

    let c = client.clone();
    let off_state = off(move || c.two_factor_status()).await.unwrap();
    assert_eq!((off_state.enabled, off_state.recovery_codes_left, off_state.pending), (false, 0, false));
    // A wrong password is 403 `wrong_password`, and the agent stays signed in, not even "removed".
    let c = client.clone();
    let wrong = off(move || c.two_factor_setup("wrong password!")).await.unwrap_err();
    assert!(matches!(&wrong, Error::Refused { code, .. } if code == "wrong_password"), "{wrong:?}");
    let status = client.status().unwrap();
    assert!(status.signed_in && !status.removed, "{status:?}");
    let c = client.clone();
    let setup = off(move || c.two_factor_setup(PASSWORD)).await.unwrap();
    assert_eq!(setup.secret.len(), 32);
    assert!(setup.otpauth_uri.starts_with("otpauth://totp/Craft%20Cloud:ana?secret="), "{}", setup.otpauth_uri);
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let c = client.clone();
    assert!(off(move || c.two_factor_status()).await.unwrap().pending);
    // A wrong first code: 403 `two_factor_invalid`; the agent is not signed out.
    let right = code_for(&secret, server.now());
    let bad = wrong_code(&right);
    let c = client.clone();
    let invalid = off(move || c.two_factor_confirm(&bad)).await.unwrap_err();
    assert!(matches!(&invalid, Error::Refused { code, .. } if code == "two_factor_invalid"), "{invalid:?}");
    assert!(client.status().unwrap().signed_in);
    let c = client.clone();
    let codes = off(move || c.two_factor_confirm(&right)).await.unwrap();
    assert_eq!(codes.recovery_codes.len(), 10);
    let c = client.clone();
    let on = off(move || c.two_factor_status()).await.unwrap();
    assert_eq!((on.enabled, on.recovery_codes_left), (true, 10));
    // Already on: a conflict, said as one.
    let c = client.clone();
    let conflict = off(move || c.two_factor_setup(PASSWORD)).await.unwrap_err();
    assert!(matches!(&conflict, Error::Refused { code, .. } if code == "conflict"), "{conflict:?}");

    // New recovery codes with a fresh step; the old ones die.
    server.advance(30_000);
    let fresh = code_for(&secret, server.now());
    let c = client.clone();
    let renewed = off(move || c.two_factor_recovery_codes(PASSWORD, &fresh)).await.unwrap();
    assert!(renewed.recovery_codes.iter().all(|code| !codes.recovery_codes.contains(code)));

    // Password change and the other computers: passed on too.
    let c = client.clone();
    let weak = off(move || c.change_password(PASSWORD, "short")).await.unwrap_err();
    assert!(matches!(&weak, Error::Refused { code, .. } if code == "weak_password"), "{weak:?}");
    let c = client.clone();
    assert_eq!(off(move || c.revoke_other_devices()).await.unwrap().revoked, 0);
    let c = client.clone();
    let devices = off(move || c.devices()).await.unwrap();
    let this = devices.iter().find(|d| d.current).unwrap().id;
    let c = client.clone();
    let renamed = off(move || c.rename_device(this, "my desktop")).await.unwrap();
    assert_eq!(renamed.name, "my desktop");
    let c = client.clone();
    let missing = off(move || c.rename_device(craft_cloud_app::DeviceId::generate(), "x")).await.unwrap_err();
    assert!(matches!(&missing, Error::Refused { code, .. } if code == "not_found"), "{missing:?}");

    // Ten wrong codes lock it: 429 `two_factor_locked`, typed with its wait, and still no sign-out.
    for _ in 0..10 {
        let proof = TwoFactorProofRequest { password: PASSWORD.into(), totp_code: Some(bad_code(&secret, &server)), recovery_code: None };
        let c = client.clone();
        let invalid = off(move || c.two_factor_disable(&proof)).await.unwrap_err();
        assert!(matches!(&invalid, Error::Refused { code, .. } if code == "two_factor_invalid"), "{invalid:?}");
    }
    let proof = TwoFactorProofRequest { password: PASSWORD.into(), totp_code: Some(code_for(&secret, server.now())), recovery_code: None };
    let c = client.clone();
    let locked = off(move || c.two_factor_disable(&proof)).await.unwrap_err();
    assert_eq!((locked.code(), locked.retry_after_secs()), (Some("two_factor_locked"), Some(900)), "{locked:?}");
    let status = client.status().unwrap();
    assert!(status.signed_in && !status.removed, "{status:?}");

    // Turning it off with a recovery code works once the lock is over.
    server.advance(900_001);
    let proof = TwoFactorProofRequest { password: PASSWORD.into(), totp_code: None, recovery_code: Some(renewed.recovery_codes[0].clone()) };
    let c = client.clone();
    off(move || c.two_factor_disable(&proof)).await.unwrap();
    let c = client.clone();
    assert!(!off(move || c.two_factor_status()).await.unwrap().enabled);

    // A real 401 (the device was signed out on the server) is still what it was: the agent can't go on.
    let ana = server.meta.user_by_name("ana").await.unwrap().unwrap();
    server.meta.revoke_all_devices(ana.id, craft_cloud_meta::RevokeReason::Admin, server.now()).await.unwrap();
    let c = client.clone();
    let revoked = off(move || c.two_factor_status()).await.unwrap_err();
    assert!(matches!(&revoked, Error::Refused { code, .. } if code == "needs_you"), "{revoked:?}");
    let _ = machine.stop().await;
}

fn bad_code(secret: &[u8], server: &Server) -> String {
    wrong_code(&code_for(secret, server.now()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joining_an_account_with_two_factor_takes_the_code_in_a_second_call() {
    let server = Server::start().await;
    let first = Machine::start().await;
    let (secret, recovery) = protect(&server, &first).await;
    let invite = Code::generate(INVITE_CHARS).unwrap();
    server.meta.create_invite("ana", &invite.hash(), server.now(), INVITE_TTL_MS).await.unwrap();

    let second = Machine::start().await;
    let client = second.client();
    let join = JoinRequest {
        join: format!("{}/{}", server.url, invite.display()),
        folder: Some(second.folder.display().to_string()),
        device_name: Some("laptop".into()),
        adopt: true,
        password: None,
        totp_code: None,
        recovery_code: None,
    };
    // No second factor: `two_factor_required` (no challenge to give: the same join goes again), and the invite is not used up.
    let c = client.clone();
    let j = join.clone();
    let refused = off(move || c.join(&j)).await.unwrap_err();
    assert_eq!(refused.code(), Some("two_factor_required"), "{refused:?}");
    assert!(!client.status().unwrap().signed_in);
    // A wrong code is `two_factor_invalid`; the invite still works.
    server.advance(30_000);
    let right = code_for(&secret, server.now());
    let c = client.clone();
    let j = JoinRequest { totp_code: Some(wrong_code(&right)), ..join.clone() };
    let invalid = off(move || c.join(&j)).await.unwrap_err();
    assert!(matches!(&invalid, Error::Refused { code, .. } if code == "two_factor_invalid"), "{invalid:?}");
    let c = client.clone();
    let j = JoinRequest { totp_code: Some(right), ..join.clone() };
    assert!(off(move || c.join(&j)).await.unwrap().signed_in);
    // A recovery code does it too, on a new invite.
    let invite = Code::generate(INVITE_CHARS).unwrap();
    server.meta.create_invite("ana", &invite.hash(), server.now(), INVITE_TTL_MS).await.unwrap();
    let third = Machine::start().await;
    let c = third.client();
    let j = JoinRequest {
        join: format!("{}/{}", server.url, invite.display()),
        folder: Some(third.folder.display().to_string()),
        recovery_code: Some(recovery[2].clone()),
        ..join
    };
    assert!(off(move || c.join(&j)).await.unwrap().signed_in);
    let _ = (first.stop().await, second.stop().await, third.stop().await);
}

/// The new calls take small bodies only, and what is secret is kept nowhere: not in `account.json`, not in what `run` prints.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_secret_is_stored_or_printed_and_bodies_are_small() {
    let server = Server::start().await;
    let machine = Machine::start().await;
    let (secret, recovery) = protect(&server, &machine).await;
    sign_out(&machine).await;
    let client = machine.client();
    let request = machine.login_request(&server);
    let c = client.clone();
    let challenge = off(move || c.login(&request)).await.unwrap_err().challenge().cloned().unwrap();
    server.advance(30_000);
    let used = code_for(&secret, server.now());
    let step = machine.second_step(&server, &challenge.challenge, Some(&used), None);
    let c = client.clone();
    off(move || c.login_second_factor(&step)).await.unwrap();

    // Big bodies are refused before they are read.
    let api = machine.api();
    let big = format!(r#"{{"password":"{}"}}"#, "x".repeat(6000));
    for path in ["/v1/account/two-factor/setup", "/v1/login/second-factor", "/v1/account/password"] {
        let (status, _) = raw(
            api.port,
            &format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json", api.port, api.token),
            &big,
        );
        assert_eq!(status, 413, "{path}");
    }
    let account_json = std::fs::read_to_string(machine.data.path().join("account.json")).unwrap();
    let mut every_file = String::new();
    collect(machine.data.path(), &mut every_file);
    let (exit, out) = machine.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    for secret_text in std::iter::once(used).chain(std::iter::once(challenge.challenge)).chain(recovery).chain(std::iter::once(PASSWORD.to_owned())) {
        assert!(!account_json.contains(&secret_text), "{secret_text} in account.json");
        assert!(!every_file.contains(&secret_text), "{secret_text} in the data folder");
        assert!(!out.contains(&secret_text), "{secret_text} in what run printed");
    }
}

fn collect(dir: &Path, into: &mut String) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, into);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            // The local API's own token is this folder's business, not a second factor.
            if path.file_name().is_some_and(|n| n != LOCAL_API_FILE) {
                into.push_str(&text);
            }
        }
    }
}

/// One raw HTTP/1.1 request, so a test controls every header.
fn raw(port: u16, head: &str, body: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let request = format!("{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    let status = answer.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, answer.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default())
}

// ---- the command line ----

fn cli_login(url: &str, data: &Path, folder: &Path) -> cli::Cli {
    cli::Cli::try_parse_from([
        "craft-cloud-agent",
        "--data-dir",
        data.to_str().unwrap(),
        "login",
        url,
        "--user",
        "ana",
        "--folder",
        folder.to_str().unwrap(),
        "--adopt",
    ])
    .unwrap()
}

/// A signed-in, protected account from the command line's point of view: the secret and a recovery code. Another agent's data
/// folder made it, through the server's own routes.
async fn protected_account(server: &Server) -> (Vec<u8>, Vec<String>) {
    let machine = Machine::start().await;
    let made = protect(server, &machine).await;
    let _ = machine.stop().await;
    made
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_command_line_asks_for_the_code_on_standard_input() {
    let server = Server::start().await;
    let (secret, recovery) = protected_account(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let (data, folder) = (dir.path().join("data"), dir.path().join("Craft Cloud"));

    // A wrong code, then the right one: two lines after the password, and the code is never an argument.
    server.advance(30_000);
    let right = code_for(&secret, server.now());
    let mut out = Vec::new();
    let stdin = format!("{PASSWORD}\n{}\n{right}\n", wrong_code(&right));
    let exit = cli::run(cli_login(&server.url, &data, &folder), &mut stdin.as_bytes(), &mut out).await.unwrap();
    let printed = String::from_utf8(out).unwrap();
    assert_eq!(exit, Exit::Done, "{printed}");
    assert!(printed.contains("two-factor sign-in") && printed.contains("signed in as ana"), "{printed}");
    assert!(!printed.contains(&right) && !printed.contains(PASSWORD), "nothing secret is echoed: {printed}");

    // Three wrong codes: it gives up with the server's message, and nothing is signed in.
    let dir = tempfile::tempdir().unwrap();
    let (data, folder) = (dir.path().join("data"), dir.path().join("Craft Cloud"));
    let wrong = wrong_code(&right);
    let stdin = format!("{PASSWORD}\n{wrong}\n{wrong}\n{wrong}\n");
    let err = cli::run(cli_login(&server.url, &data, &folder), &mut stdin.as_bytes(), &mut Vec::new()).await.unwrap_err();
    assert!(matches!(err, AgentError::TwoFactorInvalid(_)), "{err:?}");
    assert!(DataDir::new(data.clone()).load_account().unwrap().is_none());

    // A recovery code instead of a code.
    let stdin = format!("{PASSWORD}\n{}\n", recovery[0].to_lowercase());
    let mut out = Vec::new();
    let exit = cli::run(cli_login(&server.url, &data, &folder), &mut stdin.as_bytes(), &mut out).await.unwrap();
    assert_eq!(exit, Exit::Done, "{}", String::from_utf8_lossy(&out));

    // Nothing on standard input where the code should be: a plain usage error, no panic.
    let dir = tempfile::tempdir().unwrap();
    let (data, folder) = (dir.path().join("data"), dir.path().join("Craft Cloud"));
    let stdin = format!("{PASSWORD}\n");
    let err = cli::run(cli_login(&server.url, &data, &folder), &mut stdin.as_bytes(), &mut Vec::new()).await.unwrap_err();
    assert!(matches!(err, AgentError::Usage(ref m) if m.contains("no code")), "{err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lock_says_how_long_to_wait() {
    let server = Server::start().await;
    let machine = Machine::start().await;
    let (secret, _) = protect(&server, &machine).await;
    let client = machine.client();
    for _ in 0..10 {
        let proof = TwoFactorProofRequest { password: PASSWORD.into(), totp_code: Some(bad_code(&secret, &server)), recovery_code: None };
        let c = client.clone();
        let _ = off(move || c.two_factor_disable(&proof)).await;
    }
    let _ = machine.stop().await;
    let dir = tempfile::tempdir().unwrap();
    let (data, folder) = (dir.path().join("data"), dir.path().join("Craft Cloud"));
    let stdin = format!("{PASSWORD}\n{}\n", code_for(&secret, server.now() + 30_000));
    let err = cli::run(cli_login(&server.url, &data, &folder), &mut stdin.as_bytes(), &mut Vec::new()).await.unwrap_err();
    match err {
        AgentError::TwoFactorLocked { retry_after_secs, message } => {
            assert_eq!(retry_after_secs, Some(900));
            assert!(message.contains("15 minutes"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_command_line_joins_with_a_code_and_through_a_running_agent() {
    let server = Server::start().await;
    let (secret, _) = protected_account(&server).await;
    let invite = Code::generate(INVITE_CHARS).unwrap();
    server.meta.create_invite("ana", &invite.hash(), server.now(), INVITE_TTL_MS).await.unwrap();
    server.advance(30_000);
    let right = code_for(&secret, server.now());
    let dir = tempfile::tempdir().unwrap();
    let (data, folder) = (dir.path().join("data"), dir.path().join("Craft Cloud"));
    let join = cli::Cli::try_parse_from([
        "craft-cloud-agent",
        "--data-dir",
        data.to_str().unwrap(),
        "join",
        &format!("{}/{}", server.url, invite.display()),
        "--folder",
        folder.to_str().unwrap(),
        "--adopt",
    ])
    .unwrap();
    let mut out = Vec::new();
    let stdin = format!("{right}\n");
    let exit = cli::run(join, &mut stdin.as_bytes(), &mut out).await.unwrap();
    let printed = String::from_utf8(out).unwrap();
    assert_eq!(exit, Exit::Done, "{printed}");
    assert!(printed.contains("two-factor sign-in") && printed.contains("joined as ana"), "{printed}");

    // `login` while `run` holds the data folder goes through the running agent, second step and all.
    let machine = Machine::start().await;
    server.advance(30_000);
    let right = code_for(&secret, server.now());
    let data = machine.data.path().to_path_buf();
    let stdin = format!("{PASSWORD}\n{right}\n");
    let mut out = Vec::new();
    let exit = cli::run(cli_login(&server.url, &data, &machine.folder), &mut stdin.as_bytes(), &mut out).await.unwrap();
    let printed = String::from_utf8(out).unwrap();
    assert_eq!(exit, Exit::Done, "{printed}");
    assert!(printed.contains("through the running agent"), "{printed}");
    assert!(machine.client().status().unwrap().signed_in);
    let _ = machine.stop().await;
}

// ---- the admin pages' confirmation with a second factor ----

use craft_cloud_app::{AdminConfirm, AdminPasswordReset, UserQuery};

impl Server {
    /// The same server with `root`, an administrator with the same password.
    async fn start_with_admin() -> Self {
        let server = Self::start().await;
        server.meta.create_user("root", &hash_password(PASSWORD).unwrap(), Role::Admin, 0).await.unwrap();
        server
    }
}

impl Machine {
    fn login_as(&self, server: &Server, user: &str) -> LoginRequest {
        LoginRequest { user: user.into(), device_name: Some(format!("{user}-pc")), ..self.login_request(server) }
    }
}

/// A confirmation with the password and, if given, a code (six digits) or recovery code.
fn confirm_with(code: Option<&str>) -> AdminConfirm {
    let mut confirm = AdminConfirm { password: Some(PASSWORD.into()), ..AdminConfirm::default() };
    if let Some(code) = code {
        if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) {
            confirm.totp_code = Some(code.into());
        } else {
            confirm.recovery_code = Some(code.into());
        }
    }
    confirm
}

/// An admin whose account has two-factor sign-in confirms each change with a code too: the agent passes the code, and the
/// server's `two_factor_invalid` (403) and `two_factor_locked` (429, with the wait) come back typed, never as a signed-out
/// agent. A reset code with `reset_two_factor` and the reset of an account's two-factor sign-in carry their fields.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_with_two_factor_confirms_changes_with_a_code_through_the_agent() {
    let server = Server::start_with_admin().await;
    let admin = Machine::start().await;
    let client = admin.client();
    let c = client.clone();
    let request = admin.login_as(&server, "root");
    assert!(off(move || c.login(&request)).await.unwrap().signed_in);

    // The account says it is an admin, without two-factor sign-in yet.
    let c = client.clone();
    let me = off(move || c.account()).await.unwrap();
    assert_eq!((me.username.as_str(), me.role.as_str(), me.two_factor_enabled), ("root", "admin", false));
    // Without it, a password is enough.
    let ana = server.meta.user_by_name("ana").await.unwrap().unwrap();
    let ana_id = ana.id.0;
    let c = client.clone();
    off(move || c.admin_disable_user(ana_id, &confirm_with(None))).await.unwrap();
    let c = client.clone();
    off(move || c.admin_enable_user(ana_id, &confirm_with(None))).await.unwrap();

    // Two-factor sign-in on for the admin.
    let setup = {
        let c = client.clone();
        off(move || c.two_factor_setup(PASSWORD)).await.unwrap()
    };
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let first = code_for(&secret, server.now());
    let c = client.clone();
    let recovery = off(move || c.two_factor_confirm(&first)).await.unwrap().recovery_codes;
    let c = client.clone();
    assert!(off(move || c.account()).await.unwrap().two_factor_enabled);

    // A password alone, or with a wrong code: 403 `two_factor_invalid`; the agent is as it was.
    let c = client.clone();
    let missing = off(move || c.admin_disable_user(ana_id, &confirm_with(None))).await.unwrap_err();
    assert_eq!(missing.code(), Some("two_factor_invalid"), "{missing:?}");
    server.advance(30_000);
    let right = code_for(&secret, server.now());
    let wrong = wrong_code(&right);
    let c = client.clone();
    let bad = off(move || c.admin_disable_user(ana_id, &confirm_with(Some(&wrong)))).await.unwrap_err();
    assert_eq!(bad.code(), Some("two_factor_invalid"), "{bad:?}");
    let status = client.status().unwrap();
    assert!(status.signed_in && !status.removed, "{status:?}");
    assert!(!server.meta.admin_user(ana.id, 0).await.unwrap().unwrap().disabled_ms.is_some(), "nothing changed");
    // The right code: done.
    let c = client.clone();
    let fresh = right.clone();
    off(move || c.admin_disable_user(ana_id, &confirm_with(Some(&fresh)))).await.unwrap();
    assert!(server.meta.admin_user(ana.id, 0).await.unwrap().unwrap().disabled_ms.is_some());
    // A recovery code, in the other field, once.
    let c = client.clone();
    let code = recovery[0].clone();
    off(move || c.admin_enable_user(ana_id, &confirm_with(Some(&code)))).await.unwrap();
    assert!(server.meta.admin_user(ana.id, 0).await.unwrap().unwrap().disabled_ms.is_none());
    let c = client.clone();
    let code = recovery[0].clone();
    let used = off(move || c.admin_disable_user(ana_id, &confirm_with(Some(&code)))).await.unwrap_err();
    assert_eq!(used.code(), Some("two_factor_invalid"), "a recovery code works once: {used:?}");

    // A reset code that also turns two-factor off when it is used: the fields travel.
    server.advance(30_000);
    let reset = AdminPasswordReset {
        password: Some(PASSWORD.into()),
        totp_code: Some(code_for(&secret, server.now())),
        reset_two_factor: true,
        ..AdminPasswordReset::default()
    };
    let c = client.clone();
    let issued = off(move || c.admin_issue_reset(ana_id, &reset)).await.unwrap();
    assert!(!issued.code.is_empty());

    // The list shows who has two-factor sign-in; resetting it for an account that has it.
    let ana_machine = Machine::start().await;
    let ana_client = ana_machine.client();
    let c = ana_client.clone();
    let ana_request = ana_machine.login_as(&server, "ana");
    // (Ana's password was not changed: the reset code is only issued.)
    assert!(off(move || c.login(&ana_request)).await.unwrap().signed_in);
    let c = ana_client.clone();
    let ana_setup = off(move || c.two_factor_setup(PASSWORD)).await.unwrap();
    let ana_secret = totp::base32_decode(&ana_setup.secret).unwrap();
    let ana_first = code_for(&ana_secret, server.now());
    let c = ana_client.clone();
    off(move || c.two_factor_confirm(&ana_first)).await.unwrap();
    let c = client.clone();
    let listed = off(move || c.admin_users(&UserQuery { query: Some("ana".into()), ..UserQuery::default() })).await.unwrap();
    assert!(listed.users.iter().any(|u| u.username == "ana" && u.two_factor_enabled), "{listed:?}");
    server.advance(30_000);
    let c = client.clone();
    let fresh = code_for(&secret, server.now());
    off(move || c.admin_reset_two_factor(ana_id, &confirm_with(Some(&fresh)))).await.unwrap();
    let c = ana_client.clone();
    let off_state = off(move || c.two_factor_status()).await;
    // Ana's device was signed out with it: her agent is told so, as a revoked device.
    assert!(off_state.is_err() || !off_state.unwrap().enabled);
    let c = client.clone();
    let listed = off(move || c.admin_users(&UserQuery { query: Some("ana".into()), ..UserQuery::default() })).await.unwrap();
    assert!(listed.users.iter().any(|u| u.username == "ana" && !u.two_factor_enabled), "{listed:?}");

    // Too many wrong codes stop the confirmation: the server's per-account limit says `429 rate_limited` first (wrong codes
    // count like wrong passwords), and its lock `429 two_factor_locked` with the wait comes after more; either is kept as the
    // server said it (the code, the status, the wait if there is one), and the agent stays signed in.
    let mut stopped = None;
    for _ in 0..12 {
        let c = client.clone();
        let wrong = wrong_code(&code_for(&secret, server.now()));
        let refused = off(move || c.admin_disable_user(ana_id, &confirm_with(Some(&wrong)))).await.unwrap_err();
        if matches!(refused.code(), Some("rate_limited" | "two_factor_locked")) {
            stopped = Some(refused);
            break;
        }
        assert_eq!(refused.code(), Some("two_factor_invalid"), "{refused:?}");
    }
    let stopped = stopped.expect("too many wrong codes are stopped");
    if stopped.code() == Some("two_factor_locked") {
        assert!(stopped.retry_after_secs().is_some_and(|secs| secs > 0), "{stopped:?}");
    }
    assert!(client.status().unwrap().signed_in);

    let (exit, out) = admin.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    for secret in [PASSWORD, issued.code.as_str()] {
        assert!(!out.contains(secret), "the agent printed a secret: {out}");
    }
    let _ = ana_machine.stop().await;
}
