//! The admin command line, called in-process, and the real binary started with `init` + `serve`.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

use clap::Parser;
use craft_cloud_proto::join::{Code, INVITE_CHARS, JoinString};
use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, FileHash, Manifest, Name, NewVersion, NodeId, Op, VersionId};
use craft_cloud_server::cli::{Cli, CliError, run};
use craft_cloud_server::config::{Config, ConfigError};
use craft_cloud_store::{BlobKey, Store};

async fn cli(args: &[&str], stdin: &str) -> Result<String, CliError> {
    let (result, output) = cli_capture(args, stdin).await;
    result?;
    Ok(output)
}

async fn cli_capture(args: &[&str], stdin: &str) -> (Result<(), CliError>, String) {
    let parsed = Cli::try_parse_from(std::iter::once("craft-cloud-server").chain(args.iter().copied())).unwrap();
    let mut out = Vec::new();
    let result = run(parsed.command, &mut Cursor::new(stdin.as_bytes().to_vec()), &mut out).await;
    (result, String::from_utf8(out).unwrap())
}

fn tree_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                out.insert(relative, std::fs::read(path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

fn config_with_database_url(text: &mut String, url: &str) {
    let section = text.find("[database]").unwrap();
    let url_start = section + text[section..].find("url = ").unwrap();
    let url_end = url_start + text[url_start..].find('\n').unwrap_or(text.len() - url_start);
    text.replace_range(url_start..url_end, &format!("url = \"{url}\""));
}

async fn checkpoint_sqlite(url: &str) {
    let mut db = <sqlx::SqliteConnection as sqlx::Connection>::connect(url).await.unwrap();
    sqlx::Executor::execute(&mut db, "PRAGMA wal_checkpoint(TRUNCATE)").await.unwrap();
    sqlx::Connection::close(db).await.unwrap();
}

async fn add_gc_manifest(meta: &craft_cloud_meta::Meta, store: &Store, space: craft_cloud_proto::SpaceId, data: &[u8], hash_byte: u8) -> Manifest {
    let id = ChunkId::from_bytes(*blake3::hash(data).as_bytes());
    store.put(BlobKey::chunk(&id), data).await.unwrap();
    meta.add_chunk(space, &id, u64::try_from(data.len()).unwrap() + 1, 0).await.unwrap();
    let manifest = Manifest::new(
        ChunkerParamsId::new("fastcdc-v1").unwrap(),
        u64::try_from(data.len()).unwrap(),
        FileHash::from_bytes([hash_byte; 32]),
        vec![ChunkRef { id, len: u32::try_from(data.len()).unwrap() }],
    )
    .unwrap();
    store.put(BlobKey::manifest(&manifest.id()), &manifest.encode()).await.unwrap();
    meta.add_manifest(space, &manifest, u64::try_from(manifest.encode().len()).unwrap() + 1, 0).await.unwrap();
    manifest
}

#[test]
fn gc_test_leave_wal_uncheckpointed() {
    let Ok(url) = std::env::var("CRAFT_CLOUD_TEST_WAL_URL") else { return };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let meta = craft_cloud_meta::Meta::open(&url).await.unwrap();
        let ana = meta.user_by_name("ana").await.unwrap().unwrap();
        meta.reset_epoch(ana.space).await.unwrap();
        std::process::exit(0);
    });
}

/// Runs the real `craft-cloud-server` binary with the arguments and waits for it to exit, so that every connection it
/// opened is closed, and every write a close or a checkpoint makes is on disk, when this returns: whether it exited with
/// success, its standard output and its standard error.
fn run_binary(args: &[&str]) -> (bool, String, String) {
    let output = Process::new(env!("CARGO_BIN_EXE_craft-cloud-server")).args(args).stdin(Stdio::null()).output().unwrap();
    (output.status.success(), String::from_utf8(output.stdout).unwrap(), String::from_utf8(output.stderr).unwrap())
}

/// What a database holds, in a form two reads can be compared by. On SQLite the bytes of the database file and of its WAL
/// ([`sqlite_snapshot`]); on Postgres a fingerprint of every table of the schema and the value of every sequence.
#[derive(PartialEq)]
enum DbState {
    Sqlite { db: Vec<u8>, wal: Vec<u8> },
    Postgres { tables: BTreeMap<String, (i64, String)>, sequences: BTreeMap<String, Option<i64>> },
}

impl DbState {
    /// What differs, short enough to read in a failure message (the bytes of a database are not).
    fn difference(&self, other: &DbState) -> String {
        match (self, other) {
            (DbState::Sqlite { db: db1, wal: wal1 }, DbState::Sqlite { db: db2, wal: wal2 }) => {
                let first = |a: &[u8], b: &[u8]| a.iter().zip(b).position(|(x, y)| x != y);
                format!(
                    "database file {} -> {} bytes (first difference at {:?}), WAL {} -> {} bytes (first difference at {:?})",
                    db1.len(),
                    db2.len(),
                    first(db1, db2),
                    wal1.len(),
                    wal2.len(),
                    first(wal1, wal2)
                )
            }
            (DbState::Postgres { tables: t1, sequences: s1 }, DbState::Postgres { tables: t2, sequences: s2 }) => {
                let changed: Vec<_> = t1
                    .keys()
                    .chain(t2.keys())
                    .filter(|name| t1.get(*name) != t2.get(*name))
                    .map(|name| format!("{name}: {:?} -> {:?}", t1.get(name), t2.get(name)))
                    .collect();
                format!("tables changed: {changed:?}; sequences equal: {}", s1 == s2)
            }
            _ => "different kinds of database".to_owned(),
        }
    }
}

/// The names of the files that differ between two snapshots of a directory tree, short enough to read.
fn tree_difference(before: &BTreeMap<PathBuf, Vec<u8>>, after: &BTreeMap<PathBuf, Vec<u8>>) -> String {
    let added: Vec<_> = after.keys().filter(|path| !before.contains_key(*path)).collect();
    let removed: Vec<_> = before.keys().filter(|path| !after.contains_key(*path)).collect();
    let changed: Vec<_> = before.iter().filter(|(path, bytes)| after.get(*path).is_some_and(|other| other != *bytes)).map(|(path, _)| path).collect();
    format!("added {added:?}, removed {removed:?}, changed {changed:?}")
}

/// A server for the GC tests on SQLite (`postgres_url` is `None`) or on the Postgres database at the given URL, with the
/// admin of `init`, a user `ana`, and the handles a test seeds it through.
struct GcServer {
    _parent: tempfile::TempDir,
    dir: PathBuf,
    config: String,
    database_url: String,
    postgres: bool,
    storage: PathBuf,
    meta: craft_cloud_meta::Meta,
    store: Store,
    user: craft_cloud_meta::UserRecord,
}

impl GcServer {
    async fn new(postgres_url: Option<&str>) -> GcServer {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("data");
        cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "seed"], "admin password 1\n").await.unwrap();
        let config_path = dir.join("config.toml");
        if let Some(url) = postgres_url {
            let mut text = std::fs::read_to_string(&config_path).unwrap();
            config_with_database_url(&mut text, url);
            std::fs::write(&config_path, text).unwrap();
        }
        let config = Config::load(&config_path).unwrap();
        let (meta, store) = craft_cloud_server::cli::open(&config).await.unwrap();
        let user = meta.create_user("ana", "test-hash", craft_cloud_meta::Role::Admin, 0).await.unwrap();
        let craft_cloud_server::config::StorageConfig::Disk { path: storage, .. } = &config.storage else { panic!("init uses disk storage") };
        GcServer {
            dir,
            config: config_path.to_str().unwrap().to_owned(),
            database_url: config.database.url.clone(),
            postgres: postgres_url.is_some(),
            storage: storage.clone(),
            meta,
            store,
            user,
            _parent: parent,
        }
    }

    async fn db_state(&self) -> DbState {
        if !self.postgres {
            let (db, wal) = sqlite_snapshot(&self.dir.join("meta.db"));
            return DbState::Sqlite { db, wal };
        }
        let pool = sqlx::PgPool::connect(&self.database_url).await.unwrap();
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.tables WHERE table_schema = current_schema() AND table_type = 'BASE TABLE' ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(!names.is_empty(), "the fingerprint must see the schema's tables");
        let mut tables = BTreeMap::new();
        for name in names {
            let sql = format!(
                "SELECT COUNT(*)::bigint, COALESCE(md5(string_agg(t::text, E'\\n' ORDER BY t::text)), '') FROM \"{}\" t",
                name.replace('"', "\"\"")
            );
            let fingerprint: (i64, String) = sqlx::query_as(sqlx::AssertSqlSafe(sql)).fetch_one(&pool).await.unwrap();
            tables.insert(name, fingerprint);
        }
        let sequences: Vec<(String, Option<i64>)> =
            sqlx::query_as("SELECT sequencename::text, last_value FROM pg_sequences WHERE schemaname = current_schema() ORDER BY 1")
                .fetch_all(&pool)
                .await
                .unwrap();
        pool.close().await;
        DbState::Postgres { tables, sequences: sequences.into_iter().collect() }
    }

    /// Runs the command as a process of its own (see [`run_binary`]) with the database and the whole store directory
    /// tree (names and bytes) read before it starts and after it has exited, and fails the test if either differs.
    async fn run_unchanged(&self, args: &[&str]) -> (bool, String, String) {
        let db_before = self.db_state().await;
        let tree_before = tree_snapshot(&self.storage);
        let result = run_binary(args);
        let db_after = self.db_state().await;
        let tree_after = tree_snapshot(&self.storage);
        assert!(db_before == db_after, "the command changed the database: {}", db_before.difference(&db_after));
        assert!(tree_before == tree_after, "the command changed the store: {}", tree_difference(&tree_before, &tree_after));
        result
    }

    /// `gc --dry-run --json`, which must not change anything: its report, and whether it found no problem.
    async fn gc_json(&self) -> (serde_json::Value, bool) {
        let (ok, out, err) = self.run_unchanged(&["gc", "--dry-run", "--json", "--config", &self.config]).await;
        assert!(ok || err.contains("gc dry-run found problems"), "{err}");
        (serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}: {err}")), ok)
    }

    async fn exec(&self, sql: &'static str) {
        if self.postgres {
            let pool = sqlx::PgPool::connect(&self.database_url).await.unwrap();
            sqlx::query(sql).execute(&pool).await.unwrap();
            pool.close().await;
        } else {
            let mut conn = raw_sqlite(&self.database_url).await;
            sqlx::query(sql).execute(&mut conn).await.unwrap();
            sqlx::Connection::close(conn).await.unwrap();
        }
    }

    async fn count(&self, sql: &'static str) -> i64 {
        if self.postgres {
            let pool = sqlx::PgPool::connect(&self.database_url).await.unwrap();
            let n = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
            pool.close().await;
            n
        } else {
            let mut conn = raw_sqlite(&self.database_url).await;
            let n = sqlx::query_scalar(sql).fetch_one(&mut conn).await.unwrap();
            sqlx::Connection::close(conn).await.unwrap();
            n
        }
    }
}

/// The GC dry run on every scenario the design lists, on the server it is given. Every run goes through
/// [`GcServer::run_unchanged`], so each of them proves that it changed neither the database nor the store.
async fn gc_scenario(s: &GcServer) {
    let c = s.config.as_str();
    let user = &s.user;

    // An empty store: nothing recorded, nothing stored.
    let (empty, ok) = s.gc_json().await;
    assert!(ok, "{empty}");
    assert_eq!(empty["schema_version"], 1);
    assert_eq!(empty["store"]["chunk_orphans"]["count"], 0);
    assert_eq!(empty["store"]["manifest_orphans"]["count"], 0);
    assert_eq!(empty["store"]["foreign"]["count"], 0);
    assert_eq!(empty["candidates"]["chunks"]["count"], 0);
    assert_eq!(empty["nothing_was_changed"], true);

    // Without --dry-run: refused, and nothing changed.
    let (ok, out, err) = s.run_unchanged(&["gc", "--config", c]).await;
    assert!(!ok && err.contains("deleting is not implemented yet; run with --dry-run"), "{err}");
    assert_eq!(out, "");

    // A clean store: a version's manifest and chunk, recorded, stored and named; the roots protect them.
    let current_manifest = add_gc_manifest(&s.meta, &s.store, user.space, &[44; 12], 4).await;
    let device = s.meta.create_device(user.id, "gc-test", &[46; 32], 0).await.unwrap();
    let space = s.meta.space_info(user.space).await.unwrap();
    let node = NodeId::generate();
    let version = NewVersion {
        id: VersionId::generate(),
        manifest: current_manifest.id(),
        mtime_ms: 1,
        exec: false,
        save: Some(craft_cloud_proto::SaveRecord { doc: craft_cloud_proto::live::node_doc(user.space, node), frontiers: vec![] }),
    };
    s.meta.commit(user.space, device, &[Op::Create { node, parent: space.root, name: Name::new("root.bin").unwrap(), version }], 0).await.unwrap();
    let (clean, ok) = s.gc_json().await;
    assert!(ok, "{clean}");
    assert_eq!(clean["database"]["problems"], 0);
    assert_eq!(clean["store"]["chunk_orphans"]["count"], 0);
    assert_eq!(clean["store"]["foreign"]["count"], 0);
    assert_eq!(clean["candidates"]["chunks"]["count"], 0, "the current version's chunk is protected");
    // The recorded inventory: what the database records for what the store holds (one chunk of 12 bytes and one manifest,
    // each stored with its format byte).
    assert_eq!(clean["store"]["chunk_recorded"]["count"], 1);
    assert_eq!(clean["store"]["chunk_recorded"]["stored_bytes"], 13);
    assert_eq!(clean["store"]["manifest_recorded"]["count"], 1);
    assert_eq!(clean["store"]["manifest_recorded"]["stored_bytes"], u64::try_from(current_manifest.encode().len()).unwrap() + 1);
    assert_eq!(clean["store"]["chunk_recorded"]["bytes_source"], "database blobs.stored_size");
    let (_, text, _) = s.run_unchanged(&["gc", "--dry-run", "--config", c]).await;
    assert!(text.contains("store chunks: 1 recorded, 13 stored bytes (as the database records them)"), "{text}");

    // An orphan (stored, no `blobs` row, as a refused upload leaves it) and a foreign object (not a valid key).
    let orphan_data = [41u8];
    let orphan = ChunkId::from_bytes(*blake3::hash(&orphan_data).as_bytes());
    s.store.put(BlobKey::chunk(&orphan), &orphan_data).await.unwrap();
    let foreign_path = s.storage.join("chunks/ff/ff/not-a-hash");
    std::fs::create_dir_all(foreign_path.parent().unwrap()).unwrap();
    std::fs::write(foreign_path, b"foreign").unwrap();
    let (ok, out, err) = s.run_unchanged(&["gc", "--dry-run", "--config", c]).await;
    assert!(ok, "{out}{err}");
    assert!(out.contains("store chunks: 1 orphan(s), 2 stored bytes"), "{out}");
    assert!(out.contains("foreign objects: 1"), "{out}");
    assert!(out.ends_with("nothing was changed\n"), "{out}");
    let (report, _) = s.gc_json().await;
    assert_eq!(report["store"]["chunk_orphans"]["examples"][0], orphan.to_string());
    assert_eq!(report["candidates"]["orphans"]["count"], 0, "an orphan is only a potential candidate");
    assert_eq!(report["candidates"]["orphans"]["potential_count"], 1);

    // A complete index and a chunk no manifest names: the one candidate. Every kind of root protects its chunks: the
    // import snapshot, the live snapshot, a trashed file's version; and a manifest that nothing roots is protected, never
    // a candidate, and its chunk is named by the complete index.
    let candidate_data = [43u8];
    let candidate = ChunkId::from_bytes(*blake3::hash(&candidate_data).as_bytes());
    s.store.put(BlobKey::chunk(&candidate), &candidate_data).await.unwrap();
    s.meta.add_chunk(user.space, &candidate, 2, 1).await.unwrap();
    let import_manifest = add_gc_manifest(&s.meta, &s.store, user.space, &[45; 12], 5).await;
    let doc = craft_cloud_proto::DocId::generate();
    s.meta.create_live_document(user.space, doc, "document:wordcraft", node, &import_manifest.id(), 0).await.unwrap();
    s.meta.append_live_update(user.space, doc, device, 1, b"live state", 1).await.unwrap();
    let live_snapshot = add_gc_manifest(&s.meta, &s.store, user.space, &[50; 12], 6).await;
    assert!(s.meta.compact_live_doc(user.space, doc, 1, &live_snapshot.id()).await.unwrap());
    let unrooted_manifest = add_gc_manifest(&s.meta, &s.store, user.space, &[51; 12], 7).await;
    let trash_manifest = add_gc_manifest(&s.meta, &s.store, user.space, &[52; 12], 8).await;
    let trash_node = NodeId::generate();
    let trash_version = VersionId::generate();
    let trash_new_version = NewVersion { id: trash_version, manifest: trash_manifest.id(), mtime_ms: 2, exec: false, save: None };
    s.meta
        .commit(
            user.space,
            device,
            &[Op::Create { node: trash_node, parent: space.root, name: Name::new("trash.bin").unwrap(), version: trash_new_version }],
            2,
        )
        .await
        .unwrap();
    s.meta.commit(user.space, device, &[Op::Delete { node: trash_node, expected: Some(trash_version) }], 3).await.unwrap();
    let (rooted, ok) = s.gc_json().await;
    assert!(ok, "{rooted}");
    assert_eq!(rooted["candidates"]["chunks"]["count"], 1);
    assert_eq!(rooted["candidates"]["chunks"]["examples"], serde_json::json!([candidate.to_string()]));
    assert_eq!(rooted["candidates"]["manifests"]["protected_count"], 1);
    assert_eq!(rooted["candidates"]["manifests"]["count"], 0);
    assert_eq!(rooted["database"]["verify"]["notices"]["unrooted_manifests"]["examples"][0], unrooted_manifest.id().to_string());
    assert!(rooted["database"]["verify"]["notices"]["unrooted_manifests"]["examples"].as_array().unwrap().iter().all(|id| id
        != &current_manifest.id().to_string()
        && id != &import_manifest.id().to_string()
        && id != &live_snapshot.id().to_string()
        && id != &trash_manifest.id().to_string()));

    // Manifests without an index (as an upgrade finds them): no dependency is known, every chunk is protected.
    s.exec("DELETE FROM manifest_chunks").await;
    s.exec("DELETE FROM manifest_index").await;
    let (unindexed, ok) = s.gc_json().await;
    assert!(ok, "an incomplete index is a protection, not a problem: {unindexed}");
    assert_eq!(unindexed["index_incomplete"], true);
    assert_eq!(unindexed["candidates"]["chunks"]["count"], 0);
    assert_eq!(unindexed["candidates"]["chunks"]["examples"].as_array().unwrap().len(), 0);
    assert_eq!(unindexed["candidates"]["chunks"]["protection"], "index incomplete: dependencies unknown");
    cli(&["index", "build", "--config", c], "").await.unwrap();
    let (rebuilt, _) = s.gc_json().await;
    assert_eq!(rebuilt["index_incomplete"], false);
    assert_eq!(rebuilt["candidates"]["chunks"]["examples"], serde_json::json!([candidate.to_string()]));

    // An index still being built (the second edge not written yet) protects every chunk too.
    let incomplete_data_a = [47u8; 12];
    let incomplete_data_b = [48u8; 12];
    let incomplete_a = ChunkId::from_bytes(*blake3::hash(&incomplete_data_a).as_bytes());
    let incomplete_b = ChunkId::from_bytes(*blake3::hash(&incomplete_data_b).as_bytes());
    for (id, data) in [(incomplete_a, incomplete_data_a), (incomplete_b, incomplete_data_b)] {
        s.store.put(BlobKey::chunk(&id), &data).await.unwrap();
        s.meta.add_chunk(user.space, &id, 13, 2).await.unwrap();
    }
    let incomplete_manifest = Manifest::new(
        ChunkerParamsId::new("fastcdc-v1").unwrap(),
        24,
        FileHash::from_bytes([49; 32]),
        vec![ChunkRef { id: incomplete_a, len: 12 }, ChunkRef { id: incomplete_b, len: 12 }],
    )
    .unwrap();
    s.store.put(BlobKey::manifest(&incomplete_manifest.id()), &incomplete_manifest.encode()).await.unwrap();
    s.meta
        .clone()
        .with_index_batch_rows(1)
        .add_manifest(user.space, &incomplete_manifest, u64::try_from(incomplete_manifest.encode().len()).unwrap() + 1, 2)
        .await
        .unwrap();
    let (building, _) = s.gc_json().await;
    assert_eq!(building["candidates"]["chunks"]["count"], 0);
    assert_eq!(building["candidates"]["chunks"]["examples"].as_array().unwrap().len(), 0);
    assert_eq!(building["candidates"]["chunks"]["protection"], "index incomplete: dependencies unknown");

    // A recorded chunk whose object is gone: a problem (exit code non-zero), and nothing is a candidate.
    let missing_data = [42u8];
    let missing = ChunkId::from_bytes(*blake3::hash(&missing_data).as_bytes());
    s.meta.add_chunk(user.space, &missing, 2, 1).await.unwrap();
    let (ok, report, err) = s.run_unchanged(&["gc", "--dry-run", "--json", "--config", c]).await;
    assert!(!ok && err.contains("gc dry-run found problems"), "{err}");
    assert!(report.contains("chunk_object_missing"), "{report}");
    let missing_report: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert_eq!(missing_report["candidates"]["chunks"]["count"], 0);
    assert_eq!(missing_report["candidates"]["chunks"]["examples"].as_array().unwrap().len(), 0);

    // More objects than one page (1,000): every stored object is classified exactly once, as recorded, orphan or foreign.
    s.store.put(BlobKey::chunk(&missing), &missing_data).await.unwrap();
    for i in 0u64..1001 {
        let id = ChunkId::from_bytes(*blake3::hash(&i.to_le_bytes()).as_bytes());
        let path = s.storage.join(BlobKey::chunk(&id).path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, [0u8]).unwrap();
    }
    let (paged, ok) = s.gc_json().await;
    assert!(ok, "{paged}");
    assert_eq!(paged["store"]["chunk_orphans"]["count"], 1002);
    assert_eq!(paged["store"]["chunk_orphans"]["stored_bytes"], 1003);
    let stored = i64::try_from(tree_snapshot(&s.storage.join("chunks")).len()).unwrap();
    let recorded = s.count("SELECT COUNT(*) FROM blobs WHERE kind = 0").await;
    let orphans = paged["store"]["chunk_orphans"]["count"].as_i64().unwrap();
    let foreign = paged["store"]["foreign"]["count"].as_i64().unwrap();
    assert!(stored > 1000, "the store must hold more than one page: {stored}");
    // Every recorded object is stored here, so the recorded totals are the database's own, across the page boundary.
    assert_eq!(paged["store"]["chunk_recorded"]["count"], recorded);
    assert_eq!(
        paged["store"]["chunk_recorded"]["stored_bytes"],
        s.count("SELECT CAST(COALESCE(SUM(stored_size), 0) AS BIGINT) FROM blobs WHERE kind = 0").await
    );
    assert_eq!(paged["store"]["manifest_recorded"]["count"], s.count("SELECT COUNT(*) FROM blobs WHERE kind = 1").await);
    assert_eq!(
        paged["store"]["manifest_recorded"]["stored_bytes"],
        s.count("SELECT CAST(COALESCE(SUM(stored_size), 0) AS BIGINT) FROM blobs WHERE kind = 1").await
    );
    assert_eq!(stored, recorded + orphans + foreign, "every stored chunk object is recorded, an orphan or foreign, exactly once");
}

#[tokio::test]
async fn gc_dry_run_scenario_on_sqlite_changes_nothing() {
    gc_scenario(&GcServer::new(None).await).await;
}

#[tokio::test]
async fn gc_dry_run_scenario_on_postgres_changes_nothing() {
    let Ok(admin_url) = std::env::var("CRAFT_CLOUD_TEST_PG") else {
        assert!(std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_none(), "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_PG is not");
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    let db_name = format!("gc_{}", uuid::Uuid::now_v7().simple());
    let admin = sqlx::PgPool::connect(&admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db_name}"))).execute(&admin).await.unwrap();
    let (prefix, _) = admin_url.rsplit_once('/').unwrap();
    gc_scenario(&GcServer::new(Some(&format!("{prefix}/{db_name}"))).await).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE {db_name} WITH (FORCE)"))).execute(&admin).await.unwrap();
    admin.close().await;
}

/// A SQLite database that a crashed process left with a WAL that holds data no checkpoint wrote to the database file.
/// The dry run reads through it and leaves the database file and the WAL as they were; it may make a `-shm` file, as
/// `verify` may, which is SQLite's rebuilt index of the WAL and not data.
#[tokio::test]
async fn gc_dry_run_on_sqlite_with_a_crashed_wal_changes_no_data() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config = dir.join("config.toml");
    let config_path = config.to_str().unwrap();
    let loaded = Config::load(&config).unwrap();
    checkpoint_sqlite(&loaded.database.url).await;
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "gc_test_leave_wal_uncheckpointed", "--nocapture"])
        .env("CRAFT_CLOUD_TEST_WAL_URL", &loaded.database.url)
        .status()
        .unwrap();
    assert!(status.success());
    let db_path = dir.join("meta.db");
    let storage = dir.join("blobs");
    assert!(storage.is_dir(), "the store directory of init");
    let (db_before, wal_before) = sqlite_snapshot(&db_path);
    let tree_before = tree_snapshot(&storage);
    assert!(!wal_before.is_empty(), "test requires a non-empty WAL");

    let (ok, out, err) = run_binary(&["gc", "--dry-run", "--json", "--config", config_path]);
    assert!(ok, "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["schema_version"], 1);
    let (db_after, wal_after) = sqlite_snapshot(&db_path);
    assert!(db_before == db_after, "database file changed: {} -> {} bytes", db_before.len(), db_after.len());
    assert!(wal_before == wal_after, "WAL changed: {} -> {} bytes", wal_before.len(), wal_after.len());
    assert!(tree_before == tree_snapshot(&storage), "the store changed");

    // Without --dry-run: refused, and the same bytes.
    let (ok, _, err) = run_binary(&["gc", "--config", config_path]);
    assert!(!ok && err.contains("deleting is not implemented yet; run with --dry-run"), "{err}");
    let (db_after, wal_after) = sqlite_snapshot(&db_path);
    assert!(db_before == db_after && wal_before == wal_after, "a refused command changed the database");
    assert!(tree_before == tree_snapshot(&storage), "a refused command changed the store");
}

/// A SQLite database with no WAL at all (checkpointed and closed): the dry run may leave an empty `-wal` and a `-shm`
/// behind, as `verify` may, and changes no data.
#[tokio::test]
async fn gc_dry_run_on_a_checkpointed_sqlite_database_changes_no_data() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config = dir.join("config.toml");
    let loaded = Config::load(&config).unwrap();
    checkpoint_sqlite(&loaded.database.url).await;
    let db_path = dir.join("meta.db");
    let storage = dir.join("blobs");
    let before = sqlite_snapshot(&db_path);
    let tree_before = tree_snapshot(&storage);
    assert!(before.1.is_empty(), "no WAL to start from");
    let (ok, _, err) = run_binary(&["gc", "--dry-run", "--config", config.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(before == sqlite_snapshot(&db_path), "database file or WAL changed");
    assert!(tree_before == tree_snapshot(&storage), "the store changed");
}

#[tokio::test]
async fn gc_chunk_candidates_use_one_database_audit_snapshot() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config = Config::load(&dir.join("config.toml")).unwrap();
    let (meta, store) = craft_cloud_server::cli::open(&config).await.unwrap();
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    for byte in 0u8..11 {
        let data = [byte];
        let id = ChunkId::from_bytes(*blake3::hash(&data).as_bytes());
        store.put(BlobKey::chunk(&id), &data).await.unwrap();
        meta.add_chunk(ana.space, &id, 2, i64::from(byte)).await.unwrap();
    }
    let report = craft_cloud_server::gc::dry_run(&meta, &store).await.unwrap();
    let json = report.to_json();
    assert_eq!(json["candidates"]["chunks"]["count"], 11);
    assert_eq!(json["candidates"]["chunks"]["examples"].as_array().unwrap().len(), 11);
}

type StoreFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, craft_cloud_store::StoreError>> + Send + 'a>>;

/// A backend that, like one written before `size_raw` existed, cannot tell an object's size without reading it.
struct NoSizeBackend(craft_cloud_store::memory::MemoryBackend);

impl craft_cloud_store::Backend for NoSizeBackend {
    fn put_raw<'a, 'b, 'c>(&'a self, path: &'b str, data: bytes::Bytes) -> StoreFuture<'c, ()>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(async move { craft_cloud_store::Backend::put_raw(&self.0, path, data).await })
    }

    fn get_raw<'a, 'b, 'c>(&'a self, path: &'b str) -> StoreFuture<'c, Option<bytes::Bytes>>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(async move { craft_cloud_store::Backend::get_raw(&self.0, path).await })
    }

    fn delete_raw<'a, 'b, 'c>(&'a self, path: &'b str) -> StoreFuture<'c, ()>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(async move { craft_cloud_store::Backend::delete_raw(&self.0, path).await })
    }

    fn list_raw<'a, 'b, 'c, 'd>(&'a self, prefix: &'b str, after: Option<&'c str>, limit: usize) -> StoreFuture<'d, Vec<String>>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        Box::pin(async move { craft_cloud_store::Backend::list_raw(&self.0, prefix, after, limit).await })
    }
}

/// A backend that cannot report sizes makes the dry run report a problem about the size, once, and exit non-zero. It
/// neither crashes nor takes the object for a size of nothing and goes on.
#[tokio::test]
async fn gc_dry_run_reports_a_backend_that_cannot_give_sizes_as_a_problem() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config = Config::load(&dir.join("config.toml")).unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&config).await.unwrap();
    let store = Store::new(NoSizeBackend(craft_cloud_store::memory::MemoryBackend::new()));
    for byte in 0u8..3 {
        let data = [byte];
        store.put(BlobKey::chunk(&ChunkId::from_bytes(*blake3::hash(&data).as_bytes())), &data).await.unwrap();
    }
    let report = craft_cloud_server::gc::dry_run(&meta, &store).await.unwrap();
    assert!(report.has_problems());
    let json = report.to_json();
    let error = json["store"]["listing_error"].as_str().unwrap();
    assert!(error.contains("could not read stored size") && error.contains("metadata-only object sizes"), "{error}");
    assert_eq!(json["store"]["chunk_orphans"]["count"], 1, "the inventory stops at the first object whose size is unknown");
    assert!(report.to_text().contains("listing error: could not read stored size"));
}

#[tokio::test]
async fn init_then_manage_users_devices_and_epochs() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let dir_s = dir.to_str().unwrap();
    let out = cli(&["init", "--dir", dir_s, "--admin", "Ana"], "admin password 1\n").await.unwrap();
    assert!(out.contains("serve --config"), "{out}");
    let config_path = dir.join("config.toml");
    let config = Config::load(&config_path).unwrap();
    assert!(config.listen.ip().is_loopback());
    let c = config_path.to_str().unwrap();

    // init refuses a directory that isn't empty.
    assert!(matches!(cli(&["init", "--dir", dir_s, "--admin", "x"], "admin password 1\n").await, Err(CliError::Usage(_))));

    assert!(cli(&["user", "--config", c, "add", "bob"], "bob password 1\n").await.unwrap().contains("added bob (member)"));
    assert!(matches!(cli(&["user", "--config", c, "add", "BOB"], "another password\n").await, Err(CliError::Meta(_))));
    assert!(matches!(cli(&["user", "--config", c, "add", "carol"], "short\n").await, Err(CliError::Usage(_))));
    assert!(cli(&["user", "--config", c, "reset-password", "ana"], "a brand new password\n").await.unwrap().contains("password changed for ana"));
    assert_eq!(cli(&["device", "--config", c, "list", "bob"], "").await.unwrap(), "");
    assert!(cli(&["reset-epoch", "--config", c, "bob"], "").await.unwrap().starts_with("new epoch "));
    assert!(matches!(cli(&["reset-epoch", "--config", c, "nobody"], "").await, Err(CliError::Usage(_))));

    // --config may also follow the action.
    assert!(cli(&["user", "add", "--config", c, "dora"], "dora password 1\n").await.unwrap().contains("added dora (member)"));
    assert_eq!(cli(&["device", "list", "--config", c, "dora"], "").await.unwrap(), "");
    assert!(matches!(cli(&["user", "add", "erin"], "erin password 1\n").await, Err(CliError::Usage(m)) if m.contains("--config")));
}

#[tokio::test]
async fn reset_epoch_all_resets_every_space() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    cli(&["user", "--config", c, "add", "bob"], "bob password 1\n").await.unwrap();

    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let spaces = meta.all_spaces().await.unwrap();
    assert_eq!(spaces.len(), 2);
    let mut before = Vec::new();
    for space in &spaces {
        before.push(meta.space_info(*space).await.unwrap().epoch);
    }
    let out = cli(&["reset-epoch", "--config", c, "--all"], "").await.unwrap();
    assert!(out.contains("reset 2 space(s)"), "{out}");
    for (space, old) in spaces.iter().zip(before) {
        assert_ne!(meta.space_info(*space).await.unwrap().epoch, old);
    }
}

/// The config's `compression` reaches the store `open` builds: zstd by default, plain with `"none"`, and what was
/// stored compressed still reads after compression is turned off.
#[tokio::test]
async fn storage_compression_follows_the_config() {
    use craft_cloud_proto::ChunkId;
    use craft_cloud_store::BlobKey;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(
        &["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"],
        "admin password 1
",
    )
    .await
    .unwrap();
    let config_path = dir.join("config.toml");
    let compressible = |tag: &str| format!("{tag}: the same line again and again. ").repeat(500).into_bytes();
    let key = |data: &[u8]| BlobKey::chunk(&ChunkId::from_bytes(*blake3::hash(data).as_bytes()));
    let format_on_disk = |key: BlobKey| std::fs::read(dir.join("blobs").join(key.path())).unwrap()[0];

    let (_, store) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let zstd = compressible("zstd");
    store.put(key(&zstd), &zstd).await.unwrap();
    assert_eq!(format_on_disk(key(&zstd)), 1, "compressed by default");

    // `[storage]` is not the last section (`init` can end with another): the line goes right under its header.
    let text = std::fs::read_to_string(&config_path).unwrap().replace("[storage]\n", "[storage]\ncompression = \"none\"\n");
    std::fs::write(&config_path, text).unwrap();
    let (_, store) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let plain = compressible("none");
    store.put(key(&plain), &plain).await.unwrap();
    assert_eq!(format_on_disk(key(&plain)), 0, "plain with compression = \"none\"");
    assert_eq!(store.get(key(&zstd)).await.unwrap(), zstd, "compressed blobs still read");
}

/// `live reset-doc` empties a full recents document, which takes appends again.
#[tokio::test]
async fn live_reset_doc_empties_a_full_document() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let meta = meta.with_live_doc_cap(10);
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let device = meta.create_device(ana.id, "pc", &[3; 32], 0).await.unwrap();
    let app = craft_cloud_proto::live::AppName::new("wordcraft").unwrap();
    let doc = craft_cloud_proto::live::recents_doc(ana.space, &app);
    meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, 0).await.unwrap();
    meta.append_live_update(ana.space, doc, device, 1, &[1; 20], 0).await.unwrap();
    assert!(meta.append_live_update(ana.space, doc, device, 2, &[2], 0).await.is_err(), "full");
    let out = cli(&["live", "--config", c, "reset-doc", "ana", "wordcraft"], "").await.unwrap();
    assert!(out.contains("emptied the wordcraft recents of ana"), "{out}");
    assert_eq!(meta.live_doc(ana.space, doc).await.unwrap().unwrap().head, 0);
    assert_eq!(meta.append_live_update(ana.space, doc, device, 2, &[2], 0).await.unwrap(), 1, "open again");
    assert!(cli(&["live", "--config", c, "reset-doc", "ana", "Bad App"], "").await.is_err());
    assert!(matches!(cli(&["live", "--config", c, "reset-doc", "nobody", "wordcraft"], "").await, Err(CliError::Usage(_))));
}

/// `live reset-doc` takes a document's ID too (a document of a file has no app name to find it by), and drops its
/// snapshot with its log.
#[tokio::test]
async fn live_reset_doc_takes_a_document_id_and_drops_its_snapshot() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let device = meta.create_device(ana.id, "pc", &[3; 32], 0).await.unwrap();
    let doc = craft_cloud_proto::DocId::generate();
    meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, 0).await.unwrap();
    meta.append_live_update(ana.space, doc, device, 1, &[1; 20], 0).await.unwrap();
    // A snapshot as an agent uploads one: a chunk, and the manifest naming it.
    let chunk = craft_cloud_proto::ChunkId::from_bytes([7; 32]);
    meta.add_chunk(ana.space, &chunk, 31, 0).await.unwrap();
    let manifest = craft_cloud_proto::Manifest::new(
        craft_cloud_proto::ChunkerParamsId::new("fastcdc-v1").unwrap(),
        30,
        craft_cloud_proto::FileHash::from_bytes([7; 32]),
        vec![craft_cloud_proto::ChunkRef { id: chunk, len: 30 }],
    )
    .unwrap();
    meta.add_manifest(ana.space, &manifest, 100, 0).await.unwrap();
    assert!(meta.compact_live_doc(ana.space, doc, 1, &manifest.id()).await.unwrap());
    let out = cli(&["live", "--config", c, "reset-doc", "ana", &doc.to_string()], "").await.unwrap();
    assert!(out.contains(&format!("emptied the live document {doc} of ana")), "{out}");
    let reset = meta.live_doc(ana.space, doc).await.unwrap().unwrap();
    assert_eq!((reset.head, reset.base, reset.has_snapshot), (0, 0, false));
    let missing = craft_cloud_proto::DocId::generate().to_string();
    assert!(cli(&["live", "--config", c, "reset-doc", "ana", &missing], "").await.is_err(), "no such document");
}

/// `user reset-password` signs out every device of the account (and deletes its open invite and pairings) and says
/// how many; `--keep-devices` leaves the devices; `device revoke-all` signs out every device; an unknown user is an
/// error, not a panic.
#[tokio::test]
async fn reset_password_signs_out_every_device_unless_kept() {
    use craft_cloud_meta::RevokeReason;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(
        &["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"],
        "admin password 1
",
    )
    .await
    .unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    for name in ["bob", "carol", "dora"] {
        cli(
            &["user", "--config", c, "add", name],
            "some password 1
",
        )
        .await
        .unwrap();
    }
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let devices = |name: &'static str, n: u8| {
        let meta = meta.clone();
        async move {
            let user = meta.user_by_name(name).await.unwrap().unwrap();
            let mut ids = Vec::new();
            for _ in 0..n {
                let token = *blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes();
                ids.push(meta.create_device(user.id, "pc", &token, 0).await.unwrap());
            }
            (user, ids)
        }
    };

    // The default: every device is signed out, the invite and the pairing are gone, the count is printed.
    let (bob, bobs) = devices("bob", 3).await;
    let invite = Code::generate(INVITE_CHARS).unwrap();
    meta.create_invite("bob", &invite.hash(), 0, 1_000_000).await.unwrap();
    let pairing = Code::generate(craft_cloud_proto::join::PAIRING_CHARS).unwrap();
    let pairing_id = meta.create_pairing(bob.id, bobs[0], &pairing.hash(), 0).await.unwrap();
    let out = cli(
        &["user", "--config", c, "reset-password", "bob"],
        "a brand new password
",
    )
    .await
    .unwrap();
    assert!(out.contains("password changed for bob") && out.contains("signed out 3 device(s)"), "{out}");
    for id in &bobs {
        assert!(!meta.device_active(*id).await.unwrap());
    }
    let listed = meta.list_devices(bob.id).await.unwrap();
    assert!(listed.iter().all(|d| d.revoked && d.revoke_reason == Some(RevokeReason::Reset) && d.revoked_ms.is_some()), "{listed:?}");
    assert_eq!(meta.invite_needs_password(&invite.hash(), 1).await.unwrap(), None);
    assert_eq!(meta.pairing_status(bob.id, pairing_id, 1).await.unwrap(), None);
    assert!(meta.account(bob.id).await.unwrap().password_changed_ms.is_some());
    let shown = cli(&["device", "--config", c, "list", "bob"], "").await.unwrap();
    assert_eq!(shown.matches("(revoked)").count(), 3, "{shown}");

    // `--keep-devices`, before or after the name: the devices stay, the password still changes.
    let (carol, carols) = devices("carol", 2).await;
    let out = cli(
        &["user", "--config", c, "reset-password", "carol", "--keep-devices"],
        "a brand new password
",
    )
    .await
    .unwrap();
    assert!(out.contains("password changed for carol") && out.contains("stay signed in"), "{out}");
    for id in &carols {
        assert!(meta.device_active(*id).await.unwrap());
    }
    assert!(meta.account(carol.id).await.unwrap().password_changed_ms.is_some());
    let out = cli(
        &["user", "--config", c, "reset-password", "--keep-devices", "carol"],
        "another new password
",
    )
    .await
    .unwrap();
    assert!(out.contains("stay signed in"), "{out}");

    // `device revoke-all`.
    let (dora, doras) = devices("dora", 2).await;
    let out = cli(&["device", "--config", c, "revoke-all", "dora"], "").await.unwrap();
    assert!(out.contains("revoked 2 device(s) of dora"), "{out}");
    for id in &doras {
        assert!(!meta.device_active(*id).await.unwrap());
    }
    assert!(meta.list_devices(dora.id).await.unwrap().iter().all(|d| d.revoke_reason == Some(RevokeReason::Admin)));
    assert!(cli(&["device", "--config", c, "revoke-all", "dora"], "").await.unwrap().contains("revoked 0 device(s) of dora"));
    // Another account's devices are untouched.
    assert!(meta.device_active(carols[0]).await.unwrap());

    // An unknown user is an error, and nothing is read or changed.
    assert!(matches!(cli(&["user", "--config", c, "reset-password", "nobody"], "a brand new password
").await, Err(CliError::Usage(m)) if m.contains("nobody")));
    assert!(matches!(cli(&["device", "--config", c, "revoke-all", "nobody"], "").await, Err(CliError::Usage(m)) if m.contains("nobody")));
    // A password that breaks the rules signs nobody out.
    let (_, fresh) = devices("dora", 1).await;
    assert!(matches!(
        cli(
            &["user", "--config", c, "reset-password", "dora"],
            "short
"
        )
        .await,
        Err(CliError::Usage(_))
    ));
    assert!(meta.device_active(fresh[0]).await.unwrap());
}

/// `user invite` prints a join string and link to paste on the new computer (with `public_url` set), or the code
/// and how to finish it (without); a new one replaces the old; `--revoke` ends it.
#[tokio::test]
async fn user_invite_prints_a_join_string_and_revoke_ends_it() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();

    let out = cli(&["user", "--config", c, "invite", "cleo"], "").await.unwrap();
    assert!(out.contains("public_url"), "{out}");
    let code_line = out.lines().find_map(|l| l.strip_prefix("Code: ")).unwrap_or_else(|| panic!("{out}"));
    let bare = Code::parse(code_line, INVITE_CHARS).unwrap();

    // Top-level keys go before the first table.
    let text = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(&config_path, format!("public_url = \"https://Cloud.Example.com/\"\n{text}")).unwrap();
    let out = cli(&["user", "invite", "--config", c, "cleo"], "").await.unwrap();
    let join = out.lines().find_map(|l| l.strip_prefix("Join string: ")).unwrap_or_else(|| panic!("{out}"));
    let link = out.lines().find_map(|l| l.strip_prefix("Link: ")).unwrap_or_else(|| panic!("{out}"));
    let parsed = JoinString::parse(join).unwrap();
    assert_eq!(parsed.server, "https://cloud.example.com");
    assert_eq!(JoinString::parse(link).unwrap(), parsed);
    assert!(link.starts_with("https://cloud.example.com/join/"), "{link}");
    assert!(out.contains("new account"), "{out}");

    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let now = craft_cloud_server::state::now_ms();
    assert_eq!(meta.invite_needs_password(&parsed.code.hash(), now).await.unwrap(), Some(true));
    assert_eq!(meta.invite_needs_password(&bare.hash(), now).await.unwrap(), None, "replaced");
    let ana = cli(&["user", "--config", c, "invite", "ana"], "").await.unwrap();
    assert!(!ana.contains("new account"), "{ana}");

    assert!(cli(&["user", "--config", c, "invite", "--revoke", "cleo"], "").await.unwrap().contains("revoked"));
    assert_eq!(meta.invite_needs_password(&parsed.code.hash(), now).await.unwrap(), None);
    assert!(matches!(cli(&["user", "--config", c, "invite", "--revoke", "cleo"], "").await, Err(CliError::Usage(_))), "nothing to revoke");
    assert!(matches!(cli(&["user", "--config", c, "invite", "bad name"], "").await, Err(CliError::Meta(_) | CliError::Usage(_))));

    std::fs::write(&config_path, format!("public_url = \"ftp://x\"\n{text}")).unwrap();
    assert!(matches!(Config::load(&config_path), Err(ConfigError::PublicUrl(_))));
}

#[tokio::test]
async fn init_validates_first_and_leaves_nothing_behind_on_failure() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let dir_s = dir.to_str().unwrap();

    assert!(matches!(cli(&["init", "--dir", dir_s, "--admin", "bad name"], "admin password 1\n").await, Err(CliError::Usage(_))));
    assert!(!dir.exists());

    // Bad passwords are refused before anything is created.
    assert!(matches!(cli(&["init", "--dir", dir_s, "--admin", "ana"], "").await, Err(CliError::Usage(m)) if m.contains("no password")));
    assert!(matches!(cli(&["init", "--dir", dir_s, "--admin", "ana"], "\n").await, Err(CliError::Usage(_))));
    let long = format!("{}\n", "a".repeat(2000));
    assert!(matches!(cli(&["init", "--dir", dir_s, "--admin", "ana"], &long).await, Err(CliError::Usage(_))));
    assert!(!dir.exists());

    // A valid name in the same place succeeds.
    cli(&["init", "--dir", dir_s, "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert!(dir.join("config.toml").exists());
}

#[tokio::test]
async fn init_needs_behind_proxy_for_a_public_address() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let dir_s = dir.to_str().unwrap();

    let refused = cli(&["init", "--dir", dir_s, "--admin", "ana", "--listen", "0.0.0.0:8080"], "admin password 1\n").await;
    assert!(matches!(refused, Err(CliError::Usage(ref m)) if m.contains("--behind-proxy")), "{refused:?}");
    assert!(!dir.exists());

    // Behind a proxy, the proxy must be named: otherwise every client would share its address.
    let unnamed = cli(&["init", "--dir", dir_s, "--admin", "ana", "--listen", "0.0.0.0:8080", "--behind-proxy"], "admin password 1\n").await;
    assert!(matches!(unnamed, Err(CliError::Config(ConfigError::NoTrustedProxies))), "{unnamed:?}");
    assert!(!dir.exists());

    let args = ["init", "--dir", dir_s, "--admin", "ana", "--listen", "0.0.0.0:8080", "--behind-proxy", "--trusted-proxy", "10.0.0.1"];
    cli(&args, "admin password 1\n").await.unwrap();
    let config = Config::load(&dir.join("config.toml")).unwrap();
    assert!(config.behind_proxy);
    assert_eq!(config.trusted_proxies, vec!["10.0.0.1".parse::<std::net::IpAddr>().unwrap()]);
}

#[tokio::test]
async fn init_writes_the_capacity_it_is_given_and_refuses_a_bad_one() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let dir_s = dir.to_str().unwrap();

    // Without --capacity none is set, as before the option existed.
    let plain = parent.path().join("plain");
    cli(&["init", "--dir", plain.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert_eq!(Config::load(&plain.join("config.toml")).unwrap().storage.capacity(), None);

    // A bad size is refused, names the flag, and leaves nothing behind.
    for bad in ["lots", "1.5", "1.25 TB", "0 GB", "1 XB"] {
        let refused = cli(&["init", "--dir", dir_s, "--admin", "ana", "--capacity", bad], "admin password 1\n").await;
        assert!(matches!(refused, Err(CliError::Usage(ref m)) if m.contains("--capacity")), "{bad}: {refused:?}");
        assert!(!dir.exists(), "{bad}");
    }

    cli(&["init", "--dir", dir_s, "--admin", "ana", "--capacity", "1.5 TiB"], "admin password 1\n").await.unwrap();
    let config = Config::load(&dir.join("config.toml")).unwrap();
    assert_eq!(config.storage.capacity(), Some(3 << 39));
    assert!(matches!(config.storage, craft_cloud_server::config::StorageConfig::Disk { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn init_makes_owner_only_files() {
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(dir.join("config.toml")).unwrap().permissions().mode() & 0o777, 0o600);
    let mode = |name: &str| std::fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("meta.db"), 0o600);
    for name in ["meta.db-wal", "meta.db-shm"] {
        if dir.join(name).exists() {
            assert_eq!(mode(name), 0o600, "{name}");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn init_restricts_an_existing_empty_directory() {
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(dir.join("meta.db")).unwrap().permissions().mode() & 0o777, 0o600);
}

/// Kills the server process when the test ends, even if it panics.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_binary_serves_what_init_created() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let exe = env!("CARGO_BIN_EXE_craft-cloud-server");

    let mut init = Process::new(exe)
        .args(["init", "--dir", dir.to_str().unwrap(), "--admin", "ana", "--listen", &format!("127.0.0.1:{port}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    init.stdin.take().unwrap().write_all(b"admin password 1\n").unwrap();
    assert!(init.wait().unwrap().success());

    let server =
        Server(Process::new(exe).args(["serve", "--config", dir.join("config.toml").to_str().unwrap()]).stderr(Stdio::null()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    let body = loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut conn) => {
                use std::io::Read;
                conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                conn.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                conn.write_all(b"GET /api/v1/info HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
                let mut text = String::new();
                conn.read_to_string(&mut text).unwrap();
                break text;
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => panic!("server never listened: {e}"),
        }
    };
    drop(server);
    assert!(body.starts_with("HTTP/1.1 200"), "{body}");
    assert!(body.contains("\"chunker_params\":[\"fastcdc-v1\"]"), "{body}");
}

/// The command line stays the way back in without HTTP: `user invite` replaces a password reset code the admin API made
/// (one row per account, so the reset code stops working), and makes an invite that adds a computer as before.
#[tokio::test]
async fn user_invite_replaces_a_password_reset_code() {
    use craft_cloud_meta::{AdminActor, AdminVia, RESET_TTL_MS};
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    cli(&["user", "--config", c, "add", "bob"], "bob password 1\n").await.unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let bob = meta.user_by_name("bob").await.unwrap().unwrap();
    let device = meta.create_device(ana.id, "pc", &[9u8; 32], 0).await.unwrap();
    let actor = AdminActor { user: ana.id, via: AdminVia::Device(device), password_hash: ana.password_hash.clone(), two_factor: false };
    let reset = Code::generate(INVITE_CHARS).unwrap();
    meta.admin_issue_reset(&actor, bob.id, &reset.hash(), false, 0, RESET_TTL_MS).await.unwrap();
    assert_eq!(meta.invite_needs_password(&reset.hash(), 1).await.unwrap(), Some(true));

    let out = cli(&["user", "--config", c, "invite", "bob"], "").await.unwrap();
    assert!(out.contains("Invite for bob") && out.contains("adds a computer"), "{out}");
    // The reset code is gone, and the new code is an invite: bob has a password, so it asks for none.
    assert_eq!(meta.invite_needs_password(&reset.hash(), 1).await.unwrap(), None);
    let code = out.lines().find_map(|l| l.strip_prefix("Code: ")).unwrap().trim().to_owned();
    let invite = Code::parse_any(&code).unwrap();
    assert_eq!(meta.invite_needs_password(&invite.hash(), 1).await.unwrap(), Some(false));
    // The command line does not enforce what the HTTP API does: it can reset the last admin's password.
    assert!(cli(&["user", "--config", c, "reset-password", "ana"], "a brand new password\n").await.unwrap().contains("password changed for ana"));
}

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A failed sign-in's event `days` days ago, written through the store a config opens, for the pruning to find.
async fn failure_days_ago(config: &Config, days: i64, target: Option<craft_cloud_meta::UserId>) {
    use craft_cloud_meta::{AuditAction, AuditFailure, Step};
    // Opened without the retention, which would prune the older events when a later one is written: the events are dated
    // in the past here, and the server's clock is not.
    let meta = craft_cloud_meta::Meta::open(&config.database.url).await.unwrap();
    let failure = AuditFailure { action: AuditAction::AuthLoginFailed, target, step: Step::Password };
    meta.audit_record_failure(failure, craft_cloud_server::state::now_ms() - days * DAY_MS).await.unwrap();
}

/// The command line's recovery actions write their `cli.*` events, with no account acting, and `audit list` shows them as
/// text, newest first, with IDs and no names.
#[tokio::test]
async fn the_recovery_commands_write_events_and_audit_list_shows_them() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    cli(&["user", "--config", c, "add", "bob"], "bob password 1\n").await.unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let bob = meta.user_by_name("bob").await.unwrap().unwrap();
    // Adding an account is not one of the events.
    assert!(cli(&["audit", "--config", c, "list"], "").await.unwrap().ends_with("0 event(s)\n"));

    let first = meta.create_device(bob.id, "pc", &[1; 32], 0).await.unwrap();
    let _second = meta.create_device(bob.id, "laptop", &[2; 32], 0).await.unwrap();
    cli(&["user", "--config", c, "reset-password", "bob", "--keep-devices"], "a brand new password\n").await.unwrap();
    cli(&["device", "--config", c, "revoke", "bob", &first.to_string()], "").await.unwrap();
    // The same again changes nothing, and so writes nothing.
    cli(&["device", "--config", c, "revoke", "bob", &first.to_string()], "").await.unwrap();
    meta.create_device(bob.id, "third", &[3; 32], 0).await.unwrap();
    cli(&["device", "--config", c, "revoke-all", "bob"], "").await.unwrap();
    cli(&["device", "--config", c, "revoke-all", "bob"], "").await.unwrap();

    let events = meta.audit_list(&craft_cloud_meta::AuditQuery { limit: 100, ..Default::default() }).await.unwrap();
    let mut names: Vec<&str> = events.iter().map(|e| e.action.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["cli.device_revoked", "cli.devices_revoked_all", "cli.password_reset"]);
    for e in &events {
        assert_eq!(
            (e.actor_user, e.actor_device, e.target_id),
            (None, None, Some(bob.id.0)),
            "the command line is no account: {}",
            e.action.as_str()
        );
    }

    let out = cli(&["audit", "--config", c, "list"], "").await.unwrap();
    assert!(out.ends_with("3 event(s)\n"), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].contains("cli.devices_revoked_all") && lines[0].contains("success") && lines[0].contains("actor=-  device=-"), "{out}");
    assert!(lines[0].contains(&format!("target=user:{}", bob.id.0)) && lines[0].contains(r#"{"revoked":2}"#), "{out}");
    assert!(lines[2].contains("cli.password_reset") && lines[2].contains(r#"{"revoked":0}"#), "{out}");
    // Accounts are named by ID: no name, no device name, no password in the text.
    for secret in ["bob", "ana", "laptop", "third", "$argon2"] {
        assert!(!out.contains(secret), "{secret} in {out}");
    }
    // Filters, on a name or on a group of names, on an account, and a limit.
    assert!(cli(&["audit", "--config", c, "list", "--action", "cli."], "").await.unwrap().ends_with("3 event(s)\n"));
    assert!(cli(&["audit", "--config", c, "list", "--action", "cli.password_reset"], "").await.unwrap().ends_with("1 event(s)\n"));
    assert!(cli(&["audit", "--config", c, "list", "--action", "auth."], "").await.unwrap().ends_with("0 event(s)\n"));
    assert!(cli(&["audit", "--config", c, "list", "--user", "Bob"], "").await.unwrap().ends_with("3 event(s)\n"));
    assert!(cli(&["audit", "--config", c, "list", "--user", "ana"], "").await.unwrap().ends_with("0 event(s)\n"));
    assert!(cli(&["audit", "--config", c, "list", "--limit", "2"], "").await.unwrap().ends_with("2 event(s)\n"));
    // Bad input is a usage error.
    for bad in [&["--limit", "0"][..], &["--limit", "201"], &["--action", "bogus"], &["--action", "cli"], &["--user", "nobody"]] {
        let mut args = vec!["audit", "--config", c, "list"];
        args.extend_from_slice(bad);
        assert!(matches!(cli(&args, "").await, Err(CliError::Usage(_))), "{bad:?}");
    }
    assert!(matches!(cli(&["audit", "list"], "").await, Err(CliError::Usage(m)) if m.contains("--config")));
}

/// `audit prune` deletes the events older than `[audit] retention_days` (365 without the section) and reports how many;
/// `--dry-run` only counts.
#[tokio::test]
async fn audit_prune_deletes_what_is_past_the_retention_and_dry_run_only_counts() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    // Without the section: a year.
    assert_eq!(Config::load(&config_path).unwrap().audit.retention_days, 365);
    for days in [400, 399, 300, 1] {
        failure_days_ago(&Config::load(&config_path).unwrap(), days, None).await;
    }
    let dry = cli(&["audit", "--config", c, "prune", "--dry-run"], "").await.unwrap();
    assert_eq!(dry, "2 audit event(s) are older than 365 days; dry run, nothing deleted\n");
    assert!(cli(&["audit", "--config", c, "list"], "").await.unwrap().ends_with("4 event(s)\n"), "a dry run deletes nothing");
    assert_eq!(cli(&["audit", "--config", c, "prune"], "").await.unwrap(), "deleted 2 audit event(s) older than 365 days\n");
    assert!(cli(&["audit", "--config", c, "list"], "").await.unwrap().ends_with("2 event(s)\n"));
    assert_eq!(cli(&["audit", "--config", c, "prune"], "").await.unwrap(), "deleted 0 audit event(s) older than 365 days\n");

    // With the section: a month.
    let text = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(&config_path, format!("{text}\n[audit]\nretention_days = 30\n")).unwrap();
    assert_eq!(Config::load(&config_path).unwrap().audit.retention_days, 30);
    failure_days_ago(&Config::load(&config_path).unwrap(), 31, None).await;
    assert_eq!(
        cli(&["audit", "--config", c, "prune"], "").await.unwrap(),
        "deleted 2 audit event(s) older than 30 days\n",
        "the one of 300 days ago and the one of 31"
    );
    assert!(cli(&["audit", "--config", c, "list"], "").await.unwrap().ends_with("1 event(s)\n"), "the one of yesterday stays");
}

/// `[audit]` is read or refused as the design says: absent it is the default, a value out of range or an unknown key is an
/// error that names the problem and not the file's text.
#[tokio::test]
async fn the_audit_config_section_is_optional_and_checked() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let base = std::fs::read_to_string(&config_path).unwrap();
    let with = |section: &str| Config::parse(&format!("{base}\n{section}\n"));
    assert_eq!(with("").unwrap().audit.retention_days, 365);
    assert_eq!(with("[audit]").unwrap().audit.retention_days, 365);
    assert_eq!(with("[audit]\nretention_days = 1").unwrap().audit.retention_days, 1);
    assert_eq!(with("[audit]\nretention_days = 3650").unwrap().audit.retention_days, 3650);
    assert!(matches!(with("[audit]\nretention_days = 0"), Err(ConfigError::AuditRetention(0, 1, 3650))));
    assert!(matches!(with("[audit]\nretention_days = 3651"), Err(ConfigError::AuditRetention(3651, 1, 3650))));
    assert!(matches!(with("[audit]\nretention_days = -1"), Err(ConfigError::Parse(_))));
    assert!(matches!(with("[audit]\nretention = 30"), Err(ConfigError::Parse(_))), "an unknown key is refused");
    let shown = with("[audit]\nretention_days = 9999").unwrap_err().to_string();
    assert!(shown.contains("9999") && shown.contains("3650"), "{shown}");
}

// ---- two-factor sign-in: the key file, `serve`, `user reset-2fa` and `two-factor rekey` ----

use craft_cloud_meta::{DeviceActor, Meta, MetaError, RevokeReason, UserId};
use craft_cloud_server::secretbox::{KEY_BYTES, SecretBox, TwoFactorKey};
use craft_cloud_server::two_factor::{KeyState, check_startup, load_keys, rekey_hooked};

/// A data directory made by `init` (admin `ana`) and its database.
async fn init_dir(parent: &std::path::Path) -> (std::path::PathBuf, Meta) {
    let dir = parent.join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&dir.join("config.toml")).unwrap()).await.unwrap();
    (dir, meta)
}

/// Turns two-factor sign-in on for `name` (made if needed) with `sealed` as its secret; returns the device that did it.
async fn protect(meta: &Meta, name: &str, sealed: &[u8]) -> (craft_cloud_meta::UserRecord, craft_cloud_proto::DeviceId) {
    let user = match meta.user_by_name(name).await.unwrap() {
        Some(user) => user,
        None => meta.create_user(name, "$argon2id$stub", craft_cloud_meta::Role::Member, 0).await.unwrap(),
    };
    let device = meta.create_device(user.id, "pc", blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes(), 0).await.unwrap();
    let actor = DeviceActor { user: user.id, device, password_hash: user.password_hash.clone() };
    meta.two_factor_begin_setup(&actor, sealed, 1_000).await.unwrap();
    let hashes: Vec<[u8; 32]> = (0..10).map(|_| Code::generate(INVITE_CHARS).unwrap().hash()).collect();
    meta.two_factor_confirm(user.id, device, &|_: UserId, _: &[u8]| Ok(Some(5)), &hashes, 2_000).await.unwrap();
    (user, device)
}

#[tokio::test]
async fn init_makes_the_two_factor_key_and_names_it_in_the_config() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let out = cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert!(out.contains("two-factor.key") && out.contains("apart from your database backups"), "{out}");
    let key = std::fs::read(dir.join("two-factor.key")).unwrap();
    assert_eq!(key.len(), KEY_BYTES);
    assert_ne!(key, vec![0u8; KEY_BYTES]);
    let config = Config::load(&dir.join("config.toml")).unwrap();
    let section = config.two_factor.clone().unwrap();
    assert_eq!(section.key_file, std::path::PathBuf::from("two-factor.key"));
    assert_eq!(config.key_path(&section.key_file), dir.join("two-factor.key"));
    assert!(matches!(load_keys(&config).unwrap(), KeyState::Loaded(_)));
    // The key is in no log line of init and not in the config.
    let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(text.contains("[two_factor]") && text.contains("key_file = \"two-factor.key\""), "{text}");
    // Two directories, two keys.
    let other = parent.path().join("other");
    cli(&["init", "--dir", other.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert_ne!(std::fs::read(other.join("two-factor.key")).unwrap(), key);
}

#[cfg(unix)]
#[tokio::test]
async fn the_two_factor_key_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert_eq!(std::fs::metadata(dir.join("two-factor.key")).unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn a_config_from_before_two_factor_sign_in_loads_and_has_no_key() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let path = dir.join("config.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.split("[two_factor]").next().unwrap()).unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.two_factor, None);
    assert!(matches!(load_keys(&config).unwrap(), KeyState::NotConfigured));
    // No key and no secrets: the server runs, and two-factor sign-in can't be set up.
    assert!(check_startup(&meta, KeyState::NotConfigured, &config, 0).await.unwrap().is_none());
    // An unknown key in the section is refused.
    std::fs::write(&path, format!("{text}key_pile = 1\n")).unwrap();
    assert!(matches!(Config::load(&path), Err(ConfigError::Parse(_))));
}

/// The server fails closed: it will not start on a database whose two-factor secrets its key can't open, and it opens every one
/// of them to know. The message names the key file the config expects.
#[tokio::test]
async fn the_server_refuses_to_start_when_the_key_cannot_open_every_secret() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let config = Config::load(&dir.join("config.toml")).unwrap();
    let key_path = dir.join("two-factor.key");
    let KeyState::Loaded(secret_box) = load_keys(&config).unwrap() else { panic!("a key") };
    let (ana, bob, cy) = (meta.user_by_name("ana").await.unwrap().unwrap(), protect(&meta, "bob", b"x").await.0, protect(&meta, "cy", b"x").await.0);
    let _ = (ana, bob);
    // Nothing sealed with the key yet: every secret is a stranger's.
    let err = check_startup(&meta, load_keys(&config).unwrap(), &config, 5_000).await.unwrap_err();
    assert!(err.contains("2 of 2") && err.contains(&key_path.display().to_string()), "{err}");
    // Re-seal both properly, then break one: all are opened, so one is enough to refuse.
    for name in ["bob", "cy"] {
        let user = meta.user_by_name(name).await.unwrap().unwrap();
        let sealed = secret_box.seal(user.id, b"12345678901234567890").unwrap();
        assert!(meta.two_factor_replace_ciphertext(user.id, false, b"x", &sealed).await.unwrap());
    }
    let opened = check_startup(&meta, load_keys(&config).unwrap(), &config, 5_000).await.unwrap();
    assert!(opened.is_some(), "the right key opens them all");
    let sealed_for_someone_else = secret_box.seal(UserId(uuid::Uuid::now_v7()), b"12345678901234567890").unwrap();
    let current = meta.two_factor_secrets(5_000).await.unwrap().into_iter().find(|s| s.user == cy.id).unwrap().active.unwrap();
    assert!(meta.two_factor_replace_ciphertext(cy.id, false, &current, &sealed_for_someone_else).await.unwrap());
    let err = check_startup(&meta, load_keys(&config).unwrap(), &config, 5_000).await.unwrap_err();
    assert!(err.contains("1 of 2") && err.contains(&key_path.display().to_string()), "{err}");
    assert!(!err.contains("12345678901234567890"), "no secret in the message");

    // A different key at the same path: the same.
    std::fs::write(&key_path, [7u8; KEY_BYTES]).unwrap();
    let err = check_startup(&meta, load_keys(&config).unwrap(), &config, 5_000).await.unwrap_err();
    assert!(err.contains("2 of 2"), "{err}");
    // No key file at all, with secrets: refused, naming the path (the server never makes one).
    std::fs::remove_file(&key_path).unwrap();
    let keys = load_keys(&config).unwrap();
    assert!(matches!(keys, KeyState::Missing(ref p) if *p == key_path), "{keys:?}");
    let err = check_startup(&meta, keys, &config, 5_000).await.unwrap_err();
    assert!(err.contains(&key_path.display().to_string()) && err.contains("does not exist"), "{err}");
    assert!(!key_path.exists(), "nothing made it");
    // A key file of the wrong length is an error of its own, naming the file.
    std::fs::write(&key_path, [7u8; 31]).unwrap();
    assert!(load_keys(&config).unwrap_err().to_string().contains("exactly 32 bytes"));
    // No `[two_factor]` section at all, with secrets: refused too.
    let mut bare = config.clone();
    bare.two_factor = None;
    let err = check_startup(&meta, KeyState::NotConfigured, &bare, 5_000).await.unwrap_err();
    assert!(err.contains("key_file"), "{err}");
}

#[tokio::test]
async fn without_secrets_a_missing_key_does_not_stop_the_server() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let config = Config::load(&dir.join("config.toml")).unwrap();
    std::fs::remove_file(dir.join("two-factor.key")).unwrap();
    let keys = load_keys(&config).unwrap();
    assert!(matches!(keys, KeyState::Missing(_)));
    assert!(check_startup(&meta, keys, &config, 0).await.unwrap().is_none(), "it runs; set-up answers two_factor_unavailable");
    assert!(!dir.join("two-factor.key").exists());
}

#[tokio::test]
async fn reset_2fa_takes_it_away_and_signs_every_device_out() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let c = dir.join("config.toml");
    let c = c.to_str().unwrap();
    let (bob, device) = protect(&meta, "bob", b"sealed").await;
    let extra = meta.create_device(bob.id, "second", &[8u8; 32], 0).await.unwrap();
    // Wrong codes lock it; the reset lifts the lock with the rest.
    for _ in 0..10 {
        let wrong = |_: UserId, _: &[u8]| -> Result<Option<i64>, craft_cloud_meta::UnreadableSecret> { Ok(None) };
        let _ = meta.confirm_second_factor(bob.id, Some(&craft_cloud_meta::Proof::Totp(&wrong)), 3_000).await;
    }
    assert!(meta.two_factor_locked_for(bob.id, 3_001).await.unwrap().is_some());
    let out = cli(&["user", "--config", c, "reset-2fa", "Bob"], "").await.unwrap();
    assert!(out.contains("two-factor sign-in of bob is off") && out.contains("signed out 2 device(s)"), "{out}");
    assert!(!meta.two_factor_enabled(bob.id).await.unwrap());
    assert_eq!(meta.two_factor_locked_for(bob.id, 3_001).await.unwrap(), None);
    for id in [device, extra] {
        assert!(!meta.device_active(id).await.unwrap());
    }
    assert!(meta.list_devices(bob.id).await.unwrap().iter().all(|d| d.revoke_reason == Some(RevokeReason::TwoFactor)));
    // Again: nothing to reset, and nothing is signed out. An unknown user is an error, not a panic.
    let fresh = meta.create_device(bob.id, "fresh", &[9u8; 32], 0).await.unwrap();
    assert!(matches!(cli(&["user", "--config", c, "reset-2fa", "bob"], "").await, Err(CliError::Meta(MetaError::Conflict(_)))));
    assert!(meta.device_active(fresh).await.unwrap());
    assert!(matches!(cli(&["user", "--config", c, "reset-2fa", "nobody"], "").await, Err(CliError::Usage(m)) if m.contains("nobody")));
    // `--config` after the action, as everywhere.
    protect(&meta, "bob", b"sealed").await;
    assert!(cli(&["user", "reset-2fa", "--config", c, "bob"], "").await.unwrap().contains("is off"));
}

/// `two-factor rekey`: every secret under the older key is rewritten under the current one; it can be run again, refuses
/// (changing nothing) when a secret has a key that is neither, and after it the older key can go.
#[tokio::test]
async fn rekey_moves_every_secret_to_the_new_key_and_is_repeatable() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let c = dir.join("config.toml");
    let (old_path, new_path) = (dir.join("two-factor.key"), dir.join("new.key"));
    let old = TwoFactorKey::from_file(&old_path).unwrap();
    let old_box = SecretBox::new(&old, None).unwrap();
    let mut users = Vec::new();
    for name in ["bob", "cy"] {
        let user = meta.create_user(name, "$argon2id$stub", craft_cloud_meta::Role::Member, 0).await.unwrap();
        users.push(protect(&meta, name, &old_box.seal(user.id, b"12345678901234567890").unwrap()).await.0);
    }
    // A set-up that is open is rewritten as well.
    let dora = meta.create_user("dora", "$argon2id$stub", craft_cloud_meta::Role::Member, 0).await.unwrap();
    let dev = meta.create_device(dora.id, "pc", &[4u8; 32], 0).await.unwrap();
    let actor = DeviceActor { user: dora.id, device: dev, password_hash: "$argon2id$stub".to_owned() };
    let now = craft_cloud_server::state::now_ms();
    meta.two_factor_begin_setup(&actor, &old_box.seal(dora.id, b"pending-pending-pend").unwrap(), now).await.unwrap();

    // Rotating: the new key in `key_file`, the old one in `key_file_old`.
    let new_key = loop {
        let key = TwoFactorKey::generate().unwrap();
        if key.id() != old.id() {
            break key;
        }
    };
    std::fs::write(&new_path, new_key.to_file_bytes().as_slice()).unwrap();
    let text = std::fs::read_to_string(&c).unwrap();
    let rotated = text.replace("key_file = \"two-factor.key\"", "key_file = \"new.key\"\nkey_file_old = \"two-factor.key\"");
    assert_ne!(rotated, text);
    std::fs::write(&c, &rotated).unwrap();
    let cs = c.to_str().unwrap();
    // The server starts in the middle of a rotation: both keys open everything.
    let config = Config::load(&c).unwrap();
    assert!(check_startup(&meta, load_keys(&config).unwrap(), &config, now).await.unwrap().is_some());

    let out = cli(&["two-factor", "--config", cs, "rekey"], "").await.unwrap();
    assert!(
        out.contains("rewrote 3 secret(s) under the current key; 0 were under it already")
            && out.contains("every secret is under the current key now")
            && out.contains("key_file_old can be removed once every server instance"),
        "{out}"
    );
    let new_box = SecretBox::new(&new_key, None).unwrap();
    for user in &users {
        let sealed = meta.two_factor_secrets(now).await.unwrap().into_iter().find(|s| s.user == user.id).unwrap().active.unwrap();
        assert_eq!(new_box.open(user.id, &sealed).unwrap().as_slice(), b"12345678901234567890", "only the new key is needed now");
        assert_eq!(SecretBox::key_id(&sealed), Some(new_key.id()));
    }
    let pending = meta.two_factor_secrets(now).await.unwrap().into_iter().find(|s| s.user == dora.id).unwrap().pending.unwrap();
    assert_eq!(new_box.open(dora.id, &pending).unwrap().as_slice(), b"pending-pending-pend");
    // Run again: nothing left to do. Then the old key goes, and the server starts on the new one alone.
    let out = cli(&["two-factor", "--config", cs, "rekey"], "").await.unwrap();
    assert!(out.contains("rewrote 0 secret(s) under the current key; 3 were under it already"), "{out}");
    std::fs::write(&c, text.replace("key_file = \"two-factor.key\"", "key_file = \"new.key\"")).unwrap();
    std::fs::remove_file(&old_path).unwrap();
    let config = Config::load(&c).unwrap();
    assert!(check_startup(&meta, load_keys(&config).unwrap(), &config, now).await.unwrap().is_some());

    // A secret under a key that is neither refuses the whole rotation and changes nothing.
    let stranger = SecretBox::new(&key_with_other_name(&[old.id(), new_key.id()]), None).unwrap();
    let eve = meta.create_user("eve", "$argon2id$stub", craft_cloud_meta::Role::Member, 0).await.unwrap();
    protect(&meta, "eve", &stranger.seal(eve.id, b"12345678901234567890").unwrap()).await;
    let before = meta.two_factor_secrets(now).await.unwrap();
    std::fs::write(&c, rotated).unwrap();
    std::fs::write(&old_path, old.to_file_bytes().as_slice()).unwrap();
    let refused = cli(&["two-factor", "--config", cs, "rekey"], "").await;
    assert!(matches!(refused, Err(CliError::Usage(ref m)) if m.contains("neither") && m.contains("nothing was changed")), "{refused:?}");
    assert_eq!(meta.two_factor_secrets(now).await.unwrap(), before);
    // And the server doesn't start with it either.
    let config = Config::load(&c).unwrap();
    assert!(check_startup(&meta, load_keys(&config).unwrap(), &config, now).await.is_err());
    // Without any key configured, rekey says so.
    std::fs::write(&c, text.split("[two_factor]").next().unwrap()).unwrap();
    assert!(matches!(cli(&["two-factor", "--config", cs, "rekey"], "").await, Err(CliError::Usage(m)) if m.contains("no key")));
}

/// A key whose name is none of `taken`.
fn key_with_other_name(taken: &[u8]) -> TwoFactorKey {
    loop {
        let key = TwoFactorKey::generate().unwrap();
        if !taken.contains(&key.id()) {
            return key;
        }
    }
}

fn spawn_serve(exe: &str, config: &std::path::Path) -> Child {
    Process::new(exe).args(["serve", "--config", config.to_str().unwrap()]).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap()
}

fn init_with_binary(exe: &str, dir: &std::path::Path, port: u16) {
    use std::io::Write;
    let mut init = Process::new(exe)
        .args(["init", "--dir", dir.to_str().unwrap(), "--admin", "ana", "--listen", &format!("127.0.0.1:{port}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    init.stdin.take().unwrap().write_all(b"admin password 1\n").unwrap();
    assert!(init.wait().unwrap().success());
}

fn listens(port: u16, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `serve` never makes the key. Without one and without secrets it runs; with secrets in the database it exits with an error
/// that names the key file, and listens on nothing.
#[test]
fn serve_never_makes_the_key_and_refuses_secrets_it_cannot_open() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let exe = env!("CARGO_BIN_EXE_craft-cloud-server");
    init_with_binary(exe, &dir, port);
    let key_path = dir.join("two-factor.key");
    assert!(key_path.exists());

    // No key file, no secrets: it runs, and the file is still not there.
    std::fs::remove_file(&key_path).unwrap();
    let running = Server(spawn_serve(exe, &dir.join("config.toml")));
    assert!(listens(port, Duration::from_secs(20)), "the server never listened");
    drop(running);
    assert!(!key_path.exists(), "serve must not make the key");

    // A secret in the database and no key: it exits at once with the path in its message, and listens on nothing.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (meta, _) = craft_cloud_server::cli::open(&Config::load(&dir.join("config.toml")).unwrap()).await.unwrap();
        protect(&meta, "bob", b"a sealed secret that no key here can open").await;
    });
    let mut refused = spawn_serve(exe, &dir.join("config.toml"));
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match refused.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            None => {
                let _ = refused.kill();
                panic!("the server kept running on secrets it cannot open");
            }
        }
    };
    assert!(!status.success());
    let mut message = String::new();
    std::io::Read::read_to_string(&mut refused.stderr.take().unwrap(), &mut message).unwrap();
    assert!(message.contains("error:") && message.contains(&key_path.display().to_string()), "{message}");
    assert!(!message.contains("a sealed secret"), "no secret in the message");
    assert!(!key_path.exists(), "serve must not make the key");
    assert!(!listens(port, Duration::from_millis(300)));

    // With a key that doesn't open the secret: refused as well.
    std::fs::write(&key_path, [3u8; KEY_BYTES]).unwrap();
    let refused = Process::new(exe).args(["serve", "--config", dir.join("config.toml").to_str().unwrap()]).output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("can't be opened"), "{}", String::from_utf8_lossy(&refused.stderr));
}

/// A confirmation that lands exactly between `two-factor rekey`'s first scan and its rewrite can't leave a secret under the old
/// key. The seam (`rekey_hooked`) holds the rekey after its scan until the confirmations, which move each set-up's secret to the
/// active one, have all committed. Rekey reads an account's secrets fresh under its lock; a rewrite from the scan's snapshot would
/// find its pending ciphertext gone and leave the one now active under the old key. The first successful rekey is the one that
/// is checked: no second run repairs anything here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_confirmation_between_rekeys_scan_and_rewrite_leaves_nothing_under_the_old_key() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let old = TwoFactorKey::from_file(&dir.join("two-factor.key")).unwrap();
    let old_box = SecretBox::new(&old, None).unwrap();
    let new_key = key_with_other_name(&[old.id()]);
    let now = craft_cloud_server::state::now_ms();
    for round in 0..4 {
        let mut confirms = Vec::new();
        for n in 0..6 {
            let user = meta.create_user(&format!("u{round}x{n}"), "$argon2id$stub", craft_cloud_meta::Role::Member, 0).await.unwrap();
            let device = meta.create_device(user.id, "pc", blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes(), 0).await.unwrap();
            let actor = DeviceActor { user: user.id, device, password_hash: "$argon2id$stub".to_owned() };
            meta.two_factor_begin_setup(&actor, &old_box.seal(user.id, b"12345678901234567890").unwrap(), now).await.unwrap();
            confirms.push((user.id, device));
        }
        let (scanned_tx, scanned_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let rekeying = {
            let meta = meta.clone();
            let both = SecretBox::new(&new_key, Some(&old)).unwrap();
            tokio::spawn(async move {
                let go_rx = std::sync::Mutex::new(go_rx);
                let hold = move || {
                    let _ = scanned_tx.send(());
                    let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(30));
                };
                rekey_hooked(&meta, &both, &hold).await
            })
        };
        // Rekey has scanned (its snapshot holds every set-up's secret as a pending one under the old key)…
        tokio::task::spawn_blocking(move || scanned_rx.recv_timeout(Duration::from_secs(30))).await.unwrap().expect("rekey scanned");
        // …now every confirmation commits, moving the pending secret to the active one…
        for (user, device) in confirms {
            let hashes: Vec<[u8; 32]> = (0..10).map(|_| Code::generate(INVITE_CHARS).unwrap().hash()).collect();
            meta.two_factor_confirm(user, device, &|_: UserId, _: &[u8]| Ok(Some(5)), &hashes, now + 1).await.unwrap();
        }
        assert!(
            meta.two_factor_all_secrets().await.unwrap().iter().all(|row| row.pending.is_none() && row.active.is_some()),
            "the set-ups are active now"
        );
        // …and only then does rekey go on.
        go_tx.send(()).unwrap();
        let done = rekeying.await.unwrap().expect("rekey succeeds");
        assert_eq!(done.rewritten, 6, "round {round}: the secrets that became active meanwhile were rewritten");
        for row in meta.two_factor_all_secrets().await.unwrap() {
            for sealed in [row.active, row.pending].into_iter().flatten() {
                assert_eq!(SecretBox::key_id(&sealed), Some(new_key.id()), "round {round}: a secret is still under the old key after one rekey");
            }
        }
    }
    let alone = SecretBox::new(&new_key, None).unwrap();
    for row in meta.two_factor_all_secrets().await.unwrap() {
        for sealed in [row.active, row.pending].into_iter().flatten() {
            alone.open(row.user, &sealed).unwrap();
        }
    }
}

/// A listener on a free loopback port that answers every connection with `reply` (or, with `None`, reads the
/// request and then stays silent until `hold` has passed), for `healthcheck` to ask.
fn fake_server(reply: Option<&'static str>, hold: Duration) -> std::net::SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut request = [0u8; 512];
            let _ = conn.read(&mut request);
            match reply {
                Some(text) => {
                    let _ = conn.write_all(text.as_bytes());
                }
                None => std::thread::sleep(hold),
            }
        }
    });
    addr
}

fn healthcheck(addr: std::net::SocketAddr) -> std::process::Output {
    Process::new(env!("CARGO_BIN_EXE_craft-cloud-server")).args(["healthcheck", "--addr", &addr.to_string()]).output().unwrap()
}

#[test]
fn healthcheck_succeeds_only_on_a_200() {
    let ok = fake_server(Some("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}"), Duration::ZERO);
    let out = healthcheck(ok);
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());

    let broken = fake_server(Some("HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n"), Duration::ZERO);
    let out = healthcheck(broken);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("status 500") && !stderr.contains("panicked"), "{stderr}");

    let not_http = fake_server(Some("hello there\r\n"), Duration::ZERO);
    let out = healthcheck(not_http);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stderr).unwrap().contains("not an HTTP answer"));
}

#[test]
fn healthcheck_needs_a_complete_status_line() {
    // Stops inside the status line, with and without closing the connection later.
    let truncated = fake_server(Some("HTTP/1.1 200"), Duration::ZERO);
    let out = healthcheck(truncated);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stderr).unwrap().contains("ended inside the status line"));

    // More than the buffer holds without a line end.
    let endless = fake_server(Some("HTTP/1.1 200 OK xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"), Duration::ZERO);
    let out = healthcheck(endless);
    assert_eq!(out.status.code(), Some(1));

    // A bare line feed is not a status line end.
    let bare = fake_server(Some("HTTP/1.1 200 OK\n"), Duration::ZERO);
    assert_eq!(healthcheck(bare).status.code(), Some(1));
}

#[test]
fn healthcheck_fails_with_nobody_listening_and_with_a_bad_address() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let out = healthcheck(std::net::SocketAddr::from(([127, 0, 0, 1], port)));
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("can't connect") && !stderr.contains("panicked"), "{stderr}");

    let out = Process::new(env!("CARGO_BIN_EXE_craft-cloud-server")).args(["healthcheck", "--addr", "not an address"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stderr).unwrap().contains("bad address"));
}

#[test]
fn healthcheck_gives_up_on_an_answer_that_never_comes() {
    // Called in-process with a short timeout so the test stays quick; the binary uses `HEALTH_TIMEOUT`.
    let silent = fake_server(None, Duration::from_secs(5));
    let started = Instant::now();
    let result = craft_cloud_server::cli::check_health(silent, Duration::from_millis(300));
    assert!(matches!(result, Err(CliError::Usage(m)) if m.contains("no answer in time")));
    assert!(started.elapsed() < Duration::from_secs(3));

    let started = Instant::now();
    let out = healthcheck(silent);
    assert_eq!(out.status.code(), Some(1));
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
}

#[test]
fn healthcheck_is_ok_against_the_real_server() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let exe = env!("CARGO_BIN_EXE_craft-cloud-server");
    init_with_binary(exe, &dir, port);
    let _server = Server(spawn_serve(exe, &dir.join("config.toml")));
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if healthcheck(addr).status.success() {
            return;
        }
        assert!(Instant::now() < deadline, "the server never became healthy");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[tokio::test]
async fn web_sessions_prune_deletes_only_what_is_over() {
    use craft_cloud_meta::{NewChallenge, NewWebSession, WebSignIn};
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    let config = Config::load(&config_path).unwrap();
    let idle = config.web_admin.idle_ms();
    let now = craft_cloud_server::state::now_ms();

    // One session per way of being over, and one that is not. (The sign-in checks the generation it is given.)
    let (meta, _) = craft_cloud_server::cli::open(&config).await.unwrap();
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let sign_in = |at: i64, absolute_ms: i64| {
        let (meta, ana) = (meta.clone(), ana.clone());
        async move {
            let session = NewWebSession { id_hash: *blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes(), absolute_ms };
            let challenge =
                NewChallenge { id_hash: *blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes(), client_key: "ip:127.0.0.1".to_owned() };
            let made = meta.web_sign_in(ana.id, (&ana.password_hash, ana.auth_gen), &challenge, &session, at).await.unwrap();
            assert_eq!(made, Some(WebSignIn::Session));
            session
        }
    };
    let day = 24 * 3_600_000;
    let idle_one = sign_in(now - idle - 1_000, 30 * day).await;
    let expired_one = sign_in(now - 2 * day, day).await;
    let logged_out = sign_in(now - 1_000, day).await;
    meta.web_logout(&logged_out.id_hash, now - 500).await.unwrap();
    let valid = sign_in(now - 1_000, day).await;

    let out = cli(&["web-sessions", "--config", c, "prune", "--dry-run"], "").await.unwrap();
    assert_eq!(out, "3 web session(s) are over; dry run, nothing deleted\n");
    assert!(meta.web_sessions_count_over(now, idle).await.unwrap() == 3, "a dry run deletes nothing");
    let out = cli(&["web-sessions", "prune", "--config", c], "").await.unwrap();
    assert_eq!(out, "deleted 3 web session(s) that were over\n");
    assert_eq!(cli(&["web-sessions", "--config", c, "prune"], "").await.unwrap(), "deleted 0 web session(s) that were over\n");
    assert!(meta.web_session(&valid.id_hash, now, idle).await.unwrap().is_some(), "a valid session is never touched");
    for gone in [&idle_one, &expired_one, &logged_out] {
        assert!(meta.web_session(&gone.id_hash, now, idle).await.unwrap().is_none());
    }
    assert!(matches!(cli(&["web-sessions", "prune"], "").await, Err(CliError::Usage(m)) if m.contains("--config")));
}

// ---- verify and the reference index ----

/// A data directory with a user and the pieces `verify` looks at: one file's manifest of three chunks (one repeated),
/// recorded and stored as an upload does, and a commit that names it. Returns what the tests damage.
struct Seeded {
    dir: std::path::PathBuf,
    config: String,
    db_url: String,
    chunks: Vec<craft_cloud_proto::ChunkId>,
    manifest: craft_cloud_proto::ManifestId,
    meta: Option<craft_cloud_meta::Meta>,
    _keep: tempfile::TempDir,
}

fn chunk_bytes(n: u8) -> Vec<u8> {
    format!("verify chunk {n} {}", uuid::Uuid::now_v7()).into_bytes()
}

async fn seeded() -> Seeded {
    use craft_cloud_proto::{ChunkRef, ChunkerParamsId, FileHash, Manifest, Name, NewVersion, NodeId, Op, VersionId};
    use craft_cloud_store::BlobKey;
    let keep = tempfile::tempdir().unwrap();
    let dir = keep.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let config = Config::load(&config_path).unwrap();
    let (meta, store) = craft_cloud_server::cli::open(&config).await.unwrap();
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let datas = [chunk_bytes(1), chunk_bytes(2)];
    let mut chunks = Vec::new();
    for data in &datas {
        let id = craft_cloud_proto::ChunkId::from_bytes(*blake3::hash(data).as_bytes());
        store.put(BlobKey::chunk(&id), data).await.unwrap();
        meta.add_chunk(ana.space, &id, data.len() as u64 + 1, 0).await.unwrap();
        chunks.push(id);
    }
    // The first chunk twice: [a, b, a].
    let order = [chunks[0], chunks[1], chunks[0]];
    let lens = [datas[0].len() as u32, datas[1].len() as u32, datas[0].len() as u32];
    let refs: Vec<ChunkRef> = order.iter().zip(lens).map(|(id, len)| ChunkRef { id: *id, len }).collect();
    let size = lens.iter().map(|l| u64::from(*l)).sum();
    let manifest = Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), size, FileHash::from_bytes([3; 32]), refs).unwrap();
    store.put(BlobKey::manifest(&manifest.id()), &manifest.encode()).await.unwrap();
    meta.add_manifest(ana.space, &manifest, manifest.encode().len() as u64 + 1, 0).await.unwrap();
    let info = meta.space_info(ana.space).await.unwrap();
    let device = meta.create_device(ana.id, "laptop", &[5u8; 32], 0).await.unwrap();
    let v = NewVersion { id: VersionId::generate(), manifest: manifest.id(), mtime_ms: 1, exec: false, save: None };
    meta.commit(ana.space, device, &[Op::Create { node: NodeId::generate(), parent: info.root, name: Name::new("f.txt").unwrap(), version: v }], 0)
        .await
        .unwrap();
    Seeded {
        db_url: config.database.url.clone(),
        config: config_path.to_str().unwrap().to_owned(),
        dir,
        chunks,
        manifest: manifest.id(),
        meta: Some(meta),
        _keep: keep,
    }
}

async fn raw_sqlite(url: &str) -> sqlx::SqliteConnection {
    use sqlx::Connection;
    sqlx::SqliteConnection::connect(url).await.unwrap()
}

/// The bytes of the database file and of its WAL, read straight from disk without opening SQLite, so that nothing here
/// can checkpoint away a change the command under test made. A WAL that is not there counts as an empty one: opening a
/// WAL-mode database that has no open connection makes an empty `-wal` and a `-shm` file, which changes no data. The
/// `-shm` file is a rebuilt shared-memory index of the WAL, not data, so it is not compared; a change to the data
/// shows up in the database file or in the WAL's bytes.
fn sqlite_snapshot(db_path: &std::path::Path) -> (Vec<u8>, Vec<u8>) {
    let read = |path: std::path::PathBuf| if path.exists() { std::fs::read(path).unwrap() } else { Vec::new() };
    (read(db_path.to_owned()), read(db_path.with_extension("db-wal")))
}

/// Not a test of its own: run as a child of the crashed-WAL tests, it commits a chunk row and ends without closing the
/// database, which leaves the row in the WAL and in no checkpoint.
#[test]
fn verify_test_leave_a_chunk_in_the_wal() {
    let Ok(url) = std::env::var("CRAFT_CLOUD_TEST_WAL_CHUNK_URL") else { return };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let meta = craft_cloud_meta::Meta::open(&url).await.unwrap();
        let ana = meta.user_by_name("ana").await.unwrap().unwrap();
        let id = craft_cloud_proto::ChunkId::from_bytes(*blake3::hash(b"a chunk only the WAL knows").as_bytes());
        meta.add_chunk(ana.space, &id, 9, 0).await.unwrap();
        std::process::exit(0);
    });
}

/// A database a crashed process left with a row only in its WAL: `verify` (a process of its own, finished before
/// anything is compared) reads that row, and the database file, the WAL and the storage tree keep their bytes. With
/// `delete_shm`, the `-shm` file the crash left is gone before the run, in a directory that can be written.
async fn verify_of_a_crashed_wal(delete_shm: bool) {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config = dir.join("config.toml");
    let loaded = Config::load(&config).unwrap();
    let mut conn = raw_sqlite(&loaded.database.url).await;
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&mut conn).await.unwrap();
    sqlx::Connection::close(conn).await.unwrap();
    let status = Process::new(std::env::current_exe().unwrap())
        .args(["--exact", "verify_test_leave_a_chunk_in_the_wal", "--nocapture"])
        .env("CRAFT_CLOUD_TEST_WAL_CHUNK_URL", &loaded.database.url)
        .status()
        .unwrap();
    assert!(status.success());
    let db_path = dir.join("meta.db");
    if delete_shm {
        let _ = std::fs::remove_file(dir.join("meta.db-shm"));
    }
    let (db_before, wal_before) = sqlite_snapshot(&db_path);
    assert!(!wal_before.is_empty(), "test requires a non-empty WAL");
    let tree_before = tree_snapshot(&dir.join("blobs"));

    let (ok, out, err) = run_binary(&["verify", "--json", "--config", config.to_str().unwrap()]);
    assert!(!ok, "the chunk's object is not stored, so verify must find a problem: {out}{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}{err}"));
    assert_eq!(report["problems"]["by_kind"]["chunk_object_missing"], 1, "verify must see the row that only the WAL holds: {out}");
    let (db_after, wal_after) = sqlite_snapshot(&db_path);
    assert!(db_before == db_after, "database file changed: {} -> {} bytes", db_before.len(), db_after.len());
    assert!(wal_before == wal_after, "WAL changed: {} -> {} bytes", wal_before.len(), wal_after.len());
    assert!(tree_before == tree_snapshot(&dir.join("blobs")), "the storage changed");
}

#[tokio::test]
async fn verify_reads_a_crashed_wal_and_changes_no_data() {
    verify_of_a_crashed_wal(false).await;
}

#[tokio::test]
async fn verify_reads_a_crashed_wal_whose_shm_file_is_gone() {
    verify_of_a_crashed_wal(true).await;
}

fn object_path(dir: &std::path::Path, kind: &str, id: &[u8; 32]) -> std::path::PathBuf {
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    dir.join("blobs").join(kind).join(&hex[0..2]).join(&hex[2..4]).join(&hex)
}

async fn verify_json(c: &str, extra: &[&str]) -> (serde_json::Value, bool) {
    let mut args = vec!["verify", "--config", c, "--json"];
    args.extend_from_slice(extra);
    match cli(&args, "").await {
        Ok(out) => (serde_json::from_str(&out).unwrap(), true),
        Err(CliError::Inconsistent(_)) => {
            // The report is written before the error: run it again through `run` for its text.
            let parsed = Cli::try_parse_from(std::iter::once("craft-cloud-server").chain(args.iter().copied())).unwrap();
            let mut out = Vec::new();
            let _ = run(parsed.command, &mut Cursor::new(Vec::new()), &mut out).await;
            (serde_json::from_slice(&out).unwrap(), false)
        }
        Err(other) => panic!("{other}"),
    }
}

#[tokio::test]
async fn verify_passes_on_a_sound_server_and_changes_nothing() {
    let s = seeded().await;
    let before = s.meta.as_ref().unwrap().index_edge_count().await.unwrap();
    let out = cli(&["verify", "--config", &s.config], "").await.unwrap();
    assert!(out.contains("no problems found"), "{out}");
    assert!(out.contains("checked 1 manifest(s) and 2 chunk(s) (chunk objects read and hashed)"), "{out}");
    let (json, ok) = verify_json(&s.config, &[]).await;
    assert!(ok);
    assert_eq!(json["ok"], true);
    assert_eq!(
        (json["index"]["manifests"].as_u64(), json["index"]["complete"].as_u64(), json["index"]["all_complete"].as_bool()),
        (Some(1), Some(1), Some(true))
    );
    assert_eq!(json["problems"]["total"], 0);
    assert_eq!(s.meta.as_ref().unwrap().index_edge_count().await.unwrap(), before);
    assert_eq!(s.meta.as_ref().unwrap().index_edges(&s.manifest).await.unwrap().len(), 3);
}

#[tokio::test]
async fn verify_reports_a_missing_chunk_object_and_a_missing_manifest_object() {
    let s = seeded().await;
    std::fs::remove_file(object_path(&s.dir, "chunks", s.chunks[1].as_bytes())).unwrap();
    let (json, ok) = verify_json(&s.config, &[]).await;
    assert!(!ok);
    assert_eq!(json["problems"]["by_kind"]["chunk_object_missing"], 1, "{json}");
    assert_eq!(json["problems"]["listed"][0]["id"], s.chunks[1].to_string());
    // Without reading chunk objects it is not seen, and the text says so.
    let (json, ok) = verify_json(&s.config, &["--skip-chunk-reads"]).await;
    assert!(ok, "{json}");
    assert_eq!(json["checked"]["chunk_objects_read"], false);
    assert!(cli(&["verify", "--config", &s.config, "--skip-chunk-reads"], "").await.unwrap().contains("chunk objects not read"));
    // A damaged chunk object is reported as damaged, not missing.
    std::fs::write(object_path(&s.dir, "chunks", s.chunks[0].as_bytes()), b"\0not the chunk").unwrap();
    let (json, _) = verify_json(&s.config, &[]).await;
    assert_eq!(json["problems"]["by_kind"]["chunk_object_corrupt"], 1, "{json}");
    // The manifest object is gone: reported (what it names cannot be checked without it).
    std::fs::remove_file(object_path(&s.dir, "manifests", s.manifest.as_bytes())).unwrap();
    let (json, ok) = verify_json(&s.config, &[]).await;
    assert!(!ok);
    assert_eq!(json["problems"]["by_kind"]["manifest_object_missing"], 1, "{json}");
    // Verify never repairs: the same report again.
    let (again, _) = verify_json(&s.config, &[]).await;
    assert_eq!(again["problems"]["by_kind"], json["problems"]["by_kind"]);
}

#[tokio::test]
async fn verify_reports_index_damage_and_unknown_dependencies() {
    let s = seeded().await;
    let mut conn = raw_sqlite(&s.db_url).await;
    // A wrong edge in a complete index, an orphan edge, an index row with no manifest, and a chunk that is not recorded.
    sqlx::query("UPDATE manifest_chunks SET chunk = $1 WHERE manifest = $2 AND ordinal = 1")
        .bind(s.chunks[0].as_bytes().to_vec())
        .bind(s.manifest.as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO manifest_chunks (manifest, ordinal, chunk) VALUES ($1, 0, $2)")
        .bind(vec![0xaa; 32])
        .bind(s.chunks[0].as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO manifest_index (manifest, chunk_count, indexed_ordinals, state) VALUES ($1, 0, 0, 1)")
        .bind(vec![0xbb; 32])
        .execute(&mut conn)
        .await
        .unwrap();
    let (json, ok) = verify_json(&s.config, &["--skip-chunk-reads"]).await;
    assert!(!ok);
    let by = &json["problems"]["by_kind"];
    assert_eq!(
        (by["index_mismatch"].as_u64(), by["orphan_edges"].as_u64(), by["index_without_manifest"].as_u64()),
        (Some(1), Some(1), Some(1)),
        "{json}"
    );
    let mismatch = json["problems"]["listed"].as_array().unwrap().iter().find(|p| p["kind"] == "index_mismatch").unwrap();
    assert_eq!(mismatch["detail"], "ordinal 1: wrong_chunk");
    // A chunk the database does not record: both the manifest walk and the audit say so.
    sqlx::query("DELETE FROM blobs WHERE kind = 0 AND id = $1").bind(s.chunks[0].as_bytes().to_vec()).execute(&mut conn).await.unwrap();
    let (json, _) = verify_json(&s.config, &["--skip-chunk-reads"]).await;
    assert!(json["problems"]["by_kind"]["chunk_not_recorded"].as_u64().unwrap() >= 1, "{json}");
    assert!(json["problems"]["by_kind"]["unknown_dependency"].as_u64().unwrap() >= 1, "{json}");
    // A version that names a manifest that is not recorded.
    sqlx::query("DELETE FROM blobs WHERE kind = 1 AND id = $1").bind(s.manifest.as_bytes().to_vec()).execute(&mut conn).await.unwrap();
    let (json, _) = verify_json(&s.config, &["--skip-chunk-reads"]).await;
    assert_eq!(json["problems"]["by_kind"]["root_manifest_unknown"], 1, "{json}");
    // Nothing was repaired or removed by looking.
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM manifest_chunks").fetch_one(&mut conn).await.unwrap(), 4);
}

#[tokio::test]
async fn an_index_built_after_the_fact_is_exact_and_verify_notices_until_then() {
    let s = seeded().await;
    let mut conn = raw_sqlite(&s.db_url).await;
    // The index as an upgrade finds it: manifests recorded, nothing indexed.
    sqlx::query("DELETE FROM manifest_chunks").execute(&mut conn).await.unwrap();
    sqlx::query("DELETE FROM manifest_index").execute(&mut conn).await.unwrap();
    let (json, ok) = verify_json(&s.config, &[]).await;
    assert!(ok, "an incomplete index is a notice, not a problem: {json}");
    assert_eq!((json["index"]["absent"].as_u64(), json["index"]["all_complete"].as_bool()), (Some(1), Some(false)));
    assert_eq!(json["notices"]["manifests_without_complete_index"], 1);
    assert!(cli(&["verify", "--config", &s.config], "").await.unwrap().contains("index build"));

    // A build in batches of two edges, then it is complete, exact, and verify says so.
    let out = cli(&["index", "build", "--config", &s.config, "--batch-rows", "2"], "").await.unwrap();
    assert!(out.contains("indexed 1 manifest(s)"), "{out}");
    assert_eq!(
        s.meta.as_ref().unwrap().index_edges(&s.manifest).await.unwrap().iter().map(|(_, c)| *c).collect::<Vec<_>>(),
        vec![s.chunks[0], s.chunks[1], s.chunks[0]]
    );
    let (json, ok) = verify_json(&s.config, &[]).await;
    assert!(ok);
    assert_eq!(json["index"]["all_complete"], true);
    // Again changes nothing.
    assert!(cli(&["index", "build", "--config", &s.config], "").await.unwrap().contains("indexed 0 manifest(s)"));
    assert!(matches!(cli(&["index", "build", "--config", &s.config, "--batch-rows", "0"], "").await, Err(CliError::Usage(_))));

    // A manifest whose object cannot be read is reported and stays unindexed: nothing is guessed from the database.
    sqlx::query("DELETE FROM manifest_chunks").execute(&mut conn).await.unwrap();
    sqlx::query("DELETE FROM manifest_index").execute(&mut conn).await.unwrap();
    std::fs::remove_file(object_path(&s.dir, "manifests", s.manifest.as_bytes())).unwrap();
    let built = cli(&["index", "build", "--config", &s.config], "").await;
    assert!(matches!(built, Err(CliError::Inconsistent(_))), "{built:?}");
    assert_eq!(s.meta.as_ref().unwrap().index_state(&s.manifest).await.unwrap(), craft_cloud_meta::IndexState::Absent);
    assert!(s.meta.as_ref().unwrap().index_edges(&s.manifest).await.unwrap().is_empty());
}

#[tokio::test]
async fn verify_needs_a_config() {
    assert!(matches!(cli(&["verify"], "").await, Err(CliError::Usage(m)) if m.contains("--config")));
}

/// The config of `s` with its database url and its storage path replaced where given.
fn config_with(s: &Seeded, database: Option<&std::path::Path>, storage: Option<&std::path::Path>) -> String {
    let text = std::fs::read_to_string(&s.config).unwrap();
    let slashed = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
    let lines: Vec<String> = text
        .lines()
        .map(|line| match (line.trim_start(), database, storage) {
            (l, Some(db), _) if l.starts_with("url =") => format!("url = \"sqlite:{}\"", slashed(db)),
            (l, _, Some(path)) if l.starts_with("path =") => format!("path = \"{}\"", slashed(path)),
            _ => line.to_owned(),
        })
        .collect();
    let target = s.dir.join("wrong.toml");
    std::fs::write(&target, lines.join("\n")).unwrap();
    target.to_str().unwrap().to_owned()
}

#[tokio::test]
async fn verify_of_a_wrong_database_path_is_an_error_and_creates_nothing() {
    let s = seeded().await;
    let nowhere = s.dir.join("nowhere");
    let missing_db = nowhere.join("meta.db");
    let config = config_with(&s, Some(&missing_db), None);
    let err = cli(&["verify", "--config", &config], "").await.expect_err("a database that is not there must not verify");
    assert!(!matches!(err, CliError::Inconsistent(_)), "an error, not a report: {err}");
    assert!(!missing_db.exists() && !nowhere.exists(), "nothing is created");
    // The same in a folder that exists: no new empty database appears in it.
    let existing_dir_db = s.dir.join("other.db");
    let config = config_with(&s, Some(&existing_dir_db), None);
    assert!(cli(&["verify", "--config", &config, "--json"], "").await.is_err());
    assert!(!existing_dir_db.exists(), "no empty database was created");
    // A file that is not a Craft Cloud database is refused too.
    let empty = s.dir.join("empty.db");
    std::fs::write(&empty, b"").unwrap();
    let config = config_with(&s, Some(&empty), None);
    assert!(cli(&["verify", "--config", &config], "").await.is_err());
    assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0, "and left as it was");
}

#[tokio::test]
async fn verify_refuses_older_and_newer_schemas_without_changing_the_database() {
    let mut s = seeded().await;
    let db_path = std::path::PathBuf::from(s.db_url.strip_prefix("sqlite:").unwrap().split('?').next().unwrap());
    let mut conn = raw_sqlite(&s.db_url).await;
    let current: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_version").fetch_one(&mut conn).await.unwrap();
    use sqlx::Connection;
    conn.close().await.unwrap();
    drop(s.meta.take());
    for (version, message) in [(current - 1, "start the server once"), (current + 1, "newer Craft Cloud")] {
        let mut conn = raw_sqlite(&s.db_url).await;
        sqlx::query("DELETE FROM schema_version").execute(&mut conn).await.unwrap();
        sqlx::query("INSERT INTO schema_version (version) VALUES ($1)").bind(version).execute(&mut conn).await.unwrap();
        // Checkpoint during setup only, then close every connection, so the snapshots below are taken of files at rest.
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&mut conn).await.unwrap();
        conn.close().await.unwrap();
        let before = sqlite_snapshot(&db_path);
        let (ok, _, error) = run_binary(&["verify", "--config", &s.config]);
        assert!(!ok && error.contains(message), "verify must refuse a non-current schema: {error}");
        assert!(sqlite_snapshot(&db_path) == before, "verify changed the database or its WAL for schema version {version}");
    }
}

#[tokio::test]
async fn verify_of_a_wrong_storage_path_is_an_error_and_creates_nothing() {
    let s = seeded().await;
    let missing = s.dir.join("no-blobs-here");
    let config = config_with(&s, None, Some(&missing));
    let err = cli(&["verify", "--config", &config], "").await.expect_err("storage that is not there must not verify");
    assert!(matches!(err, CliError::Usage(_)), "{err}");
    assert!(!missing.exists(), "the storage directory is not created");
}

#[tokio::test]
async fn verify_reports_sizes_the_database_records_wrongly() {
    let s = seeded().await;
    let mut conn = raw_sqlite(&s.db_url).await;
    let (a, b) = (s.chunks[0], s.chunks[1]);
    // A chunk recorded as longer than the manifest says (the manifest lists it twice), and a chunk whose stored length
    // differs from the object's.
    sqlx::query("UPDATE blobs SET stored_size = stored_size + 7 WHERE kind = 0 AND id = $1")
        .bind(a.as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("UPDATE blobs SET stored_size = stored_size - 1 WHERE kind = 0 AND id = $1")
        .bind(b.as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    // The manifest's stored size and its file's size.
    sqlx::query("UPDATE blobs SET stored_size = stored_size + 3, file_size = file_size + 1 WHERE kind = 1 AND id = $1")
        .bind(s.manifest.as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    // Without reading the chunk objects the database's own sizes are checked against the manifests.
    let (json, ok) = verify_json(&s.config, &["--skip-chunk-reads"]).await;
    assert!(!ok, "{json}");
    let by = &json["problems"]["by_kind"];
    assert_eq!(by["chunk_length_mismatch"], 3, "{json}");
    assert_eq!(by["manifest_stored_size_mismatch"], 1, "{json}");
    assert_eq!(by["manifest_file_size_mismatch"], 1, "{json}");
    assert!(by["chunk_stored_size_mismatch"].is_null(), "the objects were not read: {json}");
    // Reading them also compares the stored lengths with the objects.
    let (json, ok) = verify_json(&s.config, &[]).await;
    assert!(!ok);
    assert_eq!(json["problems"]["by_kind"]["chunk_stored_size_mismatch"], 2, "{json}");
    assert_eq!(json["problems"]["by_kind"]["chunk_length_mismatch"], 3, "{json}");
    // Looking repairs nothing.
    let (again, _) = verify_json(&s.config, &[]).await;
    assert_eq!(again["problems"]["by_kind"], json["problems"]["by_kind"]);
}

#[tokio::test]
async fn space_commands_create_list_add_archive_and_reset_one_epoch() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    cli(&["user", "--config", c, "add", "bob"], "bob password 1\n").await.unwrap();
    cli(&["user", "--config", c, "add", "cy"], "cy password 111\n").await.unwrap();

    // Create: the owner is a member from the start; a name is unique in any case; an unknown owner and a bad name are usage errors.
    let out = cli(&["space", "--config", c, "create", "Straße", "--owner", "bob"], "").await.unwrap();
    assert!(out.contains("owned by bob"), "{out}");
    assert!(matches!(cli(&["space", "--config", c, "create", "STRASSE", "--owner", "bob"], "").await, Err(CliError::Meta(_))));
    assert!(matches!(cli(&["space", "--config", c, "create", "Other", "--owner", "nobody"], "").await, Err(CliError::Usage(_))));
    assert!(matches!(cli(&["space", "--config", c, "create", "  ", "--owner", "bob"], "").await, Err(CliError::Usage(_))));

    // List and members, by name in any case.
    let list = cli(&["space", "--config", c, "list"], "").await.unwrap();
    assert!(list.contains("1 member(s), 1 owner(s)") && list.trim_end().ends_with("Straße"), "{list}");
    let members = cli(&["space", "--config", c, "members", "strasse"], "").await.unwrap();
    assert_eq!(members, "bob  owner  member\n");
    assert!(matches!(cli(&["space", "--config", c, "members", "nothing here"], "").await, Err(CliError::Usage(_))));

    // Add: accepted at once; adding again sets the role.
    cli(&["space", "--config", c, "add", "Straße", "cy", "--role", "viewer"], "").await.unwrap();
    assert_eq!(cli(&["space", "--config", c, "members", "Straße"], "").await.unwrap(), "bob  owner  member\ncy  viewer  member\n");
    cli(&["space", "--config", c, "add", "Straße", "cy", "--role", "editor"], "").await.unwrap();
    assert!(cli(&["space", "--config", c, "members", "Straße"], "").await.unwrap().contains("cy  editor  member"));
    assert!(matches!(cli(&["space", "--config", c, "add", "Straße", "cy", "--role", "boss"], "").await, Err(CliError::Usage(_))));
    assert!(matches!(cli(&["space", "--config", c, "add", "Straße", "nobody"], "").await, Err(CliError::Usage(_))));

    // Archive: marked in the list, a second call changes nothing.
    cli(&["space", "--config", c, "archive", "Straße"], "").await.unwrap();
    cli(&["space", "--config", c, "archive", "Straße"], "").await.unwrap();
    assert!(cli(&["space", "--config", c, "list"], "").await.unwrap().contains("(archived)"));

    // reset-epoch --space resets that space only, and is an exclusive target.
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let name = craft_cloud_proto::SpaceName::new("Straße").unwrap();
    let space = meta.team_space_by_name(&name).await.unwrap().unwrap();
    let others: Vec<_> = meta.all_spaces().await.unwrap().into_iter().filter(|s| *s != space).collect();
    let mut before = vec![meta.space_info(space).await.unwrap().epoch];
    for other in &others {
        before.push(meta.space_info(*other).await.unwrap().epoch);
    }
    let out = cli(&["reset-epoch", "--config", c, "--space", &space.to_string()], "").await.unwrap();
    assert!(out.contains(&format!("for space {space}")), "{out}");
    assert_ne!(meta.space_info(space).await.unwrap().epoch, before[0]);
    for (other, old) in others.iter().zip(&before[1..]) {
        assert_eq!(meta.space_info(*other).await.unwrap().epoch, *old, "another space kept its epoch");
    }
    assert!(matches!(
        cli(&["reset-epoch", "--config", c, "--space", &craft_cloud_proto::SpaceId::generate().to_string()], "").await,
        Err(CliError::Meta(_))
    ));
    assert!(Cli::try_parse_from(["craft-cloud-server", "reset-epoch", "--config", c, "--space", &space.to_string(), "--all"]).is_err());

    // The events, once each, by the command line: created, two member changes (add + role change), archived once.
    let audit = cli(&["audit", "--config", c, "list", "--action", "cli."], "").await.unwrap();
    assert_eq!(audit.matches("cli.space_created").count(), 1, "{audit}");
    assert_eq!(audit.matches("cli.space_member_changed").count(), 2, "{audit}");
    assert_eq!(audit.matches("cli.space_archived").count(), 1, "{audit}");
}

/// `backup` and `restore`, against a directory standing in for the backup server, and (with the `sftp` feature and the
/// `CRAFT_CLOUD_TEST_SFTP_*` variables of the store's conformance tests) against a real SFTP server.
#[cfg(feature = "sftp")]
mod backup {
    use std::sync::Arc;

    use bytes::Bytes;
    use craft_cloud_server::backup::{Remote, backup_to, restore_from};
    use craft_cloud_store::disk::DiskBackend;
    use craft_cloud_store::sftp::{SftpBackend, SftpConfig};
    use craft_cloud_store::{Backend, StoreError};

    use super::*;

    /// A directory on this machine that plays the backup server.
    #[derive(Clone)]
    struct DirRemote {
        backend: Arc<DiskBackend>,
        root: PathBuf,
        /// Reading a chunk fails, as when the connection dies in the middle of a restore.
        fail_chunk_reads: bool,
    }

    impl DirRemote {
        fn new(root: &Path) -> Self {
            Self { backend: Arc::new(DiskBackend::new(root).unwrap()), root: root.to_owned(), fail_chunk_reads: false }
        }

        fn failing_on_chunks(&self) -> Self {
            Self { fail_chunk_reads: true, ..self.clone() }
        }
    }

    #[async_trait::async_trait]
    impl Backend for DirRemote {
        fn kind(&self) -> &'static str {
            "other"
        }
        async fn put_raw(&self, path: &str, data: Bytes) -> Result<(), StoreError> {
            self.backend.put_raw(path, data).await
        }
        async fn get_raw(&self, path: &str) -> Result<Option<Bytes>, StoreError> {
            self.backend.get_raw(path).await
        }
        async fn get_raw_limited(&self, path: &str, max_bytes: usize) -> Result<Option<Bytes>, StoreError> {
            if self.fail_chunk_reads && path.starts_with("chunks/") {
                return Err(StoreError::Backend("the connection was lost".into()));
            }
            self.backend.get_raw_limited(path, max_bytes).await
        }
        async fn size_raw(&self, path: &str) -> Result<Option<u64>, StoreError> {
            self.backend.size_raw(path).await
        }
        async fn delete_raw(&self, path: &str) -> Result<(), StoreError> {
            self.backend.delete_raw(path).await
        }
        async fn list_raw(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>, StoreError> {
            self.backend.list_raw(prefix, after, limit).await
        }
    }

    impl Remote for DirRemote {
        async fn put_file(&self, path: &str, local: &Path) -> Result<(), StoreError> {
            let target = self.root.join(path);
            std::fs::create_dir_all(target.parent().unwrap())?;
            std::fs::copy(local, target)?;
            Ok(())
        }
        async fn get_file(&self, path: &str, local: &Path, max_bytes: u64) -> Result<bool, StoreError> {
            match std::fs::metadata(self.root.join(path)) {
                Ok(meta) if meta.len() > max_bytes => return Err(StoreError::Backend("the object is more than expected".into())),
                _ => {}
            }
            match std::fs::copy(self.root.join(path), local) {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e.into()),
            }
        }
        async fn remove_dir(&self, path: &str) -> Result<(), StoreError> {
            let _ = std::fs::remove_dir(self.root.join(path));
            Ok(())
        }
    }

    const T0: i64 = 1_700_000_000_000;

    /// A config for the data directory a restore made, as the admin would write it.
    fn config_for(dir: &Path) -> String {
        let slashes = |p: PathBuf| p.to_string_lossy().replace('\\', "/");
        let path = dir.join("config.toml");
        let text = format!(
            "listen = \"127.0.0.1:8080\"\n[database]\nurl = \"sqlite:{}\"\n[storage]\nkind = \"disk\"\npath = \"{}\"\n",
            slashes(dir.join("meta.db")),
            slashes(dir.join("blobs"))
        );
        std::fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_owned()
    }

    fn generations(remote: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(remote.join("generations"))
            .map(|d| d.map(|e| e.unwrap().file_name().into_string().unwrap()).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The second each generation began in, oldest first (a generation is named `<second>-<id>`).
    fn stamps(remote: &Path) -> Vec<String> {
        generations(remote).iter().map(|name| name[..16].to_owned()).collect()
    }

    /// The directory of the one generation that began in `second`.
    fn generation_dir(remote: &Path, second: &str) -> PathBuf {
        let found: Vec<String> = generations(remote).into_iter().filter(|name| name.starts_with(second)).collect();
        assert_eq!(found.len(), 1, "{second}: {found:?}");
        remote.join("generations").join(&found[0])
    }

    /// Backs the seeded server up twice, restores into an empty directory, and checks the result with `verify`.
    async fn backup_restore_verify<R: Remote>(remote: R) {
        let s = seeded().await;
        let config = Config::load(Path::new(&s.config)).unwrap();
        let mut out = Vec::new();
        backup_to(&config, remote.clone(), 7, None, T0, &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("backup 20231114T221320Z-"), "{text}");
        assert!(text.contains(" is complete"), "{text}");
        assert!(text.contains("chunks: 2 found, 2 copied"), "{text}");
        assert!(text.contains("manifests: 1 found, 1 copied"), "{text}");

        // A second backup sends no blob again.
        let mut out = Vec::new();
        backup_to(&config, remote.clone(), 7, None, T0 + 60_000, &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("chunks: 2 found, 0 copied (0 bytes), 2 already there"), "{text}");
        assert!(text.contains("backup 20231114T221420Z-"), "{text}");

        // A restore refuses a directory with something in it, and changes nothing there.
        let before = tree_snapshot(&s.dir);
        let mut out = Vec::new();
        let refused = restore_from(remote.clone(), &s.dir, None, &mut out).await;
        assert!(matches!(&refused, Err(CliError::Usage(m)) if m.contains("is not empty")), "{refused:?}");
        assert_eq!(tree_snapshot(&s.dir), before);

        let parent = tempfile::tempdir().unwrap();
        let restored = parent.path().join("restored");
        let mut out = Vec::new();
        restore_from(remote.clone(), &restored, None, &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("restored generation 20231114T221420Z-"), "{text}");
        assert!(text.contains("reset-epoch --all"), "{text}");

        let restored_config = config_for(&restored);
        let verified = cli(&["verify", "--config", &restored_config], "").await.unwrap();
        assert!(verified.contains("no problems found"), "{verified}");
        assert!(verified.contains("checked 1 manifest(s) and 2 chunk(s)"), "{verified}");
        let config = Config::load(Path::new(&restored_config)).unwrap();
        let meta = craft_cloud_meta::Meta::open_existing(&config.database.url).await.unwrap();
        assert!(meta.user_by_name("ana").await.unwrap().is_some());
        // Restoring into the directory it made is refused too.
        let mut out = Vec::new();
        assert!(matches!(restore_from(remote, &restored, None, &mut out).await, Err(CliError::Usage(_))));
    }

    #[tokio::test]
    async fn a_backup_restores_into_an_empty_directory_and_verifies() {
        let parent = tempfile::tempdir().unwrap();
        backup_restore_verify(DirRemote::new(&parent.path().join("remote"))).await;
    }

    #[tokio::test]
    async fn old_database_generations_are_pruned_and_blobs_are_kept() {
        let s = seeded().await;
        let config = Config::load(Path::new(&s.config)).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("remote");
        let remote = DirRemote::new(&root);
        for n in 0..4 {
            backup_to(&config, remote.clone(), 2, None, T0 + n * 60_000, &mut Vec::new()).await.unwrap();
        }
        assert_eq!(stamps(&root), ["20231114T221520Z", "20231114T221620Z"].map(String::from));
        for name in generations(&root) {
            assert!(root.join("generations").join(name).join("manifest.json").is_file());
        }
        let hex: String = s.chunks[0].as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert!(root.join("chunks").join(&hex[0..2]).join(&hex[2..4]).join(&hex).is_file(), "blobs stay when old generations go");
        // A named generation can be restored; one that is gone cannot.
        let restored = parent.path().join("older");
        restore_from(remote.clone(), &restored, Some("20231114T221520Z"), &mut Vec::new()).await.unwrap();
        let missing = restore_from(remote, &parent.path().join("missing"), Some("20231114T221320Z"), &mut Vec::new()).await;
        assert!(matches!(&missing, Err(CliError::Usage(m)) if m.contains("no complete generation")), "{missing:?}");
    }

    #[tokio::test]
    async fn a_generation_without_its_manifest_is_not_offered_and_a_damaged_dump_is_refused() {
        let s = seeded().await;
        let config = Config::load(Path::new(&s.config)).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("remote");
        let remote = DirRemote::new(&root);
        backup_to(&config, remote.clone(), 7, None, T0, &mut Vec::new()).await.unwrap();
        // A newer upload that never finished: a dump and no manifest.
        let partial = root.join("generations/20231114T230000Z");
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join("meta.sqlite"), b"half").unwrap();
        let restored = parent.path().join("restored");
        restore_from(remote.clone(), &restored, None, &mut Vec::new()).await.unwrap();

        // One flipped byte in the dump is caught by the manifest's hash, and the half-made directory is removed.
        let dump = generation_dir(&root, "20231114T221320Z").join("meta.sqlite");
        let mut bytes = std::fs::read(&dump).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&dump, bytes).unwrap();
        let damaged = parent.path().join("damaged");
        let refused = restore_from(remote, &damaged, None, &mut Vec::new()).await;
        assert!(matches!(&refused, Err(CliError::Inconsistent(m)) if m.contains("damaged")), "{refused:?}");
        assert!(!damaged.exists(), "a failed restore leaves nothing behind");
    }

    #[tokio::test]
    async fn a_restore_that_dies_leaves_the_target_as_it_was_and_no_half_made_directory() {
        let s = seeded().await;
        let config = Config::load(Path::new(&s.config)).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("remote");
        let remote = DirRemote::new(&root);
        backup_to(&config, remote.clone(), 7, None, T0, &mut Vec::new()).await.unwrap();
        let names = |dir: &Path| -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            names.sort();
            names
        };
        let before = names(parent.path());
        // The database arrives, then the connection dies on the first chunk: the target must not hold a database with
        // part of its blobs, which a re-run would then refuse as "not empty".
        let target = parent.path().join("restored");
        let died = restore_from(remote.failing_on_chunks(), &target, None, &mut Vec::new()).await;
        assert!(died.is_err());
        assert!(!target.exists(), "nothing is at the target");
        assert_eq!(names(parent.path()), before, "and no staging directory is left behind");
        // The same target restores fine afterwards, and so does an empty directory that is already there.
        restore_from(remote.clone(), &target, None, &mut Vec::new()).await.unwrap();
        assert!(target.join("meta.db").is_file() && target.join("blobs").is_dir());
        assert!(!names(parent.path()).iter().any(|n| n.contains(".restore-")));
        let empty = parent.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        restore_from(remote, &empty, None, &mut Vec::new()).await.unwrap();
        assert!(empty.join("meta.db").is_file());
        let verified = cli(&["verify", "--config", &config_for(&empty)], "").await.unwrap();
        assert!(verified.contains("no problems found"), "{verified}");
    }

    #[tokio::test]
    async fn a_restore_refuses_a_dump_bigger_than_its_manifest_says() {
        let s = seeded().await;
        let config = Config::load(Path::new(&s.config)).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("remote");
        let remote = DirRemote::new(&root);
        backup_to(&config, remote.clone(), 7, None, T0, &mut Vec::new()).await.unwrap();
        // The backup server now holds a bigger dump than the manifest recorded (or lies about its size).
        let dump = generation_dir(&root, "20231114T221320Z").join("meta.sqlite");
        let mut bytes = std::fs::read(&dump).unwrap();
        bytes.extend_from_slice(&[0u8; 4096]);
        std::fs::write(&dump, bytes).unwrap();
        let target = parent.path().join("restored");
        let refused = restore_from(remote, &target, None, &mut Vec::new()).await;
        assert!(matches!(&refused, Err(CliError::Store(e)) if e.to_string().contains("more than")), "{refused:?}");
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn backups_in_the_same_second_get_generations_of_their_own_and_none_prunes_the_other() {
        let s = seeded().await;
        let config = Config::load(Path::new(&s.config)).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("remote");
        let remote = DirRemote::new(&root);
        // Two backups that run at once, in the same second, with keep = 1 (the case where one would prune the other).
        let (mut out_a, mut out_b) = (Vec::new(), Vec::new());
        let (first, second) =
            tokio::join!(backup_to(&config, remote.clone(), 1, None, T0, &mut out_a), backup_to(&config, remote.clone(), 1, None, T0, &mut out_b));
        first.unwrap();
        second.unwrap();
        let names = generations(&root);
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|n| n.starts_with("20231114T221320Z-") && n.len() == 49), "{names:?}");
        // Each is whole: its manifest names its own dump, whose hash is the manifest's.
        for name in &names {
            let dir = root.join("generations").join(name);
            let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
            assert_eq!(manifest["database"]["file"], format!("generations/{name}/meta.sqlite"));
            let dump = std::fs::read(dir.join("meta.sqlite")).unwrap();
            assert_eq!(manifest["database"]["blake3"], blake3::hash(&dump).to_hex().to_string());
            assert_eq!(manifest["database"]["bytes"], dump.len() as u64);
        }
        // The same second again, later: still a new generation, and the other ones that began in that second stay.
        backup_to(&config, remote.clone(), 1, None, T0, &mut Vec::new()).await.unwrap();
        assert_eq!(generations(&root).len(), 3);
        restore_from(remote.clone(), &parent.path().join("restored"), None, &mut Vec::new()).await.unwrap();
        // A newer backup, then one whose clock is behind it, with keep = 1: the one just written stays (and so does the
        // newest), because a clock that went back must not make a backup delete itself.
        backup_to(&config, remote.clone(), 1, None, T0 + 120_000, &mut Vec::new()).await.unwrap();
        assert_eq!(stamps(&root), ["20231114T221520Z"].map(String::from));
        backup_to(&config, remote.clone(), 1, None, T0 + 60_000, &mut Vec::new()).await.unwrap();
        assert_eq!(stamps(&root), ["20231114T221420Z", "20231114T221520Z"].map(String::from));
        assert!(generation_dir(&root, "20231114T221420Z").join("manifest.json").is_file());
        // A generation of the first version's name (no id) is still read, and a named one restores by its second.
        let old = root.join("generations/20231114T221000Z");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::copy(generation_dir(&root, "20231114T221520Z").join("meta.sqlite"), old.join("meta.sqlite")).unwrap();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(generation_dir(&root, "20231114T221520Z").join("manifest.json")).unwrap()).unwrap();
        manifest["database"]["file"] = "generations/20231114T221000Z/meta.sqlite".into();
        std::fs::write(old.join("manifest.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();
        restore_from(remote.clone(), &parent.path().join("old"), Some("20231114T221000Z"), &mut Vec::new()).await.unwrap();
        // A generation without its manifest is a backup that may be running: left alone for a day, then taken for dead.
        let running = root.join("generations/20231114T220000Z");
        std::fs::create_dir_all(&running).unwrap();
        std::fs::write(running.join("meta.sqlite"), b"half").unwrap();
        backup_to(&config, remote.clone(), 1, None, T0 + 180_000, &mut Vec::new()).await.unwrap();
        assert!(running.exists(), "a young incomplete generation is not pruned");
        backup_to(&config, remote, 1, None, T0 + 2 * 86_400_000, &mut Vec::new()).await.unwrap();
        assert!(!running.exists(), "an old incomplete generation is");
        assert_eq!(generations(&root).len(), 1);
    }

    #[tokio::test]
    async fn commands_that_only_use_the_database_do_not_reach_for_the_blob_store() {
        // The blobs are on a server that does not answer; adding a user must not need it.
        let dir = tempfile::tempdir().unwrap();
        let slashes = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "listen = \"127.0.0.1:8080\"\n[database]\nurl = \"sqlite:{}\"\n[storage]\nkind = \"sftp\"\nsftp_host = \"127.0.0.1\"\nsftp_port = 1\n\
                 sftp_user = \"u\"\nsftp_root = \"/x\"\nsftp_key_file = \"{}\"\nsftp_host_key = \"SHA256:abc\"\nsftp_timeout_secs = 2\nsftp_retries = 0\n",
                slashes(&dir.path().join("meta.db")),
                slashes(&dir.path().join("key"))
            ),
        )
        .unwrap();
        let config = config.to_str().unwrap().to_owned();
        let added = cli(&["user", "add", "bob", "--config", &config], "a long enough password\n").await.unwrap();
        assert!(added.contains("added bob"), "{added}");
        assert!(cli(&["user", "invite", "bob", "--config", &config], "").await.is_ok());
        assert!(cli(&["device", "list", "bob", "--config", &config], "").await.is_ok());
        // What does need the blobs says why it cannot have them.
        assert!(cli(&["store", "measure", "--config", &config], "").await.is_err());
    }

    #[tokio::test]
    async fn postgres_is_pointed_at_pg_dump() {
        let s = seeded().await;
        let mut text = std::fs::read_to_string(&s.config).unwrap();
        config_with_database_url(&mut text, "postgres://user:pw@localhost/db");
        let config = Config::parse(&text).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let refused = backup_to(&config, DirRemote::new(&parent.path().join("remote")), 7, None, T0, &mut Vec::new()).await;
        assert!(matches!(&refused, Err(CliError::Usage(m)) if m.contains("pg_dump")), "{refused:?}");
    }

    #[tokio::test]
    async fn the_commands_need_a_pinned_host_key_and_a_key() {
        let s = seeded().await;
        let key = tempfile::NamedTempFile::new().unwrap();
        let key = key.path().to_str().unwrap().to_owned();
        let unpinned = cli(&["backup", "--config", &s.config, "--to", "sftp://u@127.0.0.1:1/x", "--key-file", &key], "").await;
        assert!(matches!(&unpinned, Err(CliError::Usage(m)) if m.contains("host key must be pinned")), "{unpinned:?}");
        let password =
            cli(&["backup", "--config", &s.config, "--to", "sftp://u:secret@127.0.0.1:1/x", "--key-file", &key, "--host-key", "SHA256:x"], "").await;
        match &password {
            Err(CliError::Usage(m)) => assert!(m.contains("password") && !m.contains("secret"), "{m}"),
            other => panic!("{other:?}"),
        }
        let dir = tempfile::tempdir().unwrap();
        let from =
            cli(&["restore", "--from", "sftp://u@127.0.0.1:1/x", "--dir", dir.path().join("d").to_str().unwrap(), "--key-file", &key], "").await;
        assert!(matches!(&from, Err(CliError::Usage(m)) if m.contains("host key must be pinned")), "{from:?}");
    }

    fn sftp_settings() -> Option<SftpConfig> {
        let Ok(host) = std::env::var("CRAFT_CLOUD_TEST_SFTP_HOST") else {
            assert!(
                std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_none(),
                "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_SFTP_HOST is not"
            );
            eprintln!("skipped: CRAFT_CLOUD_TEST_SFTP_HOST not set");
            return None;
        };
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set with CRAFT_CLOUD_TEST_SFTP_HOST"));
        Some(SftpConfig {
            host,
            port: var("CRAFT_CLOUD_TEST_SFTP_PORT").parse().unwrap(),
            user: var("CRAFT_CLOUD_TEST_SFTP_USER"),
            root: format!("{}/backup-{}", var("CRAFT_CLOUD_TEST_SFTP_ROOT").trim_end_matches('/'), uuid::Uuid::now_v7()),
            key_file: var("CRAFT_CLOUD_TEST_SFTP_KEY_FILE").into(),
            key_passphrase: None,
            host_key: Some(var("CRAFT_CLOUD_TEST_SFTP_HOST_KEY")),
            known_hosts: None,
            connections: 4,
            timeout: Duration::from_secs(20),
            retries: 2,
        })
    }

    #[tokio::test]
    async fn a_backup_to_an_sftp_server_restores_and_verifies() {
        let Some(settings) = sftp_settings() else { return };
        backup_restore_verify(SftpBackend::connect_for_backup(&settings).await.unwrap()).await;
    }

    /// `storage.kind = "sftp"` through the config, as `serve` and the commands open it.
    #[tokio::test]
    async fn the_sftp_storage_kind_opens_through_the_config() {
        let Some(settings) = sftp_settings() else { return };
        let dir = tempfile::tempdir().unwrap();
        let slashes = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let text = |root: &str| {
            format!(
                "listen = \"127.0.0.1:8080\"\n[database]\nurl = \"sqlite:{}\"\n[storage]\nkind = \"sftp\"\nsftp_host = \"{}\"\nsftp_port = {}\n\
                 sftp_user = \"{}\"\nsftp_root = \"{root}\"\nsftp_key_file = \"{}\"\nsftp_host_key = \"{}\"\n",
                slashes(&dir.path().join("meta.db")),
                settings.host,
                settings.port,
                settings.user,
                slashes(&settings.key_file),
                settings.host_key.as_deref().unwrap()
            )
        };
        let config = Config::parse(&text(&settings.root)).unwrap();
        let (_meta, store) = craft_cloud_server::cli::open(&config).await.unwrap();
        assert_eq!(store.kind(), "sftp");
        let data = format!("through the config {}", uuid::Uuid::now_v7()).into_bytes();
        let id = craft_cloud_proto::ChunkId::from_bytes(*blake3::hash(&data).as_bytes());
        store.put(BlobKey::chunk(&id), &data).await.unwrap();
        let (_meta, again) = craft_cloud_server::cli::open_existing(&config).await.unwrap();
        assert_eq!(&again.get(BlobKey::chunk(&id)).await.unwrap()[..], &data[..]);
        // A command that only reads does not make a root directory that is missing: it says so.
        let other = Config::parse(&text(&format!("{}-missing", settings.root))).unwrap();
        let refused = craft_cloud_server::cli::open_existing(&other).await;
        assert!(matches!(&refused, Err(CliError::Store(e)) if e.to_string().contains("does not exist")), "{:?}", refused.err());
        assert!(craft_cloud_server::cli::open_existing(&other).await.is_err(), "and still does not");
        // `store clean-temp` removes the temporary file of an upload that was cut off a few days ago, and only that.
        let config_file = dir.path().join("config.toml");
        std::fs::write(&config_file, text(&settings.root)).unwrap();
        let path = config_file.to_str().unwrap();
        let nothing = cli(&["store", "clean-temp", "--config", path], "").await.unwrap();
        assert!(nothing.contains("removed 0 temporary file(s)"), "{nothing}");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let stamp = |secs: u64| uuid::Uuid::new_v7(uuid::Timestamp::from_unix(uuid::ContextV7::new(), secs, 0)).hyphenated().to_string();
        let backend = SftpBackend::connect(&settings).await.unwrap();
        let (old, young) = (format!("chunks/ab/cd/.tmp-{}", stamp(now - 3 * 86_400)), format!("chunks/ab/cd/.tmp-{}", stamp(now)));
        for planted in [&old, &young] {
            backend.put_raw(planted, Bytes::from_static(b"cut off")).await.unwrap();
        }
        let swept = cli(&["store", "clean-temp", "--config", path], "").await.unwrap();
        assert!(swept.contains("removed 1 temporary file(s)"), "{swept}");
        assert_eq!(backend.size_raw(&old).await.unwrap(), None);
        assert_eq!(backend.size_raw(&young).await.unwrap(), Some(7));
        assert!(cli(&["store", "clean-temp", "--older-than-hours", "0", "--config", path], "").await.is_err());
    }

    /// The same through the command line: the address, the flags, and a backup that is repeated.
    #[tokio::test]
    async fn the_backup_and_restore_commands_work_against_an_sftp_server() {
        let Some(settings) = sftp_settings() else { return };
        let s = seeded().await;
        let url = format!(
            "sftp://{}@{}:{}{}",
            settings.user,
            settings.host,
            settings.port,
            if settings.root.starts_with('/') { settings.root.clone() } else { format!("/~/{}", settings.root) }
        );
        let key = settings.key_file.to_str().unwrap().to_owned();
        let pin = settings.host_key.clone().unwrap();
        let backup = |keep: &str| {
            let args =
                ["backup", "--config", &s.config, "--to", &url, "--key-file", &key, "--host-key", &pin, "--keep", keep].map(String::from).to_vec();
            async move {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                cli(&args, "").await
            }
        };
        let first = backup("3").await.unwrap();
        assert!(first.contains("chunks: 2 found, 2 copied") && first.contains("is complete"), "{first}");
        let second = backup("3").await.unwrap();
        assert!(second.contains("chunks: 2 found, 0 copied"), "{second}");
        let parent = tempfile::tempdir().unwrap();
        let restored = parent.path().join("restored");
        let out = cli(&["restore", "--from", &url, "--dir", restored.to_str().unwrap(), "--key-file", &key, "--host-key", &pin], "").await.unwrap();
        assert!(out.contains("restored generation"), "{out}");
        let verified = cli(&["verify", "--config", &config_for(&restored)], "").await.unwrap();
        assert!(verified.contains("no problems found"), "{verified}");
        // A wrong pin is refused and nothing is restored.
        let other = parent.path().join("other");
        let wrong = cli(
            &[
                "restore",
                "--from",
                &url,
                "--dir",
                other.to_str().unwrap(),
                "--key-file",
                &key,
                "--host-key",
                "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            ],
            "",
        )
        .await;
        assert!(wrong.is_err());
        assert!(!other.exists());
    }
}
