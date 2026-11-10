//! The agent's HTTP transport, bound to a team space, against a real server on a loopback port: a viewer's engine edits a
//! file, the server's copy stays as it was, and the server's `403 read_only` reaches the engine as `TransportError::ReadOnly`
//! on the wire. Raising the role resumes the uploads.

use craft_cloud_agent::cli::{self, LoginArgs};
use craft_cloud_agent::http::{HttpTransport, ServerUrl};
use craft_cloud_meta::{Meta, Role, SpaceActor, UserRecord};
use craft_cloud_proto::api::SpaceRole;
use craft_cloud_proto::{ChunkId, ConflictStamp, DeviceId, Op, SpaceId, SpaceName};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_sync::{Config, Engine, Fs, MemFs, READ_ONLY_EDITED, READ_ONLY_NEW, State, Transport, TransportError, rel};

const PASSWORD: &str = "correct horse battery";
const STAMP: ConflictStamp = ConflictStamp { year: 2026, month: 11, day: 8, hour: 9, minute: 0, utc: false };

struct World {
    url: ServerUrl,
    meta: Meta,
    space: SpaceId,
    ben: UserRecord,
    ana_token: craft_cloud_agent::http::DeviceToken,
    ben_token: craft_cloud_agent::http::DeviceToken,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dirs: Vec<tempfile::TempDir>,
}

/// A device token of `name`, made as the agent's `login` makes it.
async fn token_of(url: &str, name: &str) -> (craft_cloud_agent::http::DeviceToken, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let data = craft_cloud_agent::account::DataDir::new(dir.path().join("data"));
    let args = LoginArgs {
        url: url.to_owned(),
        user: name.to_owned(),
        folder: Some(dir.path().join("Craft Cloud")),
        device_name: Some(format!("{name}-pc")),
        adopt: false,
    };
    cli::login(&data, &args, PASSWORD, &mut Vec::new()).await.unwrap();
    (data.load_account().unwrap().unwrap().token, dir)
}

async fn world() -> World {
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
    let hash = hash_password(PASSWORD).unwrap();
    let ana = meta.create_user("ana", &hash, Role::Member, 0).await.unwrap();
    let ben = meta.create_user("ben", &hash, Role::Member, 0).await.unwrap();
    let space = meta.create_team_space(&SpaceActor::Cli, &SpaceName::new("Studio").unwrap(), ana.id, None, 0).await.unwrap();
    meta.invite_member(&SpaceActor::Cli, space, ben.id, SpaceRole::Viewer, 0).await.unwrap();
    let (ana_token, ana_dir) = token_of(&url, "ana").await;
    let (ben_token, ben_dir) = token_of(&url, "ben").await;
    World { url: ServerUrl::parse(&url).unwrap(), meta, space, ben, ana_token, ben_token, _shutdown: tx, _dirs: vec![db, blobs, ana_dir, ben_dir] }
}

impl World {
    fn transport(&self, token: &craft_cloud_agent::http::DeviceToken) -> HttpTransport {
        HttpTransport::new(self.url.clone(), token).unwrap().bound_to(self.space).unwrap()
    }

    fn engine(&self, token: &craft_cloud_agent::http::DeviceToken, name: &str) -> Engine<MemFs, HttpTransport> {
        Engine::new(MemFs::new(), self.transport(token), State::default(), Config::new(name, DeviceId::generate()), || STAMP).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_viewer_edits_a_file_and_the_servers_copy_is_unchanged() {
    let w = world().await;
    let mut ana = w.engine(&w.ana_token, "ana-pc");
    let mut ben = w.engine(&w.ben_token, "ben-pc");
    ana.fs_mut().write(&rel("doc.txt"), b"base", false, 0).unwrap();
    assert_eq!(ana.sync_once().await.unwrap().uploaded.len(), 1, "an editor uploads");
    assert_eq!(ben.sync_once().await.unwrap().downloaded.len(), 1, "a viewer downloads");
    let before = w.transport(&w.ana_token).tree(None).await.unwrap();
    assert!(before.nodes.iter().any(|n| !n.deleted && n.name.as_ref().is_some_and(|name| name.as_str() == "doc.txt")));

    // The server's answer on the wire.
    let viewer = w.transport(&w.ben_token);
    assert_eq!(
        viewer.put_chunk(&ChunkId::from_bytes(*blake3::hash(b"x").as_bytes()), bytes::Bytes::from_static(b"x")).await,
        Err(TransportError::ReadOnly)
    );
    assert_eq!(viewer.commit(&[] as &[Op]).await, Err(TransportError::ReadOnly));
    assert_eq!(viewer.missing_chunks(&[ChunkId::from_bytes([1; 32])]).await, Err(TransportError::ReadOnly));
    assert!(viewer.tree(None).await.is_ok(), "reading is allowed");

    // The viewer edits the file and adds one: both stay here, nothing is sent, nothing fails.
    ben.fs_mut().write(&rel("doc.txt"), b"viewer edit", false, 0).unwrap();
    ben.fs_mut().write(&rel("mine.txt"), b"mine", false, 0).unwrap();
    let report = ben.sync_once().await.unwrap();
    assert!(report.read_only && ben.is_read_only(), "{report:?}");
    assert!(report.uploaded.is_empty() && report.refused.is_empty(), "{report:?}");
    assert!(report.skipped.contains(&("doc.txt".to_owned(), READ_ONLY_EDITED.to_owned())), "{report:?}");
    assert!(report.skipped.contains(&("mine.txt".to_owned(), READ_ONLY_NEW.to_owned())), "{report:?}");
    assert_eq!(ben.fs().read(&rel("doc.txt")).unwrap(), b"viewer edit");
    assert!(ben.sync_once().await.unwrap().skipped.is_empty(), "quiet once reported");

    // The server's copy is the version it was: same tree, and the owner's engine sees no change.
    let after = w.transport(&w.ana_token).tree(None).await.unwrap();
    assert_eq!(after.nodes, before.nodes);
    assert!(!ana.sync_once().await.unwrap().changed());
    assert_eq!(ana.fs().read(&rel("doc.txt")).unwrap(), b"base");

    // Raised to editor: the same files go up as ordinary changes.
    w.meta.change_member_role(&SpaceActor::Cli, w.space, w.ben.id, SpaceRole::Editor, 1).await.unwrap();
    let report = ben.sync_once().await.unwrap();
    assert!(!report.read_only, "{report:?}");
    assert_eq!(report.uploaded.len(), 2, "{report:?}");
    assert_eq!(ana.sync_once().await.unwrap().downloaded.len(), 2);
    assert_eq!(ana.fs().read(&rel("doc.txt")).unwrap(), b"viewer edit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_personal_space_is_not_read_only_and_needs_no_header() {
    let w = world().await;
    // Without `bound_to` the transport works on the account's personal space, where the account is the owner.
    let own = HttpTransport::new(w.url.clone(), &w.ben_token).unwrap();
    let space = own.space().await.unwrap();
    assert_ne!(space.space, w.space);
    assert!(own.tree(None).await.is_ok());
    assert_eq!(w.transport(&w.ben_token).space().await.unwrap().role, Some(SpaceRole::Viewer));
}
