//! The same scenarios on every backend. SQLite always runs (a fresh temporary file per test). Postgres runs
//! when `CRAFT_CLOUD_TEST_PG` holds an admin URL, e.g. `postgres://postgres:pw@127.0.0.1:5432/postgres`; each
//! test creates and drops its own database. CI sets `CRAFT_CLOUD_REQUIRE_BACKENDS`, which turns a missing
//! `CRAFT_CLOUD_TEST_PG` into a failure instead of a skip.

use craft_cloud_meta::{Meta, MetaError, Role, UserRecord};
use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, FileHash, Manifest, NodeId, NodeKind};

struct Db {
    meta: Meta,
    _guard: Guard,
}

enum Guard {
    Sqlite(#[allow(dead_code)] tempfile::TempDir),
    Postgres { admin_url: String, name: String },
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Guard::Postgres { admin_url, name } = self {
            let (admin_url, name) = (admin_url.clone(), name.clone());
            // Drop the test database from a separate runtime; the test's runtime may be gone.
            let _ = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(async {
                    use sqlx::Connection;
                    if let Ok(mut conn) = sqlx::PgConnection::connect(&admin_url).await {
                        let _ = sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))).execute(&mut conn).await;
                    }
                });
            })
            .join();
        }
    }
}

async fn sqlite() -> Db {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    Db { meta: Meta::open(&url).await.unwrap(), _guard: Guard::Sqlite(dir) }
}

/// A fresh, empty Postgres database: its URL, and the guard that drops it. The guard exists before the
/// caller opens the database, so the database is dropped even when opening it fails.
async fn postgres_database() -> Option<(String, Guard)> {
    let Ok(admin_url) = std::env::var("CRAFT_CLOUD_TEST_PG") else {
        assert!(std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_none(), "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_PG is not");
        return None;
    };
    use sqlx::Connection;
    let name = format!("cc_test_{}", uuid::Uuid::now_v7().simple());
    let mut conn = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&mut conn).await.unwrap();
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    let url = format!("{base}/{name}");
    Some((url, Guard::Postgres { admin_url, name }))
}

async fn postgres() -> Option<Db> {
    let (url, guard) = postgres_database().await?;
    Some(Db { meta: Meta::open(&url).await.unwrap(), _guard: guard })
}

macro_rules! on_every_backend {
    ($($scenario:ident),* $(,)?) => {
        mod on_sqlite {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() { let db = super::sqlite().await; super::$scenario(&db.meta).await; } )*
        }
        mod on_postgres {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                let Some(db) = super::postgres().await else { eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set"); return };
                super::$scenario(&db.meta).await;
            } )*
        }
    };
}

on_every_backend!(
    users_and_devices,
    a_new_space_has_only_its_root,
    manifests_need_their_chunks_and_are_per_space,
    concurrent_signups_with_one_name_give_username_taken,
);

// ---- helpers ----

const NOW: i64 = 1_760_000_000_000;

async fn user(meta: &Meta, username: &str) -> UserRecord {
    meta.create_user(username, "$argon2id$stub", Role::Member, NOW).await.unwrap()
}

async fn root(meta: &Meta, u: &UserRecord) -> NodeId {
    meta.space_info(u.space).await.unwrap().root
}

// ---- scenarios ----

async fn users_and_devices(meta: &Meta) {
    let ana = user(meta, "ana").await;
    assert!(matches!(meta.create_user("ana", "x", Role::Admin, NOW).await, Err(MetaError::UsernameTaken)));
    assert!(matches!(meta.create_user("bad name", "x", Role::Admin, NOW).await, Err(MetaError::Invalid(_))));
    assert_eq!(meta.user_by_name("ana").await.unwrap(), Some(ana.clone()));
    assert_eq!(meta.user_by_name("bob").await.unwrap(), None);

    meta.set_password_hash(ana.id, "$argon2id$new").await.unwrap();
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$new");

    let token = [7u8; 32];
    let dev = meta.create_device(ana.id, "studio pc", &token, NOW).await.unwrap();
    let auth = meta.device_by_token(&token).await.unwrap().unwrap();
    assert_eq!((auth.device, auth.user, auth.space, auth.role), (dev, ana.id, ana.space, Role::Member));
    assert_eq!(meta.device_by_token(&[8u8; 32]).await.unwrap(), None);

    meta.touch_device(dev, NOW + 5).await.unwrap();
    let list = meta.list_devices(ana.id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].name.as_str(), list[0].last_used_ms, list[0].revoked), ("studio pc", NOW + 5, false));

    let bob = user(meta, "bob").await;
    assert!(matches!(meta.revoke_device(bob.id, dev).await, Err(MetaError::NotFound(_))));
    meta.revoke_device(ana.id, dev).await.unwrap();
    assert_eq!(meta.device_by_token(&token).await.unwrap(), None);
    assert!(meta.list_devices(ana.id).await.unwrap()[0].revoked);
}

async fn a_new_space_has_only_its_root(meta: &Meta) {
    let u = user(meta, "ana").await;
    let tree = meta.tree(u.space).await.unwrap();
    assert_eq!(tree.seq, 0);
    assert_eq!(tree.nodes.len(), 1);
    let r = &tree.nodes[0];
    assert_eq!((r.parent, r.name.clone(), r.kind.clone(), r.deleted), (None, None, NodeKind::Dir, false));
    assert_eq!(r.id, root(meta, &u).await);
}

async fn manifests_need_their_chunks_and_are_per_space(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let bob = user(meta, "bob").await;
    let chunk = ChunkId::from_bytes([9; 32]);
    assert_eq!(meta.missing_chunks(ana.space, &[chunk, chunk]).await.unwrap(), vec![chunk]);
    let m =
        Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 5, FileHash::from_bytes([0; 32]), vec![ChunkRef { id: chunk, len: 5 }]).unwrap();
    assert!(matches!(meta.add_manifest(ana.space, &m, 77, NOW).await, Err(MetaError::MissingChunks(list)) if list == vec![chunk]));

    meta.add_chunk(ana.space, &chunk, 6, NOW).await.unwrap();
    meta.add_chunk(ana.space, &chunk, 6, NOW).await.unwrap();
    assert!(meta.missing_chunks(ana.space, &[chunk]).await.unwrap().is_empty());
    // Bob can't learn that Ana has this chunk.
    assert_eq!(meta.missing_chunks(bob.space, &[chunk]).await.unwrap(), vec![chunk]);

    meta.add_manifest(ana.space, &m, 77, NOW).await.unwrap();
    assert!(meta.has_manifest(ana.space, &m.id()).await.unwrap());
    assert!(!meta.has_manifest(bob.space, &m.id()).await.unwrap());
}

async fn concurrent_signups_with_one_name_give_username_taken(meta: &Meta) {
    let tasks: Vec<_> = (0..6)
        .map(|_| {
            let meta = meta.clone();
            tokio::spawn(async move { meta.create_user("ana", "x", Role::Member, NOW).await })
        })
        .collect();
    let mut created = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => created += 1,
            Err(MetaError::UsernameTaken) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!(created, 1);
}

// ---- plain tests ----

#[tokio::test]
async fn in_memory_sqlite_works() {
    let meta = Meta::open("sqlite::memory:").await.unwrap();
    let ana = user(&meta, "ana").await;
    assert_eq!(meta.user_by_name("ana").await.unwrap(), Some(ana));
}

#[tokio::test]
async fn reopening_a_migrated_database_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    let ana = {
        let meta = Meta::open(&url).await.unwrap();
        user(&meta, "ana").await
    };
    let meta = Meta::open(&url).await.unwrap();
    assert_eq!(meta.user_by_name("ana").await.unwrap(), Some(ana));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_opens_migrate_once_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    let opens: Vec<_> = (0..4)
        .map(|_| {
            let url = url.clone();
            tokio::spawn(async move { Meta::open(&url).await })
        })
        .collect();
    for open in opens {
        open.await.unwrap().unwrap();
    }
    use sqlx::Connection;
    let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_version").fetch_one(&mut conn).await.unwrap();
    assert_eq!(versions, 1);
}
