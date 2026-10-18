//! Two agents with their own folders and data folders sync through a real server on a loopback port, driven
//! through the command line's own functions (`cli::login`, `cli::sync`, …), as a user would run them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use craft_cloud_agent::account::DataDir;
use craft_cloud_agent::cli::{self, AgentError, Exit, LoginArgs};
use craft_cloud_agent::http::{DeviceToken, HttpTransport, ServerUrl};
use craft_cloud_agent::root::{self, RootError};
use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::ChunkId;
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_store::{Backend, Store, StoreError};
use craft_cloud_sync::{Transport, TransportError};

const PASSWORD: &str = "correct horse battery";

struct Server {
    url: String,
    meta: Meta,
    /// Chunks the server has read from its store (each chunk download reads one). Only the Unix-only
    /// kept-download test reads it.
    #[cfg_attr(not(unix), allow(dead_code))]
    chunk_reads: Arc<AtomicUsize>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dirs: Vec<tempfile::TempDir>,
}

/// The server's disk store, counting the chunks read from it.
struct CountingBackend {
    inner: DiskBackend,
    chunk_reads: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Backend for CountingBackend {
    async fn put_raw(&self, path: &str, data: bytes::Bytes) -> Result<(), StoreError> {
        self.inner.put_raw(path, data).await
    }

    async fn get_raw(&self, path: &str) -> Result<Option<bytes::Bytes>, StoreError> {
        if path.starts_with("chunks/") {
            self.chunk_reads.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.get_raw(path).await
    }

    async fn delete_raw(&self, path: &str) -> Result<(), StoreError> {
        self.inner.delete_raw(path).await
    }
}

async fn server() -> Server {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let chunk_reads = Arc::new(AtomicUsize::new(0));
    let store = Store::new(CountingBackend { inner: DiskBackend::new(blobs.path()).unwrap(), chunk_reads: chunk_reads.clone() });
    let state = AppState::new(meta.clone(), store, vec![]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(craft_cloud_server::serve(state, listener, async move {
        let _ = rx.await;
    }));
    meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
    Server { url, meta, chunk_reads, _shutdown: tx, _dirs: vec![db, blobs] }
}

struct Agent {
    data: DataDir,
    folder: PathBuf,
    _dir: tempfile::TempDir,
}

fn login_args(server: &Server, folder: &Path, device: &str) -> LoginArgs {
    LoginArgs { url: server.url.clone(), user: "ana".into(), folder: Some(folder.to_owned()), device_name: Some(device.into()), adopt: false }
}

impl Agent {
    /// A data folder and a sync folder (not created yet), not signed in.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self { data: DataDir::new(dir.path().join("data")), folder: dir.path().join("Craft Cloud"), _dir: dir }
    }

    async fn signed_in(server: &Server, device: &str) -> Self {
        let agent = Self::new();
        cli::login(&agent.data, &login_args(server, &agent.folder, device), PASSWORD, &mut Vec::new()).await.unwrap();
        agent
    }

    async fn try_sync(&self, approve: bool) -> Result<(Exit, craft_cloud_agent::account::PassSummary), AgentError> {
        cli::sync(&self.data, approve, &mut Vec::new()).await
    }

    /// One pass that must finish; returns its exit code.
    async fn sync(&self) -> Exit {
        self.try_sync(false).await.unwrap().0
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

    /// Every file below the folder (outside `.craft-cloud`), `/`-separated, sorted.
    fn files(&self) -> Vec<String> {
        fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                if name == ".craft-cloud" {
                    continue;
                }
                let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
                if entry.file_type().unwrap().is_dir() {
                    walk(&entry.path(), &rel, out);
                } else {
                    out.push(rel);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.folder, "", &mut out);
        out.sort();
        out
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_sync_goes_both_ways() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.write("a.txt", "from a");
    a.write("Shots/010.psd", "layers");
    assert_eq!(a.sync().await, Exit::Done);

    let b = Agent::signed_in(&server, "laptop").await;
    assert_eq!(b.sync().await, Exit::Done);
    assert_eq!(b.files(), ["Shots/010.psd", "a.txt"]);
    assert_eq!(b.read("Shots/010.psd").as_deref(), Some("layers"));

    b.write("from-b.txt", "hello");
    std::fs::create_dir(b.path("Empty")).unwrap();
    b.sync().await;
    a.sync().await;
    assert_eq!(a.read("from-b.txt").as_deref(), Some("hello"));
    assert!(a.path("Empty").is_dir(), "empty folders sync too");
    assert_eq!(a.files(), b.files());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn big_files_travel_in_chunks_and_keep_their_exec_bit() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    // 9 MB of pseudo-random bytes: several chunks, more than one maximum-size request.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let big: Vec<u8> = (0..9_000_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    std::fs::write(a.path("render.mov"), &big).unwrap();
    std::fs::write(a.path("run.sh"), "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(a.path("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    a.sync().await;
    b.sync().await;
    assert!(std::fs::read(b.path("render.mov")).unwrap() == big);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(b.path("run.sh")).unwrap().permissions().mode() & 0o100, 0o100);
    }
    // An edit in the middle: the rest of the chunks are reused on both sides.
    let mut edited = big.clone();
    edited.splice(4_000_000..4_000_010, *b"an edit!!!");
    std::fs::write(b.path("render.mov"), &edited).unwrap();
    b.sync().await;
    a.sync().await;
    assert!(std::fs::read(a.path("render.mov")).unwrap() == edited);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_with_files_needs_adopt() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.write("notes.txt", "server copy");
    a.sync().await;

    let b = Agent::new();
    std::fs::create_dir_all(&b.folder).unwrap();
    std::fs::write(b.folder.join("notes.txt"), "server copy").unwrap();
    std::fs::write(b.folder.join("mine.txt"), "only here").unwrap();
    let refused = cli::login(&b.data, &login_args(&server, &b.folder, "laptop"), PASSWORD, &mut Vec::new()).await;
    assert!(matches!(refused, Err(AgentError::Usage(ref m)) if m.contains("--adopt")), "{refused:?}");
    assert_eq!(b.data.load_account().unwrap(), None);
    let args = LoginArgs { adopt: true, ..login_args(&server, &b.folder, "laptop") };
    cli::login(&b.data, &args, PASSWORD, &mut Vec::new()).await.unwrap();
    b.sync().await;
    // The same content is adopted, not copied; the new file goes up.
    assert_eq!(b.files(), ["mine.txt", "notes.txt"]);
    a.sync().await;
    assert_eq!(a.read("mine.txt").as_deref(), Some("only here"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edits_go_both_ways() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    a.write("doc.txt", "v1");
    a.sync().await;
    b.sync().await;
    a.write("doc.txt", "v2 from a, longer");
    a.sync().await;
    b.sync().await;
    assert_eq!(b.read("doc.txt").as_deref(), Some("v2 from a, longer"));
    b.write("doc.txt", "v3 from b");
    b.sync().await;
    a.sync().await;
    assert_eq!(a.read("doc.txt").as_deref(), Some("v3 from b"));
    assert_eq!(a.files(), ["doc.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_on_both_sides_keeps_both() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    a.write("doc.txt", "base");
    a.sync().await;
    b.sync().await;
    a.write("doc.txt", "edited on a");
    b.write("doc.txt", "edited on b!");
    a.sync().await;
    let (_, summary) = b.try_sync(false).await.unwrap();
    assert_eq!(summary.conflict_copies.len(), 1, "{summary:?}");
    let copy = &summary.conflict_copies[0];
    assert!(copy.starts_with("doc (conflict, laptop, ") && copy.ends_with(").txt"), "{copy}");
    assert_eq!(b.read("doc.txt").as_deref(), Some("edited on a"));
    assert_eq!(b.read(copy).as_deref(), Some("edited on b!"));
    b.sync().await;
    a.sync().await;
    assert_eq!(a.read(copy).as_deref(), Some("edited on b!"));
    assert_eq!(a.files(), b.files());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_is_a_move() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    a.write("a.txt", "content");
    a.write("Archive/old.txt", "old");
    a.sync().await;
    b.sync().await;
    std::fs::rename(a.path("a.txt"), a.path("Archive/a.txt")).unwrap();
    std::fs::rename(a.path("Archive"), a.path("Archive 2026")).unwrap();
    let (_, pushed) = a.try_sync(false).await.unwrap();
    assert_eq!(pushed.uploaded, 0, "a move sends no content: {pushed:?}");
    let (_, pulled) = b.try_sync(false).await.unwrap();
    assert_eq!(pulled.downloaded, 0, "{pulled:?}");
    assert!(pulled.moved_locally >= 1, "{pulled:?}");
    assert_eq!(b.files(), ["Archive 2026/a.txt", "Archive 2026/old.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mass_delete_waits_for_approval_both_ways() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    for i in 0..12 {
        a.write(&format!("f{i:02}.txt"), &format!("file {i}"));
    }
    a.sync().await;
    b.sync().await;
    assert_eq!(b.files().len(), 12);
    for i in 0..12 {
        std::fs::remove_file(a.path(&format!("f{i:02}.txt"))).unwrap();
    }
    let (exit, summary) = a.try_sync(false).await.unwrap();
    assert_eq!(exit, Exit::NeedsApproval);
    assert_eq!(exit.code(), 2);
    assert_eq!(summary.pending_deletes.len(), 12);
    assert_eq!(b.sync().await, Exit::Done);
    assert_eq!(b.files().len(), 12, "nothing was deleted on the server yet");

    let (exit, summary) = a.try_sync(true).await.unwrap();
    assert_eq!(exit, Exit::Done);
    assert_eq!(summary.removed_remotely, 12);
    let (exit, summary) = b.try_sync(false).await.unwrap();
    assert_eq!(exit, Exit::NeedsApproval);
    assert_eq!(summary.pending_local_deletes.len(), 12);
    assert_eq!(b.files().len(), 12, "the guard holds local deletes too");
    let (exit, summary) = b.try_sync(true).await.unwrap();
    assert_eq!(exit, Exit::Done);
    assert_eq!(summary.removed_locally, 12);
    assert!(b.files().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_releases_only_the_deletes_that_were_shown() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    for i in 0..12 {
        a.write(&format!("f{i:02}.txt"), "x");
        a.write(&format!("g{i:02}.txt"), "y");
    }
    a.sync().await;
    for i in 0..12 {
        std::fs::remove_file(a.path(&format!("f{i:02}.txt"))).unwrap();
    }
    let (exit, shown) = a.try_sync(false).await.unwrap();
    assert_eq!((exit, shown.pending_deletes.len()), (Exit::NeedsApproval, 12));
    // Twelve more vanish before the user approves the twelve they saw.
    for i in 0..12 {
        std::fs::remove_file(a.path(&format!("g{i:02}.txt"))).unwrap();
    }
    let (exit, summary) = a.try_sync(true).await.unwrap();
    assert_eq!(exit, Exit::NeedsApproval, "the new ones still wait");
    assert_eq!(summary.removed_remotely, 12);
    assert_eq!(summary.pending_deletes.len(), 12);
    assert!(summary.pending_deletes.iter().all(|p| p.starts_with('g')), "{summary:?}");
    let (exit, summary) = a.try_sync(true).await.unwrap();
    assert_eq!((exit, summary.removed_remotely), (Exit::Done, 12));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_one_agent_runs_at_a_time() {
    use craft_cloud_agent::lock::{self, LockError};
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.write("a.txt", "x");
    // Another agent holds the data folder: a second sync is refused and changes nothing.
    let held = lock::acquire(&a.data.path().join(lock::DATA_LOCK)).unwrap();
    let refused = a.try_sync(false).await;
    assert!(matches!(refused, Err(AgentError::Lock(LockError::Busy(_)))), "{refused:?}");
    let refused = refused.unwrap_err();
    assert_eq!(refused.exit(), Exit::NeedsYou);
    assert!(refused.to_string().contains("another craft-cloud-agent is running"));
    assert!(matches!(cli::logout(&a.data, false, &mut Vec::new()).await, Err(AgentError::Lock(_))));
    drop(held);
    // Another agent (with another data folder) syncs the same folder.
    let held = lock::acquire(&a.path(".craft-cloud").join(lock::FOLDER_LOCK)).unwrap();
    assert!(matches!(a.try_sync(false).await, Err(AgentError::Lock(LockError::Busy(_)))));
    drop(held);
    let (_, summary) = a.try_sync(false).await.unwrap();
    assert_eq!(summary.uploaded, 1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_file_stays_private_when_a_remote_edit_lands() {
    use std::os::unix::fs::PermissionsExt;
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    a.write("secret.key", "k1");
    a.sync().await;
    b.sync().await;
    std::fs::set_permissions(b.path("secret.key"), std::fs::Permissions::from_mode(0o600)).unwrap();
    a.write("secret.key", "k2 longer");
    a.sync().await;
    b.sync().await;
    assert_eq!(b.read("secret.key").as_deref(), Some("k2 longer"));
    assert_eq!(std::fs::metadata(b.path("secret.key")).unwrap().permissions().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tracked_folder_replaced_by_a_link_is_never_deleted() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    for i in 0..20 {
        a.write(&format!("f{i:02}.txt"), &format!("file {i}"));
    }
    for i in 0..3 {
        a.write(&format!("Footage/c{i}.mov"), &format!("clip {i}"));
    }
    a.sync().await;
    b.sync().await;
    // Footage moves to another drive and a link is left behind.
    let drive = tempfile::tempdir().unwrap();
    let moved = drive.path().join("Footage");
    std::fs::rename(a.path("Footage"), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, a.path("Footage")).unwrap();
    let (exit, summary) = a.try_sync(false).await.unwrap();
    assert_eq!(exit, Exit::Done);
    assert_eq!(summary.removed_remotely, 0, "{summary:?}");
    assert!(summary.pending_deletes.is_empty(), "{summary:?}");
    assert!(summary.skipped.iter().any(|(p, _)| p == "Footage"), "{summary:?}");
    let (_, summary) = b.try_sync(false).await.unwrap();
    assert_eq!(summary.removed_locally, 0);
    assert_eq!(b.files().len(), 23);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_edit_inside_a_folder_replaced_by_a_link_waits() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    for i in 0..20 {
        a.write(&format!("f{i:02}.txt"), &format!("file {i}"));
    }
    for i in 0..3 {
        a.write(&format!("Footage/c{i}.mov"), &format!("clip {i}"));
    }
    a.sync().await;
    b.sync().await;
    let drive = tempfile::tempdir().unwrap();
    let moved = drive.path().join("Footage");
    std::fs::rename(a.path("Footage"), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, a.path("Footage")).unwrap();
    // The laptop edits a clip and adds one inside the folder.
    b.write("Footage/c0.mov", "clip 0, edited");
    b.write("Footage/new.mov", "a new clip");
    b.sync().await;
    for _ in 0..2 {
        let (_, summary) = a.try_sync(false).await.unwrap();
        assert_eq!(summary.removed_remotely, 0, "{summary:?}");
        assert!(summary.pending_deletes.is_empty() && summary.conflict_copies.is_empty(), "{summary:?}");
    }
    // The link is untouched, and nothing was written through it.
    assert!(std::fs::symlink_metadata(a.path("Footage")).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(moved.join("c0.mov")).unwrap(), "clip 0");
    assert!(!moved.join("new.mov").exists());
    assert_eq!(std::fs::read_dir(a.path("")).unwrap().count(), 22, "no conflict copy of the link");
    let (_, summary) = b.try_sync(false).await.unwrap();
    assert_eq!(summary.removed_locally, 0);
    assert_eq!(b.files().len(), 24);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tracked_file_renamed_to_another_normalisation_is_never_deleted() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    a.write("caf\u{e9}.txt", "precious");
    a.write("other.txt", "other");
    a.sync().await;
    b.sync().await;
    std::fs::rename(a.path("caf\u{e9}.txt"), a.path("cafe\u{301}.txt")).unwrap();
    let (_, summary) = a.try_sync(false).await.unwrap();
    assert_eq!(summary.removed_remotely, 0, "{summary:?}");
    // Said as a name to fix, not as a link.
    assert!(summary.skipped.iter().any(|(p, why)| p == "cafe\u{301}.txt" && why == craft_cloud_sync::NOT_NFC_REASON), "{summary:?}");
    b.sync().await;
    assert_eq!(b.read("caf\u{e9}.txt").as_deref(), Some("precious"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wiped_folder_stops_the_pass() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    a.write("keep.txt", "precious");
    a.write("Folder/also.txt", "precious too");
    a.sync().await;
    b.sync().await;

    // The folder is wiped (or a different, empty one sits in its place): no marker.
    std::fs::remove_dir_all(&a.folder).unwrap();
    std::fs::create_dir(&a.folder).unwrap();
    let stopped = a.try_sync(false).await;
    assert!(matches!(stopped, Err(AgentError::Root(RootError::MarkerMissing(_)))), "{stopped:?}");
    let stopped = stopped.unwrap_err();
    assert_eq!(stopped.exit(), Exit::NeedsYou);
    assert_eq!(stopped.exit().code(), 3);
    let message = stopped.to_string();
    assert!(message.contains("nothing was deleted"), "{message}");
    // The folder is gone altogether (a drive that isn't mounted).
    std::fs::remove_dir(&a.folder).unwrap();
    assert!(matches!(a.try_sync(false).await, Err(AgentError::Root(RootError::FolderMissing(_)))));

    // Nothing was deleted on the server: the other machine keeps everything.
    assert_eq!(b.sync().await, Exit::Done);
    assert_eq!(b.files(), ["Folder/also.txt", "keep.txt"]);
    let mut status = Vec::new();
    cli::status(&a.data, &mut status).unwrap();
    assert!(String::from_utf8(status).unwrap().contains("folder problem"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_restore_keeps_newer_local_files() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let b = Agent::signed_in(&server, "laptop").await;
    a.write("doc.txt", "v1");
    a.write("other.txt", "unchanged");
    a.sync().await;
    b.sync().await;
    // A edits offline while the server is restored from a backup.
    a.write("doc.txt", "v2, newer than the backup");
    let user = server.meta.user_by_name("ana").await.unwrap().unwrap();
    server.meta.reset_epoch(user.space).await.unwrap();

    a.sync().await;
    assert_eq!(a.read("doc.txt").as_deref(), Some("v2, newer than the backup"), "a reconcile never replaces a local file");
    assert_eq!(a.files(), ["doc.txt", "other.txt"]);
    b.sync().await;
    assert_eq!(b.read("doc.txt").as_deref(), Some("v2, newer than the backup"));
    assert_eq!(b.read("other.txt").as_deref(), Some("unchanged"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logout_revokes_the_device() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.write("a.txt", "stays");
    a.sync().await;
    let account = a.data.load_account().unwrap().unwrap();
    let saved = std::fs::read(a.data.path().join("account.json")).unwrap();

    let mut out = Vec::new();
    cli::logout(&a.data, false, &mut out).await.unwrap();
    assert_eq!(a.data.load_account().unwrap(), None);
    assert_eq!(a.read("a.txt").as_deref(), Some("stays"), "local files stay");
    let user = server.meta.user_by_name("ana").await.unwrap().unwrap();
    let devices = server.meta.list_devices(user.id).await.unwrap();
    assert!(devices.iter().any(|d| d.id == account.device && d.revoked));
    assert!(matches!(a.try_sync(false).await, Err(AgentError::NotSignedIn)));

    // With the old sign-in put back, the server refuses it, and says what to do.
    std::fs::write(a.data.path().join("account.json"), saved).unwrap();
    let refused = a.try_sync(false).await;
    assert!(matches!(refused, Err(AgentError::Revoked)), "{refused:?}");
    assert!(refused.unwrap_err().to_string().contains("login"));
    // Signing out a device the server already revoked just forgets it.
    cli::logout(&a.data, false, &mut Vec::new()).await.unwrap();
    assert_eq!(a.data.load_account().unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logout_with_the_server_gone_says_how_to_forget_the_account() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let mut account = a.data.load_account().unwrap().unwrap();
    // The server moved away for good (nothing listens on port 9).
    account.server = ServerUrl::parse("http://127.0.0.1:9").unwrap();
    a.data.save_account(&account).unwrap();
    let refused = cli::logout(&a.data, false, &mut Vec::new()).await;
    assert!(matches!(refused, Err(AgentError::LogoutOffline(_))), "{refused:?}");
    assert!(refused.unwrap_err().to_string().contains("logout --local"));
    assert!(a.data.load_account().unwrap().is_some(), "nothing was changed");
    let mut out = Vec::new();
    cli::logout(&a.data, true, &mut out).await.unwrap();
    assert_eq!(a.data.load_account().unwrap(), None);
    let printed = String::from_utf8(out).unwrap();
    assert!(printed.contains("stays valid") && printed.contains("until an admin revokes it"), "{printed}");
    // The server still has the device, not revoked.
    let user = server.meta.user_by_name("ana").await.unwrap().unwrap();
    assert!(server.meta.list_devices(user.id).await.unwrap().iter().any(|d| d.id == account.device && !d.revoked));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_revoked_by_the_admin_stops_syncing() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.sync().await;
    let account = a.data.load_account().unwrap().unwrap();
    let user = server.meta.user_by_name("ana").await.unwrap().unwrap();
    server.meta.revoke_device(user.id, account.device).await.unwrap();
    let revoked = a.try_sync(false).await.unwrap_err();
    assert!(matches!(revoked, AgentError::Revoked), "{revoked:?}");
    assert_eq!(revoked.exit(), Exit::NeedsYou);
    assert_eq!(revoked.exit().code(), 3);
    let mut out = Vec::new();
    cli::status(&a.data, &mut out).unwrap();
    let status = String::from_utf8(out).unwrap();
    assert!(status.contains("failed") && status.contains("revoked"), "{status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_refuses_what_it_should() {
    let server = server().await;
    let a = Agent::new();
    let wrong = cli::login(&a.data, &login_args(&server, &a.folder, "pc"), "wrong password", &mut Vec::new()).await;
    assert!(matches!(wrong, Err(AgentError::WrongPassword)), "{wrong:?}");
    let insecure = LoginArgs { url: "http://cloud.example.com".into(), ..login_args(&server, &a.folder, "pc") };
    assert!(matches!(cli::login(&a.data, &insecure, PASSWORD, &mut Vec::new()).await, Err(AgentError::Usage(_))));
    assert!(!a.folder.exists(), "a refused login sets nothing up");

    let mut out = Vec::new();
    // `localhost` is pinned to this machine's loopback addresses.
    let local = LoginArgs { url: server.url.replace("127.0.0.1", "localhost"), ..login_args(&server, &a.folder, "pc") };
    let account = cli::login(&a.data, &local, PASSWORD, &mut out).await.unwrap();
    let printed = String::from_utf8(out).unwrap();
    assert!(printed.contains("signed in as ana"), "{printed}");
    assert_eq!(root::read(&a.folder).unwrap().map(|m| m.space), Some(account.space));
    let again = cli::login(&a.data, &login_args(&server, &a.folder, "pc"), PASSWORD, &mut Vec::new()).await;
    assert!(matches!(again, Err(AgentError::Usage(ref m)) if m.contains("logout")), "{again:?}");
}

/// The data folder holds the device token: it must never sit inside the synced folder, nor the reverse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_token_is_never_synced() {
    let server = server().await;
    let a = Agent::new();
    // The synced folder would hold the data folder.
    let parent = a.data.path().parent().unwrap().to_owned();
    let refused = cli::login(&a.data, &login_args(&server, &parent, "pc"), PASSWORD, &mut Vec::new()).await.unwrap_err();
    assert!(matches!(refused, AgentError::DataInFolder { .. }), "{refused:?}");
    assert_eq!(refused.exit(), Exit::NeedsYou);
    assert!(refused.to_string().contains("token"), "{refused}");
    // The synced folder inside the data folder, not made yet, through a `..`.
    let inside = a.data.path().join("x").join("..").join("Craft Cloud");
    let refused = cli::login(&a.data, &login_args(&server, &inside, "pc"), PASSWORD, &mut Vec::new()).await.unwrap_err();
    assert!(matches!(refused, AgentError::DataInFolder { .. }), "{refused:?}");
    assert_eq!(a.data.load_account().unwrap(), None);
    // A `..` after a folder that doesn't exist yet, stepping back into the data folder.
    let data_name = a.data.path().file_name().unwrap();
    let sneaky = parent.join("not-made-yet").join("..").join(data_name).join("Craft Cloud");
    let refused = cli::login(&a.data, &login_args(&server, &sneaky, "pc"), PASSWORD, &mut Vec::new()).await.unwrap_err();
    assert!(matches!(refused, AgentError::DataInFolder { .. }), "{refused:?}");
    assert_eq!(a.data.load_account().unwrap(), None);

    // An account edited by hand into such a layout stops the pass before anything is read.
    let b = Agent::signed_in(&server, "desktop").await;
    b.sync().await;
    let mut account = b.data.load_account().unwrap().unwrap();
    account.folder = b.data.path().parent().unwrap().to_owned();
    b.data.save_account(&account).unwrap();
    let stopped = b.try_sync(false).await.unwrap_err();
    assert!(matches!(stopped, AgentError::DataInFolder { .. }), "{stopped:?}");
    assert_eq!(stopped.exit(), Exit::NeedsYou);
}

/// A loopback server that answers every request with `/api/info` of a server speaking only protocol 99.
fn newer_server() -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = r#"{"protocol_min":99,"protocol_max":99,"chunker_params":["fastcdc-v1"],"max_chunk_bytes":4194304,
                "max_manifest_bytes":1048576,"max_batch_ops":1000,"max_batch_json_bytes":1048576,"max_page_nodes":1000,"max_missing_ids":10000}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    url
}

/// The server is asked what it speaks before every pass, not only at login: one updated since then stops the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_pass_checks_what_the_server_speaks() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.write("notes.txt", "kept");
    let mut account = a.data.load_account().unwrap().unwrap();
    account.server = ServerUrl::parse(&newer_server()).unwrap();
    a.data.save_account(&account).unwrap();
    let stopped = a.try_sync(false).await.unwrap_err();
    assert!(matches!(stopped, AgentError::Server(TransportError::Protocol(_))), "{stopped:?}");
    assert_eq!(stopped.exit(), Exit::NeedsYou);
    assert!(stopped.to_string().contains("protocol 99-99"), "{stopped}");
    let saved = a.data.load_state().unwrap();
    assert!(
        matches!(saved.last_pass.map(|l| l.outcome), Some(craft_cloud_agent::account::Outcome::Failed(ref m)) if m.contains("protocol")),
        "the stop is saved for `status`"
    );
    assert_eq!(a.read("notes.txt").as_deref(), Some("kept"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_shows_the_last_pass_but_never_the_token() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let mut out = Vec::new();
    cli::status(&a.data, &mut out).unwrap();
    assert!(String::from_utf8(out).unwrap().contains("never synced"));
    a.write("a.txt", "x");
    a.sync().await;
    let mut out = Vec::new();
    cli::status(&a.data, &mut out).unwrap();
    let status = String::from_utf8(out).unwrap();
    assert!(status.contains("signed in as ana") && status.contains("desktop") && status.contains("1 uploaded"), "{status}");
    let token = serde_json::from_slice::<serde_json::Value>(&std::fs::read(a.data.path().join("account.json")).unwrap()).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!status.contains(&token));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_transport_maps_server_answers() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let account = a.data.load_account().unwrap().unwrap();
    let url = ServerUrl::parse(&server.url).unwrap();
    let transport = HttpTransport::new(url.clone(), &account.token).unwrap();
    assert_eq!(transport.space().await.unwrap().space, account.space);
    assert_eq!(transport.get_chunk(&ChunkId::from_bytes([7; 32])).await, Err(TransportError::NotFound));
    let stranger = HttpTransport::new(url, &DeviceToken::new("not-a-token".into())).unwrap();
    assert_eq!(stranger.space().await.unwrap_err(), TransportError::Unauthorized);
    // Nothing listens on this port: try again later.
    let nowhere = HttpTransport::new(ServerUrl::parse("http://127.0.0.1:9").unwrap(), &account.token).unwrap();
    assert!(matches!(nowhere.space().await, Err(TransportError::Unavailable(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_pass_keeps_the_deletes_the_user_was_shown() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    for i in 0..12 {
        a.write(&format!("f{i:02}.txt"), "x");
    }
    a.sync().await;
    for i in 0..12 {
        std::fs::remove_file(a.path(&format!("f{i:02}.txt"))).unwrap();
    }
    let (exit, shown) = a.try_sync(false).await.unwrap();
    assert_eq!((exit, shown.pending_deletes.len()), (Exit::NeedsApproval, 12));

    // The server is unreachable for the next pass, which was meant to approve.
    let good = a.data.load_account().unwrap().unwrap();
    let bad = craft_cloud_agent::account::Account { server: ServerUrl::parse("http://127.0.0.1:9").unwrap(), ..good.clone() };
    a.data.save_account(&bad).unwrap();
    let failed = a.try_sync(true).await.unwrap_err();
    assert!(matches!(failed, AgentError::Offline(_)), "{failed:?}");
    assert_eq!(failed.exit(), Exit::Failed);
    let mut out = Vec::new();
    cli::status(&a.data, &mut out).unwrap();
    let status = String::from_utf8(out).unwrap();
    assert!(status.contains("failed") && status.contains("waiting for approval") && status.contains("delete on the server: f00.txt"), "{status}");
    assert!(!status.contains("in sync: nothing to do"), "{status}");

    // Back online: approving releases exactly the deletes that were shown.
    a.data.save_account(&good).unwrap();
    let (exit, summary) = a.try_sync(true).await.unwrap();
    assert_eq!((exit, summary.removed_remotely), (Exit::Done, 12), "{summary:?}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_makes_the_agent_folder_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let mode = |rel: &str| std::fs::metadata(a.path(rel)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(".craft-cloud"), 0o700);
    // A sync leaves it so.
    a.sync().await;
    assert_eq!(mode(".craft-cloud"), 0o700);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_state_of_another_account_stops_the_agent() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.sync().await;
    let mut saved = a.data.load_state().unwrap();
    saved.state.space = Some(craft_cloud_proto::SpaceId::generate());
    a.data.save_state(&saved).unwrap();
    let stopped = a.try_sync(false).await.unwrap_err();
    assert!(matches!(stopped, AgentError::OtherAccountState(_)), "{stopped:?}");
    assert_eq!(stopped.exit(), Exit::NeedsYou);
    assert!(stopped.to_string().contains("belongs to another account"), "{stopped}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_notices_an_existing_account_before_asking_for_the_password() {
    use clap::Parser;
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    let data = a.data.path().to_str().unwrap().to_owned();
    let cli = cli::Cli::try_parse_from(["craft-cloud-agent", "login", &server.url, "--user", "ana", "--data-dir", &data]).unwrap();
    // Nothing on standard input: a missing password would be the error if it were read first.
    let refused = cli::run(cli, &mut "".as_bytes(), &mut Vec::new()).await.unwrap_err();
    assert!(matches!(refused, AgentError::Usage(ref m) if m.contains("already signed in")), "{refused:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adopting_a_folder_turns_a_differing_file_into_a_conflict_copy() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.write("notes.txt", "server copy");
    a.sync().await;

    let b = Agent::new();
    std::fs::create_dir_all(&b.folder).unwrap();
    std::fs::write(b.folder.join("notes.txt"), "my own, different text").unwrap();
    let args = LoginArgs { adopt: true, ..login_args(&server, &b.folder, "laptop") };
    cli::login(&b.data, &args, PASSWORD, &mut Vec::new()).await.unwrap();
    let (_, summary) = b.try_sync(false).await.unwrap();
    assert_eq!(summary.conflict_copies.len(), 1, "{summary:?}");
    let copy = &summary.conflict_copies[0];
    assert_eq!(b.read("notes.txt").as_deref(), Some("server copy"));
    assert_eq!(b.read(copy).as_deref(), Some("my own, different text"), "both versions are kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_login_setup_revokes_the_new_device() {
    let server = server().await;
    let a = Agent::new();
    std::fs::create_dir_all(&a.folder).unwrap();
    // The folder is already set up for some other space.
    let marker = root::RootMarker { space: craft_cloud_proto::SpaceId::generate(), root: craft_cloud_proto::NodeId::generate() };
    root::write(&a.folder, &marker).unwrap();
    let args = LoginArgs { adopt: true, ..login_args(&server, &a.folder, "pc") };
    let refused = cli::login(&a.data, &args, PASSWORD, &mut Vec::new()).await.unwrap_err();
    assert!(matches!(refused, AgentError::Root(RootError::SetUpForOtherSpace(_))), "{refused:?}");
    assert_eq!(refused.exit(), Exit::NeedsYou);
    let message = refused.to_string();
    assert!(message.contains(".craft-cloud") && message.contains("another folder"), "{message}");
    assert_eq!(a.data.load_account().unwrap(), None);
    let user = server.meta.user_by_name("ana").await.unwrap().unwrap();
    let devices = server.meta.list_devices(user.id).await.unwrap();
    assert_eq!(devices.len(), 1, "the device was created, then revoked");
    assert!(devices.iter().all(|d| d.revoked), "{devices:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_lists_the_waiting_deletes() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    for i in 0..12 {
        a.write(&format!("f{i:02}.txt"), "x");
    }
    a.sync().await;
    for i in 0..12 {
        std::fs::remove_file(a.path(&format!("f{i:02}.txt"))).unwrap();
    }
    assert_eq!(a.sync().await, Exit::NeedsApproval);
    let mut out = Vec::new();
    cli::status(&a.data, &mut out).unwrap();
    let status = String::from_utf8(out).unwrap();
    assert!(status.contains("deletions wait for approval") && status.contains("delete on the server: f07.txt"), "{status}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_lists_the_skipped_files() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.write("Footage/c0.mov", "clip");
    a.write("other.txt", "x");
    a.sync().await;
    let drive = tempfile::tempdir().unwrap();
    std::fs::rename(a.path("Footage"), drive.path().join("Footage")).unwrap();
    std::os::unix::fs::symlink(drive.path().join("Footage"), a.path("Footage")).unwrap();
    a.sync().await;
    let mut out = Vec::new();
    cli::status(&a.data, &mut out).unwrap();
    let status = String::from_utf8(out).unwrap();
    assert!(status.contains("skipped:") && status.contains("Footage"), "{status}");
}

/// A record of an interrupted download that can't be read doesn't fail every pass: it is set aside as `.bad`, and
/// the pass report and `status` say what to check and what to remove, until it is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_landing_record_is_set_aside_and_listed() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.write("notes.txt", "x");
    assert_eq!(a.sync().await, Exit::Done);
    std::fs::create_dir_all(a.path(".craft-cloud/tmp")).unwrap();
    std::fs::write(a.path(".craft-cloud/tmp/r.landing"), br#"{"target":"notes.txt""#).unwrap();
    let (exit, summary) = a.try_sync(false).await.unwrap();
    assert_eq!(exit, Exit::Done);
    let bad = ".craft-cloud/tmp/r.landing.bad";
    assert!(summary.skipped.iter().any(|(p, why)| p == bad && why.contains(&format!("then remove {bad}"))), "{summary:?}");
    let mut out = Vec::new();
    cli::status(&a.data, &mut out).unwrap();
    let status = String::from_utf8(out).unwrap();
    assert!(status.contains("needs you:") && status.contains(bad), "{status}");
    std::fs::remove_file(a.path(bad)).unwrap();
    let (_, summary) = a.try_sync(false).await.unwrap();
    assert!(summary.skipped.is_empty(), "{summary:?}");
    let mut out = Vec::new();
    cli::status(&a.data, &mut out).unwrap();
    assert!(!String::from_utf8(out).unwrap().contains("needs you"));
}

/// A file an interrupted download took out of the folder and couldn't put back (`.rescued`) is listed by `status`
/// until the user moves it away; passes never remove it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_lists_a_rescued_file_until_it_is_gone() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    assert_eq!(a.sync().await, Exit::Done);
    std::fs::create_dir_all(a.path(".craft-cloud/tmp")).unwrap();
    let rescued = ".craft-cloud/tmp/ab12.rescued";
    std::fs::write(a.path(rescued), b"a save").unwrap();
    assert_eq!(a.sync().await, Exit::Done);
    let status = |a: &Agent| {
        let mut out = Vec::new();
        cli::status(&a.data, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    };
    let shown = status(&a);
    assert!(shown.contains("needs you:") && shown.contains(rescued) && shown.contains("move it back yourself"), "{shown}");
    assert_eq!(std::fs::read(a.path(rescued)).unwrap(), b"a save");
    std::fs::remove_file(a.path(rescued)).unwrap();
    assert!(!status(&a).contains("needs you"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_state_file_stops_the_agent() {
    let server = server().await;
    let a = Agent::signed_in(&server, "desktop").await;
    a.sync().await;
    std::fs::write(a.data.path().join("state.json"), "{not json").unwrap();
    let stopped = a.try_sync(false).await.unwrap_err();
    assert!(matches!(stopped, AgentError::Account(_)), "{stopped:?}");
    assert_eq!(stopped.exit(), Exit::NeedsYou);
    assert!(stopped.to_string().contains("move it away"), "{stopped}");
}

/// A file that can't be replaced when the server's edit arrives (on Windows: an app holds it open; here: its folder
/// may not be written): the pass skips it as in use and keeps the download, a pass while it is still blocked leaves
/// the kept file alone, and a later pass lands the kept file. The server's store counts the chunks it reads, which
/// shows the kept file landed without a download.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_that_cant_land_is_kept_and_lands_later() {
    use std::os::unix::fs::PermissionsExt;

    /// Gives the folder its mode back however the test ends, so the temporary folder can be removed.
    struct Writable(PathBuf);
    impl Drop for Writable {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    let server = server().await;
    let a = Agent::signed_in(&server, "laptop").await;
    let b = Agent::signed_in(&server, "desktop").await;
    a.write("docs/notes.txt", "v1");
    assert_eq!(a.sync().await, Exit::Done);
    assert_eq!(b.sync().await, Exit::Done);
    a.write("docs/notes.txt", "v2, from the laptop");
    assert_eq!(a.sync().await, Exit::Done);

    let folder = b.path("docs");
    let set_mode = |mode: u32| std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(mode)).unwrap();
    let _writable = Writable(folder.clone());
    set_mode(0o555);
    if std::fs::write(folder.join("probe"), b"").is_ok() {
        eprintln!("skipped: folder permissions don't hold for this user (root), so nothing here can't be replaced");
        return;
    }
    let kept = || {
        std::fs::read_dir(b.path(".craft-cloud/tmp")).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "kept")).count()
    };
    let chunk_reads = || server.chunk_reads.load(Ordering::Relaxed);
    let before = chunk_reads();
    let (exit, summary) = b.try_sync(false).await.unwrap();
    assert_eq!(exit, Exit::Done);
    assert!(summary.skipped.iter().any(|(p, why)| p == "docs/notes.txt" && why.contains("no permission") && why.contains("kept")), "{summary:?}");
    assert_eq!(b.read("docs/notes.txt").as_deref(), Some("v1"));
    assert_eq!(kept(), 1);
    let downloaded = chunk_reads();
    assert!(downloaded > before, "the first pass downloads");

    // Still blocked: skipped again, with the kept file left as it is.
    let (_, summary) = b.try_sync(false).await.unwrap();
    assert!(summary.skipped.iter().any(|(p, why)| p == "docs/notes.txt" && why.contains("kept")), "{summary:?}");
    assert_eq!(kept(), 1);

    set_mode(0o755);
    let (_, summary) = b.try_sync(false).await.unwrap();
    assert_eq!(summary.downloaded, 1, "{summary:?}");
    assert_eq!(b.read("docs/notes.txt").as_deref(), Some("v2, from the laptop"));
    assert_eq!(kept(), 0);
    assert_eq!(chunk_reads(), downloaded, "the kept file landed without a download");
    assert_eq!(b.files(), a.files());
}

// ---- HTTPS (review L6) ----

/// A self-signed certificate for `localhost` and `127.0.0.1` and its key (PKCS #8), made for this test only
/// (`tests/tls/`, see ATTRIBUTION.md). Valid until 2126.
const TEST_CERT: &[u8] = include_bytes!("tls/localhost.cert.der");
const TEST_KEY: &[u8] = include_bytes!("tls/localhost.key.der");

/// A TLS listener on a loopback port with the test certificate, answering every request with `answer` (a whole
/// HTTP/1.1 answer). Returns the port.
async fn tls_listener(answer: Vec<u8>) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::rustls;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(TEST_CERT.to_vec())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(TEST_KEY.to_vec())))
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, answer) = (acceptor.clone(), answer.clone());
            tokio::spawn(async move {
                // A client that refuses the certificate ends the handshake: nothing to answer.
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = tls.write_all(&answer).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_refuses_an_untrusted_certificate_and_works_with_a_trusted_one() {
    use craft_cloud_agent::http::{Reqwest, info_over};
    let info = craft_cloud_proto::api::InfoResponse {
        protocol_min: 1,
        protocol_max: 1,
        chunker_params: vec!["fastcdc-v1".into()],
        max_chunk_bytes: 4 * 1024 * 1024,
        max_manifest_bytes: 1024,
        max_batch_ops: 1000,
        max_batch_json_bytes: 1024 * 1024,
        max_page_nodes: 5000,
        max_missing_ids: 10_000,
    };
    let body = serde_json::to_vec(&info).unwrap();
    let mut answer =
        format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).into_bytes();
    answer.extend_from_slice(&body);
    let port = tls_listener(answer).await;
    let url = ServerUrl::parse(&format!("https://localhost:{port}")).unwrap();

    // The agent's own connection checks the certificate against this computer's trust store, which doesn't have it.
    let refused = info_over(&Reqwest::new(&url).unwrap(), &url).await;
    assert!(matches!(refused, Err(TransportError::Unavailable(_))), "a self-signed certificate was accepted: {refused:?}");

    // Trusting the test certificate, the same request to the same listener goes through TLS and comes back: the
    // refusal above was the certificate's.
    let trusting = Reqwest::trusting_only(&url, &[TEST_CERT]).unwrap();
    assert_eq!(info_over(&trusting, &url).await.unwrap(), info);
}

/// A join to a server whose certificate this computer doesn't trust says so (and how to fix it), and one to a server
/// that can't be reached names the address tried; neither creates anything here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_join_that_cant_reach_the_server_says_why_and_creates_nothing() {
    use craft_cloud_agent::cli::JoinArgs;
    use craft_cloud_agent::local_api::LocalError;
    use craft_cloud_proto::local::codes;
    let port = tls_listener(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec()).await;
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path().join("data"));
    let folder = dir.path().join("Craft Cloud");
    let args = |join: String| JoinArgs { join, folder: Some(folder.clone()), device_name: Some("pc".into()), adopt: false, autostart: false };

    let untrusted = cli::join(&data, &args(format!("https://localhost:{port}/K7M4-Q9TX")), None, &mut Vec::new()).await.unwrap_err();
    assert!(matches!(untrusted, AgentError::UntrustedCertificate { ref host } if host == "localhost"), "{untrusted:?}");
    let message = untrusted.to_string();
    assert!(message.contains("This computer doesn't trust localhost's certificate") && message.contains("root certificate"), "{message}");
    assert_eq!(LocalError::from(untrusted).code, codes::UNTRUSTED_CERTIFICATE);

    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let unreachable = cli::join(&data, &args(format!("http://127.0.0.1:{closed}/K7M4-Q9TX")), None, &mut Vec::new()).await.unwrap_err();
    assert!(unreachable.to_string().contains(&format!("127.0.0.1:{closed}")), "{unreachable}");
    assert_eq!(LocalError::from(unreachable).code, codes::OFFLINE);

    assert!(data.load_account().unwrap().is_none());
    assert!(!folder.exists(), "nothing made here");
}
