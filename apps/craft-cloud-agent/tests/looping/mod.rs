//! Running agents (`run`, each on a thread and runtime of its own, as the binary runs it) against a real server on a
//! loopback port behind a gate that can take it offline: for the recents tests.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use craft_cloud_agent::account::DataDir;
use craft_cloud_agent::cli::{self, AgentError, Exit, LoginArgs};
use craft_cloud_agent::daemon::{RunOptions, run_loop};
use craft_cloud_agent::schedule::Timing;
use craft_cloud_meta::{Meta, Role};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use tokio::sync::mpsc;

pub const PASSWORD: &str = "correct horse battery";
pub const PATIENCE: Duration = Duration::from_secs(30);

/// A loopback port in front of the server: it forwards each connection, or closes them all while the server is
/// "down" (connections already open are cut too).
pub struct Gate {
    pub addr: SocketAddr,
    up: std::sync::Arc<std::sync::atomic::AtomicBool>,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

/// Copies `from` to `to`, each piece `delay` after it was read (in order: a piece never overtakes the one before).
async fn delayed_copy(mut from: tokio::net::tcp::OwnedReadHalf, mut to: tokio::net::tcp::OwnedWriteHalf, delay: Duration) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = from.read(&mut buf).await {
            if n == 0 || tx.send((Instant::now() + delay, buf[..n].to_vec())).is_err() {
                return;
            }
        }
    });
    while let Some((at, piece)) = rx.recv().await {
        tokio::time::sleep_until(at.into()).await;
        if to.write_all(&piece).await.is_err() {
            break;
        }
    }
    reader.abort();
}

impl Gate {
    /// A gate that delays every message by `delay` each way (a LAN's latency on loopback).
    async fn delayed(to: SocketAddr, delay: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            while let Ok((inbound, _)) = listener.accept().await {
                let _ = inbound.set_nodelay(true);
                tokio::spawn(async move {
                    let Ok(outbound) = tokio::net::TcpStream::connect(to).await else { return };
                    let _ = outbound.set_nodelay(true);
                    let (in_read, in_write) = inbound.into_split();
                    let (out_read, out_write) = outbound.into_split();
                    tokio::join!(delayed_copy(in_read, out_write, delay), delayed_copy(out_read, in_write, delay));
                });
            }
        });
        let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        Self { addr, up, generation: std::sync::Arc::default(), task }
    }

    async fn new(to: SocketAddr) -> Self {
        use std::sync::atomic::Ordering;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (is_up, current) = (up.clone(), generation.clone());
        let task = tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                if !is_up.load(Ordering::SeqCst) {
                    continue;
                }
                let (born, current) = (current.load(Ordering::SeqCst), current.clone());
                tokio::spawn(async move {
                    let Ok(mut outbound) = tokio::net::TcpStream::connect(to).await else { return };
                    let copy = tokio::io::copy_bidirectional(&mut inbound, &mut outbound);
                    let cut = async {
                        while current.load(Ordering::SeqCst) == born {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    };
                    tokio::select! {
                        _ = copy => {}
                        () = cut => {}
                    }
                });
            }
        });
        Self { addr, up, generation, task }
    }

    /// Cuts every connection and refuses new ones.
    pub fn down(&self) {
        use std::sync::atomic::Ordering;
        self.up.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn up(&self) {
        self.up.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct Server {
    pub url: String,
    pub gate: Gate,
    pub meta: Meta,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Server {
    pub async fn new() -> Self {
        Self::start(None).await
    }

    /// A server whose every message takes `delay` each way (a gate that can't go down).
    pub async fn delayed(delay: Duration) -> Self {
        Self::start(Some(delay)).await
    }

    async fn start(delay: Option<Duration>) -> Self {
        let (db, blobs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
        meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
        let state = AppState::new(meta.clone(), Store::new(DiskBackend::new(blobs.path()).unwrap()), vec![]).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gate = match delay {
            Some(delay) => Gate::delayed(listener.local_addr().unwrap(), delay).await,
            None => Gate::new(listener.local_addr().unwrap()).await,
        };
        tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending()));
        Self { url: format!("http://{}", gate.addr), gate, meta, _dirs: (db, blobs) }
    }
}

pub struct Agent {
    pub data: DataDir,
    pub folder: PathBuf,
    _dir: tempfile::TempDir,
}

impl Agent {
    pub async fn signed_in(server: &Server, device: &str) -> Self {
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

    pub fn signed_out() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self { data: DataDir::new(dir.path().join("data")), folder: dir.path().join("Craft Cloud"), _dir: dir }
    }

    pub fn run(&self) -> Running {
        let (stop, stops) = mpsc::unbounded_channel();
        let data = self.data.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut out = Vec::new();
            let exit = runtime.block_on(run_loop(&data, quick(), stops, &mut out));
            (exit, String::from_utf8_lossy(&out).into_owned())
        });
        Running { stop, thread: Some(thread) }
    }

    /// The crate's client of this agent, once its local API is up.
    pub async fn client(&self) -> craft_cloud_app::Client {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let dir = self.data.path().to_owned();
            if let Ok(client) = tokio::task::spawn_blocking(move || craft_cloud_app::Client::connect_in(&dir)).await.unwrap() {
                return client;
            }
            assert!(Instant::now() < deadline, "the local API comes up");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

pub fn quick() -> RunOptions {
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
        // Never this machine's own start-at-login entry.
        autostart: craft_cloud_agent::autostart::Target::Off,
    }
}

pub type LoopResult = (Result<Exit, AgentError>, String);

pub struct Running {
    stop: mpsc::UnboundedSender<()>,
    thread: Option<std::thread::JoinHandle<LoopResult>>,
}

impl Running {
    pub async fn stop(mut self) -> LoopResult {
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

/// Waits until `cond` holds (it may block: it runs off the runtime), failing after [`PATIENCE`].
pub async fn eventually<T: Send + 'static>(what: &str, cond: impl Fn() -> Option<T> + Send + Sync + 'static) -> T {
    let cond = std::sync::Arc::new(cond);
    let deadline = Instant::now() + PATIENCE;
    loop {
        let check = cond.clone();
        if let Some(value) = tokio::task::spawn_blocking(move || check()).await.unwrap() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
