//! Two agents with their own folders and data folders sync through a real server on a loopback port, driven
//! through the command line's own functions (`cli::login`, `cli::sync`, …), as a user would run them.

use std::path::{Path, PathBuf};

use craft_cloud_agent::account::DataDir;
use craft_cloud_agent::cli::{self, AgentError, Exit, LoginArgs};
use craft_cloud_agent::http::{DeviceToken, HttpTransport, ServerUrl};
use craft_cloud_agent::root::{self, RootError};
use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::ChunkId;
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_sync::{Transport, TransportError};

const PASSWORD: &str = "correct horse battery";

struct Server {
    url: String,
    meta: Meta,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dirs: Vec<tempfile::TempDir>,
}

async fn server() -> Server {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let state = AppState::new(meta.clone(), store, vec![]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(craft_cloud_server::serve(state, listener, async move {
        let _ = rx.await;
    }));
    meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
    Server { url, meta, _shutdown: tx, _dirs: vec![db, blobs] }
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
    assert!(summary.skipped.iter().any(|(p, _)| p == "cafe\u{301}.txt"), "{summary:?}");
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
    assert!(matches!(refused, AgentError::Root(RootError::OtherSpace(_))), "{refused:?}");
    assert_eq!(refused.exit(), Exit::NeedsYou);
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
