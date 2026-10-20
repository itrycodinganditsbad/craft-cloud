//! The agent on its own: `run` loops (each on its own thread and runtime, as in the binary) with a real folder
//! watcher, against a real server on a loopback port. Every wait is for a condition, with a timeout; the loops get
//! short timings so the tests stay quick.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use craft_cloud_agent::account::{DataDir, LoopState};
use craft_cloud_agent::cli::{self, AgentError, Exit, LoginArgs};
use craft_cloud_agent::daemon::{RunOptions, run_loop};
use craft_cloud_agent::lock::LockError;
use craft_cloud_agent::schedule::Timing;
use craft_cloud_meta::{Meta, Role};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use tokio::sync::mpsc;

const PASSWORD: &str = "correct horse battery";
/// Longest wait for anything to show up.
const PATIENCE: Duration = Duration::from_secs(30);

/// What the [`Gate`] does with a new connection.
#[derive(Debug, Clone, Copy)]
enum Gateway {
    /// Forwards it to the server at this address.
    To(SocketAddr),
    /// Closes it at once, as a server that is down does.
    Down,
    /// Holds it open and never answers.
    Silent,
}

/// A loopback port the agents talk to for a whole test, in front of the server: it forwards each connection to the
/// server's current address, closes them while the server is down, or holds them without an answer. A server
/// stopped and started again gets a fresh port of its own, so no test ever binds the same port twice (Windows can
/// keep a port in use for a long while after it was closed).
struct Gate {
    addr: SocketAddr,
    mode: std::sync::Arc<std::sync::Mutex<Gateway>>,
    task: tokio::task::JoinHandle<()>,
}

impl Gate {
    async fn new(to: SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mode = std::sync::Arc::new(std::sync::Mutex::new(Gateway::To(to)));
        let current = mode.clone();
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((mut inbound, _)) = listener.accept().await {
                let gateway = *current.lock().unwrap();
                match gateway {
                    Gateway::To(server) => {
                        tokio::spawn(async move {
                            if let Ok(mut outbound) = tokio::net::TcpStream::connect(server).await {
                                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                            }
                        });
                    }
                    Gateway::Down => drop(inbound),
                    Gateway::Silent => held.push(inbound),
                }
            }
        });
        Self { addr, mode, task }
    }

    fn set(&self, gateway: Gateway) {
        *self.mode.lock().unwrap() = gateway;
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A server whose database and chunks live in folders that outlast it, so it can be stopped and started again,
/// behind a [`Gate`] whose address the agents keep.
struct Server {
    url: String,
    gate: Gate,
    db: tempfile::TempDir,
    blobs: tempfile::TempDir,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Server {
    async fn new() -> Self {
        let (db, blobs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gate = Gate::new(listener.local_addr().unwrap()).await;
        let mut server = Self { url: format!("http://{}", gate.addr), gate, db, blobs, shutdown: None, task: None };
        let meta = server.serve(listener).await;
        meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
        server
    }

    async fn serve(&mut self, listener: tokio::net::TcpListener) -> Meta {
        let meta = Meta::open(&format!("sqlite:{}", self.db.path().join("meta.db").display())).await.unwrap();
        let store = Store::new(DiskBackend::new(self.blobs.path()).unwrap());
        let state = AppState::new(meta.clone(), store, vec![]).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        self.task = Some(tokio::spawn(async move {
            let _ = craft_cloud_server::serve(state, listener, async move {
                let _ = rx.await;
            })
            .await;
        }));
        self.shutdown = Some(tx);
        meta
    }

    /// Stops the server; agents find it offline.
    async fn stop(&mut self) {
        self.gate.set(Gateway::Down);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// Starts it again, on a fresh port behind the same gate.
    async fn start(&mut self) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        self.serve(listener).await;
        self.gate.set(Gateway::To(addr));
    }
}

struct Agent {
    data: DataDir,
    folder: PathBuf,
    _dir: tempfile::TempDir,
}

impl Agent {
    async fn signed_in(server: &Server, device: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let agent = Self { data: DataDir::new(dir.path().join("data")), folder: dir.path().join("Craft Cloud"), _dir: dir };
        let args = LoginArgs {
            url: server.url.clone(),
            user: "ana".into(),
            folder: Some(agent.folder.clone()),
            device_name: Some(device.into()),
            adopt: false,
        };
        cli::login(&agent.data, &args, PASSWORD, &mut Vec::new()).await.unwrap();
        agent
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.folder.join(rel)
    }

    fn write(&self, rel: &str, data: &str) {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.path(rel)).ok()
    }

    async fn status(&self) -> String {
        let mut out = Vec::new();
        cli::status(&self.data, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn loop_state(&self) -> Option<LoopState> {
        self.data.load_status().unwrap().map(|s| s.state)
    }

    /// The loop is idle after a pass that finished.
    fn idle_after_a_pass(&self) -> bool {
        self.data.load_status().unwrap().is_some_and(|s| {
            s.state == LoopState::Idle && s.last_pass.is_some_and(|p| !matches!(p.outcome, craft_cloud_agent::account::Outcome::Failed(_)))
        })
    }

    /// Starts `run` for this agent on a thread of its own, with its own runtime (as the binary runs it).
    fn run(&self) -> Running {
        self.run_with(quick())
    }

    /// [`Agent::run`] with other options.
    fn run_with(&self, options: RunOptions) -> Running {
        let (stop, stops) = mpsc::unbounded_channel();
        let data = self.data.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut out = Vec::new();
            let exit = runtime.block_on(run_loop(&data, options, stops, &mut out));
            (exit, String::from_utf8_lossy(&out).into_owned())
        });
        Running { stop, thread: Some(thread) }
    }
}

/// Timings for tests: everything in fractions of a second, a safety-net pass far away (so it never hides a
/// trigger that doesn't work).
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
        // Never this machine's own start-at-login entry.
        autostart: craft_cloud_agent::autostart::Target::Off,
    }
}

type LoopResult = (Result<Exit, AgentError>, String);

struct Running {
    stop: mpsc::UnboundedSender<()>,
    thread: Option<std::thread::JoinHandle<LoopResult>>,
}

impl Running {
    /// Asks the loop to stop (as one Ctrl-C does) and waits for it.
    async fn stop(mut self) -> LoopResult {
        // A loop that ended by itself (it couldn't start) no longer listens.
        let _ = self.stop.send(());
        let thread = self.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap()).await.unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // A test that failed midway: two stops end the loop at once.
        let _ = self.stop.send(());
        let _ = self.stop.send(());
    }
}

/// Waits until `cond` holds, failing after [`PATIENCE`].
async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn files(folder: &Path) -> Vec<String> {
    let mut out: Vec<String> =
        std::fs::read_dir(folder).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).filter(|n| n != ".craft-cloud").collect();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_loops_keep_two_folders_in_step_and_settle() {
    let server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    let b = Agent::signed_in(&server, "desktop").await;
    let (run_a, run_b) = (a.run(), b.run());
    a.write("notes.txt", "from the laptop");
    eventually("the laptop's file reaches the desktop", || b.read("notes.txt").as_deref() == Some("from the laptop")).await;
    b.write("notes.txt", "edited on the desktop");
    b.write("Shots/010.psd", "layers");
    eventually("the desktop's edit reaches the laptop", || {
        a.read("notes.txt").as_deref() == Some("edited on the desktop") && a.read("Shots/010.psd").as_deref() == Some("layers")
    })
    .await;
    std::fs::remove_file(a.path("Shots/010.psd")).unwrap();
    eventually("the delete reaches the desktop", || b.read("Shots/010.psd").is_none()).await;
    assert!(a.status().await.contains("agent: running"), "{}", a.status().await);
    assert!(matches!(a.loop_state(), Some(LoopState::Idle | LoopState::Syncing)));

    // Neither loop's own writes keep it busy: both settle, with no pass for a good while.
    for agent in [&a, &b] {
        let mut last = (None, Instant::now());
        eventually("the loop settles", || {
            let finished = agent.data.load_status().unwrap().and_then(|s| s.last_pass).map(|p| p.finished_ms);
            if finished != last.0 {
                last = (finished, Instant::now());
            }
            last.1.elapsed() > Duration::from_millis(2_500)
        })
        .await;
    }
    assert_eq!(files(&a.folder), files(&b.folder));

    let (exit, out) = run_a.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(out.contains("stopped"), "{out}");
    assert_eq!(a.loop_state(), Some(LoopState::Stopped));
    assert!(a.status().await.contains("agent: not running"), "{}", a.status().await);
    let (exit, out) = run_b.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_agent_and_a_plain_sync_are_refused_while_the_loop_runs() {
    let server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    let running = a.run();
    eventually("the loop is up", || a.idle_after_a_pass()).await;
    let second = a.run();
    let (exit, _) = second.stop().await;
    assert!(matches!(exit, Err(AgentError::Lock(LockError::Busy(_)))), "{exit:?}");
    let err = cli::sync(&a.data, false, &mut Vec::new()).await.unwrap_err();
    assert_eq!(err.exit(), Exit::NeedsYou, "{err}");
    assert!(err.to_string().contains("running"), "{err}");
    let (exit, _) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done);
    // Once it stopped, a plain sync works again.
    assert_eq!(cli::sync(&a.data, false, &mut Vec::new()).await.unwrap().0, Exit::Done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deletions_wait_for_an_approval_given_while_the_loop_runs() {
    let server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    let b = Agent::signed_in(&server, "desktop").await;
    for i in 0..30 {
        b.write(&format!("f{i}.txt"), "content");
    }
    cli::sync(&b.data, false, &mut Vec::new()).await.unwrap();
    let running = a.run();
    eventually("the files arrive", || files(&a.folder).len() == 30).await;
    // The desktop deletes most of them (approving its own side by hand).
    for i in 0..25 {
        std::fs::remove_file(b.path(&format!("f{i}.txt"))).unwrap();
    }
    assert_eq!(cli::sync(&b.data, false, &mut Vec::new()).await.unwrap().0, Exit::NeedsApproval);
    assert_eq!(cli::sync(&b.data, true, &mut Vec::new()).await.unwrap().0, Exit::Done);
    // The loop holds the deletions back here and says so.
    eventually("the loop waits for approval", || a.data.load_status().unwrap().is_some_and(|s| s.waiting_for_approval)).await;
    assert!(a.status().await.contains("waiting for approval"), "{}", a.status().await);
    assert_eq!(files(&a.folder).len(), 30);
    // Approved from another terminal while the loop holds the locks.
    let mut out = Vec::new();
    let (exit, shown) = cli::sync(&a.data, true, &mut out).await.unwrap();
    assert_eq!(exit, Exit::Done);
    assert_eq!(shown.pending_local_deletes.len(), 25, "{}", String::from_utf8_lossy(&out));
    eventually("the approved deletions are carried out", || files(&a.folder).len() == 5).await;
    eventually("nothing waits any more", || a.data.load_status().unwrap().is_some_and(|s| !s.waiting_for_approval)).await;
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(out.contains("approval received"), "{out}");
}

/// The deletions waiting for approval here (from the server), from the loop's last finished pass.
fn waiting_here(agent: &Agent) -> usize {
    agent.data.load_state().unwrap().last_pass.and_then(|p| p.summary).map_or(0, |s| s.pending_local_deletes.len())
}

/// How many files the loop's last finished pass downloaded.
fn last_downloaded(agent: &Agent) -> usize {
    agent.data.load_state().unwrap().last_pass.and_then(|p| p.summary).map_or(0, |s| s.downloaded)
}

/// An approval covers only the set `status` showed: deletions that came after it make `sync --approve-deletes` show
/// the new list and approve nothing (exit code 2); run again, it approves the list it just showed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_covers_only_the_deletions_status_showed() {
    let server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    let b = Agent::signed_in(&server, "desktop").await;
    for i in 0..30 {
        b.write(&format!("f{i}.txt"), "content");
    }
    cli::sync(&b.data, false, &mut Vec::new()).await.unwrap();
    let running = a.run();
    eventually("the files arrive", || files(&a.folder).len() == 30).await;
    let delete_on_b = |range: std::ops::Range<usize>| {
        for i in range {
            std::fs::remove_file(b.path(&format!("f{i}.txt"))).unwrap();
        }
    };
    delete_on_b(0..25);
    assert_eq!(cli::sync(&b.data, false, &mut Vec::new()).await.unwrap().0, Exit::NeedsApproval);
    assert_eq!(cli::sync(&b.data, true, &mut Vec::new()).await.unwrap().0, Exit::Done);
    eventually("the loop lists 25 deletions", || waiting_here(&a) == 25).await;
    let shown = a.status().await;
    assert!(shown.contains("waiting for approval, set "), "{shown}");
    // More deletions arrive after `status` showed the 25.
    delete_on_b(25..27);
    if cli::sync(&b.data, false, &mut Vec::new()).await.unwrap().0 == Exit::NeedsApproval {
        assert_eq!(cli::sync(&b.data, true, &mut Vec::new()).await.unwrap().0, Exit::Done);
    }
    eventually("the loop lists 27 deletions", || waiting_here(&a) == 27).await;
    let mut out = Vec::new();
    let (exit, listed) = cli::sync(&a.data, true, &mut out).await.unwrap();
    let out = String::from_utf8_lossy(&out).into_owned();
    assert_eq!(exit, Exit::NeedsApproval, "{out}");
    assert_eq!(listed.pending_local_deletes.len(), 27, "{out}");
    assert!(out.contains("nothing approved"), "{out}");
    // Nothing goes meanwhile: no approval reached the loop, and a pass after it (for the desktop's new file) still
    // lists the 27 and deletes nothing.
    assert!(!a.data.path().join("approval.json").exists(), "nothing approved");
    b.write("after.txt", "after");
    cli::sync(&b.data, false, &mut Vec::new()).await.unwrap();
    eventually("a pass brings the desktop's new file", || last_downloaded(&a) == 1).await;
    assert_eq!(files(&a.folder).len(), 31);
    assert_eq!(waiting_here(&a), 27);
    // Run again, it approves the list it showed.
    assert_eq!(cli::sync(&a.data, true, &mut Vec::new()).await.unwrap().0, Exit::Done);
    eventually("the approved deletions are carried out", || files(&a.folder).len() == 4).await;
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
}

/// The set the deletions waiting here form, from the last pass.
fn waiting_set(agent: &Agent) -> Option<String> {
    agent.data.load_state().unwrap().last_pass.and_then(|p| p.summary).and_then(|s| s.pending_set())
}

/// `sync --approve-deletes=<set>` approves the set it names if that is the one waiting, without `status` having shown
/// it (the user read it in the loop's output); a wrong set approves nothing, shows the waiting one and exits 2. The
/// same for a one-off `sync` with no loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_can_name_the_set_it_approves() {
    let server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    let b = Agent::signed_in(&server, "desktop").await;
    for i in 0..60 {
        b.write(&format!("f{i}.txt"), "content");
    }
    cli::sync(&b.data, false, &mut Vec::new()).await.unwrap();
    async fn delete_on(b: &Agent, range: std::ops::Range<usize>) {
        for i in range {
            std::fs::remove_file(b.path(&format!("f{i}.txt"))).unwrap();
        }
        if cli::sync(&b.data, false, &mut Vec::new()).await.unwrap().0 == Exit::NeedsApproval {
            assert_eq!(cli::sync(&b.data, true, &mut Vec::new()).await.unwrap().0, Exit::Done);
        }
    }
    let running = a.run();
    eventually("the files arrive", || files(&a.folder).len() == 60).await;
    delete_on(&b, 0..25).await;
    eventually("the loop lists 25 deletions", || waiting_here(&a) == 25).await;
    let set = waiting_set(&a).unwrap();
    // A set that isn't the one waiting: nothing is approved, and the waiting set is shown.
    let mut out = Vec::new();
    let (exit, listed) = cli::sync(&a.data, cli::Approve::Set("0123456789ab".into()), &mut out).await.unwrap();
    let out = String::from_utf8_lossy(&out).into_owned();
    assert_eq!(exit, Exit::NeedsApproval, "{out}");
    assert_eq!(listed.pending_local_deletes.len(), 25, "{out}");
    assert!(out.contains(&format!("waiting for approval, set {set}")) && out.contains("nothing approved"), "{out}");
    // Nothing reached the loop, and a pass after it (for the desktop's new file) deletes nothing.
    assert!(!a.data.path().join("approval.json").exists(), "nothing approved");
    b.write("after.txt", "after");
    cli::sync(&b.data, false, &mut Vec::new()).await.unwrap();
    eventually("a pass brings the desktop's new file", || last_downloaded(&a) == 1).await;
    assert_eq!(files(&a.folder).len(), 61);
    assert_eq!(waiting_set(&a), Some(set.clone()));
    // The set the loop printed: approved, though `status` never ran.
    let (exit, _) = cli::sync(&a.data, cli::Approve::from_flag(Some(Some(set.to_uppercase()))), &mut Vec::new()).await.unwrap();
    assert_eq!(exit, Exit::Done);
    eventually("the approved deletions are carried out", || files(&a.folder).len() == 36).await;
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");

    // A one-off `sync`: the same.
    delete_on(&b, 25..50).await;
    assert_eq!(cli::sync(&a.data, false, &mut Vec::new()).await.unwrap().0, Exit::NeedsApproval);
    let set = waiting_set(&a).unwrap();
    let mut out = Vec::new();
    let (exit, _) = cli::sync(&a.data, cli::Approve::Set("0123456789ab".into()), &mut out).await.unwrap();
    let out = String::from_utf8_lossy(&out).into_owned();
    assert_eq!(exit, Exit::NeedsApproval, "{out}");
    assert!(out.contains(&format!("waiting for approval, set {set}")) && out.contains("nothing approved"), "{out}");
    assert_eq!(files(&a.folder).len(), 36);
    assert_eq!(cli::sync(&a.data, cli::Approve::Set(set), &mut Vec::new()).await.unwrap().0, Exit::Done);
    assert_eq!(files(&a.folder).len(), 11);
}

/// A delete held behind a link (review 7, finding 2: a permanent link that appeared in the same window as an
/// unrelated delete, sharing a folder name with it) is listed with a set of its own. The bare `--approve-deletes`
/// and another set release nothing, in a one-off `sync` and while the loop runs; naming the held set lets the
/// delete go.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_delete_goes_only_when_its_set_is_named() {
    let server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    let b = Agent::signed_in(&server, "desktop").await;
    for i in 0..20 {
        a.write(&format!("pad{i}.txt"), "pad");
    }
    a.write("ProjectA/Renders/r.exr", "r");
    a.write("ProjectB/Renders/old.exr", "old");
    a.write("ProjectB/Renders/keep.exr", "keep");
    cli::sync(&a.data, false, &mut Vec::new()).await.unwrap();
    cli::sync(&b.data, false, &mut Vec::new()).await.unwrap();
    // One window: ProjectA/Renders moves to another drive (a link stays), and the user deletes old.exr.
    let drive = tempfile::tempdir().unwrap();
    std::fs::rename(a.path("ProjectA/Renders"), drive.path().join("Renders")).unwrap();
    std::os::unix::fs::symlink(drive.path().join("Renders"), a.path("ProjectA/Renders")).unwrap();
    std::fs::remove_file(a.path("ProjectB/Renders/old.exr")).unwrap();
    let held_set = |a: &Agent| a.data.load_state().unwrap().last_pass.and_then(|p| p.summary).and_then(|s| s.held_set());
    let mut out = Vec::new();
    assert_eq!(cli::sync(&a.data, false, &mut out).await.unwrap().0, Exit::Done);
    let out = String::from_utf8_lossy(&out).into_owned();
    let set = held_set(&a).expect("a held set");
    assert!(out.contains("ProjectB/Renders/old.exr: held while ProjectA/Renders is a link"), "{out}");
    assert!(out.contains(&format!("--approve-deletes={set}")), "{out}");
    // The bare flag and another set release nothing; the bare flag says how to (review 8, finding 5).
    let released_only_by_naming = format!(
        "released only by naming their set: check their links (`craft-cloud-agent status`), then run `craft-cloud-agent sync --approve-deletes={set}`"
    );
    let mut out = Vec::new();
    assert_eq!(cli::sync(&a.data, true, &mut out).await.unwrap().0, Exit::Done);
    let out = String::from_utf8_lossy(&out).into_owned();
    assert!(out.contains(&released_only_by_naming) && !out.contains("nothing was waiting"), "{out}");
    let mut out = Vec::new();
    cli::sync(&a.data, cli::Approve::Set("0123456789ab".into()), &mut out).await.unwrap();
    assert!(String::from_utf8_lossy(&out).contains("nothing approved"), "{}", String::from_utf8_lossy(&out));
    assert_eq!(held_set(&a), Some(set.clone()));
    cli::sync(&b.data, false, &mut Vec::new()).await.unwrap();
    assert_eq!(b.read("ProjectB/Renders/old.exr").as_deref(), Some("old"));
    // While the loop runs: the bare flag and another set still release nothing.
    let running = a.run();
    eventually("the loop is up", || a.idle_after_a_pass()).await;
    assert!(a.status().await.contains(&format!("--approve-deletes={set}")), "{}", a.status().await);
    // `status.json` says a hold apart from deletions waiting for approval (review 8, finding 2).
    let status = a.data.load_status().unwrap().unwrap();
    assert!(status.held && !status.waiting_for_approval, "{status:?}");
    let mut out = Vec::new();
    cli::sync(&a.data, true, &mut out).await.unwrap();
    let out = String::from_utf8_lossy(&out).into_owned();
    assert!(out.contains(&released_only_by_naming) && !out.contains("nothing was waiting"), "{out}");
    cli::sync(&a.data, cli::Approve::Set("0123456789ab".into()), &mut Vec::new()).await.unwrap();
    assert!(!a.data.path().join("approval.json").exists(), "neither wrote an approval");
    // Syncing goes on while the hold stands (review 8, finding 4), and the pass that sends the new file is a pass
    // after both approvals: the held delete didn't go with it.
    a.write("new.txt", "written while held");
    eventually_synced(&b, "new.txt", "written while held").await;
    assert_eq!(b.read("ProjectB/Renders/old.exr").as_deref(), Some("old"), "still held");
    assert_eq!(held_set(&a), Some(set.clone()));
    // Named: the loop lets the delete go.
    let mut out = Vec::new();
    assert_eq!(cli::sync(&a.data, cli::Approve::Set(set.clone()), &mut out).await.unwrap().0, Exit::Done);
    assert!(String::from_utf8_lossy(&out).contains("delete on the server: ProjectB/Renders/old.exr"), "{}", String::from_utf8_lossy(&out));
    eventually("the held delete goes", || held_set(&a).is_none()).await;
    eventually("status.json says nothing is held", || a.data.load_status().unwrap().is_some_and(|s| !s.held)).await;
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(out.contains("approval received"), "{out}");
    // `run` listed the hold once, though passes ran while it stood (review 8, finding 3), and said when it ended.
    assert_eq!(out.matches("ProjectB/Renders/old.exr: held while ProjectA/Renders is a link").count(), 1, "{out}");
    assert!(out.contains("no delete is held behind a link any more"), "{out}");
    cli::sync(&b.data, false, &mut Vec::new()).await.unwrap();
    assert_eq!(b.read("ProjectB/Renders/old.exr"), None, "the user's delete reached the desktop");
    assert_eq!(b.read("ProjectB/Renders/keep.exr").as_deref(), Some("keep"));
    assert_eq!(b.read("ProjectA/Renders/r.exr").as_deref(), Some("r"), "the link's folder isn't deleted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_offline_server_is_retried_with_backoff_until_it_is_back() {
    let mut server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    let b = Agent::signed_in(&server, "desktop").await;
    let running = a.run();
    eventually("the loop is up", || a.idle_after_a_pass()).await;
    server.stop().await;
    a.write("offline.txt", "written while the server was away");
    eventually("the loop retries", || matches!(a.loop_state(), Some(LoopState::Retrying { .. }))).await;
    assert!(a.status().await.contains("offline, retrying"), "{}", a.status().await);
    server.start().await;
    // The pass right after it is back sees the new file for the first time; it goes up once it has settled.
    eventually_synced(&b, "offline.txt", "written while the server was away").await;
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_folder_pauses_the_loop_until_it_is_back() {
    let server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    a.write("keep.txt", "kept");
    let running = a.run();
    eventually("the loop is up", || a.idle_after_a_pass()).await;
    // The folder's marker goes away (as when a drive isn't mounted): the loop pauses and says why.
    let marker = a.path(".craft-cloud/root.json");
    let saved = std::fs::read(&marker).unwrap();
    std::fs::remove_file(&marker).unwrap();
    a.write("new.txt", "after the folder came back");
    eventually("the loop pauses", || matches!(a.loop_state(), Some(LoopState::Paused { next_check_ms: Some(_), .. }))).await;
    assert!(a.status().await.contains("paused"), "{}", a.status().await);
    std::fs::write(&marker, saved).unwrap();
    // It looks again by itself and goes on.
    let b = Agent::signed_in(&server, "desktop").await;
    eventually_synced(&b, "keep.txt", "kept").await;
    eventually_synced(&b, "new.txt", "after the folder came back").await;
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
}

/// A stop lets the running pass finish; a second one ends the loop at once (exit code 1). The pass here hangs on a
/// server that accepts connections and never answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_stop_ends_the_loop_at_once() {
    let mut server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    server.stop().await;
    server.gate.set(Gateway::Silent);
    let running = a.run();
    eventually("a pass is running", || matches!(a.loop_state(), Some(LoopState::Syncing))).await;
    running.stop.send(()).unwrap();
    eventually("the loop says it stops after the pass", || matches!(a.loop_state(), Some(LoopState::Stopping))).await;
    let started = Instant::now();
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Failed, "{out}");
    assert!(out.contains("stopping at once"), "{out}");
    assert!(started.elapsed() < Duration::from_secs(10), "it didn't wait for the hanging pass");
}

/// A watcher that can't start (the system's watch limit, say) is reported once, in the output and in `status`, and
/// full passes find local changes instead (every minute; here every half second).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watcher_that_cant_start_is_reported_and_full_passes_run_instead() {
    let server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    let b = Agent::signed_in(&server, "desktop").await;
    let running = a.run_with(RunOptions { fail_watch: true, ..quick() });
    eventually("the loop is up", || a.idle_after_a_pass()).await;
    assert!(a.data.load_status().unwrap().is_some_and(|s| s.not_watching.is_some()));
    assert!(a.status().await.contains("not watching the folder"), "{}", a.status().await);
    a.write("unwatched.txt", "found by a full pass");
    eventually_synced(&b, "unwatched.txt", "found by a full pass").await;
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert_eq!(out.matches("not watching the folder").count(), 1, "said once: {out}");
}

/// The watcher's events for a file still settling run no pass: an app writing it over and over doesn't keep the
/// loop scanning. A change to another file does run one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_still_being_written_runs_no_pass_by_its_events() {
    let server = Server::new().await;
    let a = Agent::signed_in(&server, "laptop").await;
    // A long window, so the file's own looks are far away and only events could run a pass.
    let options = RunOptions { timing: Timing { stability: Duration::from_secs(20), ..quick().timing }, ..quick() };
    let running = a.run_with(options);
    eventually("the loop is up", || a.idle_after_a_pass()).await;
    let last_pass = || a.data.load_status().unwrap().and_then(|s| s.last_pass);
    let waiting =
        || last_pass().and_then(|p| p.summary).is_some_and(|s| s.skipped.iter().any(|(p, why)| p == "export.mov" && why.contains("still changing")));
    let mut content = String::from("frame");
    a.write("export.mov", &content);
    eventually("the file waits to settle", waiting).await;
    let finished = last_pass().map(|p| p.finished_ms);
    // The writes go on for 1.5 s, longer than the longest debounce (`max_delay`, 1 s) plus a pass: had they run a
    // pass, it would have ended by the last write. A pass only ever moves `finished_ms` on, so no fixed wait is
    // needed after them.
    for _ in 0..15 {
        content.push_str(" frame");
        a.write("export.mov", &content);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(last_pass().map(|p| p.finished_ms), finished, "no pass ran for the waiting file's own writes");
    }
    a.write("other.txt", "another file");
    eventually("another file's change runs a pass", || last_pass().map(|p| p.finished_ms) != finished).await;
    let summary = last_pass().and_then(|p| p.summary).unwrap();
    assert!(summary.skipped.iter().any(|(p, _)| p == "export.mov"), "the waiting file still waits: {summary:?}");
    let (exit, out) = running.stop().await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
}

/// Syncs `agent` by hand until `rel` holds `want`, failing after [`PATIENCE`].
async fn eventually_synced(agent: &Agent, rel: &str, want: &str) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        cli::sync(&agent.data, false, &mut Vec::new()).await.unwrap();
        if agent.read(rel).as_deref() == Some(want) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {rel} to arrive");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
