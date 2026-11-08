//! `store recompress`, `store estimate` and `store measure`, and the physical sizes they keep. SQLite with disk always
//! runs; Postgres with S3 runs when `CRAFT_CLOUD_TEST_PG` and the `CRAFT_CLOUD_TEST_S3_*` variables are set
//! (`CRAFT_CLOUD_REQUIRE_BACKENDS` turns a missing one into a failure).

use bytes::Bytes;
use craft_cloud_meta::{BlobKind, BlobRow, Meta, Role};
use craft_cloud_proto::SpaceId;
use craft_cloud_server::storage_tools::{self, ESTIMATE_KEY};
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_store::s3::{S3Backend, S3Config};
use craft_cloud_store::{Backend, BlobKey, Compression, Store};

struct Env {
    meta: Meta,
    store: Store,
    raw: Box<dyn Backend>,
    space: SpaceId,
    _dirs: Vec<tempfile::TempDir>,
    pg: Option<(String, String)>,
}

impl Drop for Env {
    fn drop(&mut self) {
        let Some((admin_url, name)) = self.pg.take() else { return };
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

async fn with_space(meta: Meta) -> (Meta, SpaceId) {
    let user = meta.create_user("ana", "not a real hash", Role::Member, 0).await.unwrap();
    (meta, user.space)
}

async fn sqlite_disk() -> Env {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let (meta, space) = with_space(meta).await;
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let raw = Box::new(DiskBackend::new(blobs.path()).unwrap());
    Env { meta, store, raw, space, _dirs: vec![db, blobs], pg: None }
}

async fn postgres_s3() -> Option<Env> {
    let required = std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_some();
    let (Ok(admin_url), Ok(endpoint)) = (std::env::var("CRAFT_CLOUD_TEST_PG"), std::env::var("CRAFT_CLOUD_TEST_S3_ENDPOINT")) else {
        assert!(!required, "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_PG / CRAFT_CLOUD_TEST_S3_ENDPOINT are not");
        return None;
    };
    use sqlx::Connection;
    let name = format!("cc_test_{}", uuid::Uuid::now_v7().simple());
    let mut conn = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&mut conn).await.unwrap();
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    let pg = Some((admin_url.clone(), name.clone()));
    let meta = Meta::open(&format!("{base}/{name}")).await.unwrap();
    let (meta, space) = with_space(meta).await;
    let var = |n: &str| std::env::var(n).unwrap();
    let s3 = S3Config {
        bucket: var("CRAFT_CLOUD_TEST_S3_BUCKET"),
        prefix: format!("tools-test-{}", uuid::Uuid::now_v7()),
        region: "us-east-1".into(),
        endpoint: Some(endpoint),
        access_key_id: var("CRAFT_CLOUD_TEST_S3_ACCESS_KEY"),
        secret_access_key: var("CRAFT_CLOUD_TEST_S3_SECRET_KEY"),
        allow_http: true,
    };
    let store = Store::new(S3Backend::new(&s3).unwrap());
    let raw = Box::new(S3Backend::new(&s3).unwrap());
    Some(Env { meta, store, raw, space, _dirs: vec![], pg })
}

macro_rules! on_every_backend {
    ($($scenario:ident),* $(,)?) => {
        mod on_sqlite_disk {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() { super::$scenario(super::sqlite_disk().await).await; } )*
        }
        mod on_postgres_s3 {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                let Some(env) = super::postgres_s3().await else { eprintln!("skipped: Postgres/S3 not configured"); return };
                super::$scenario(env).await;
            } )*
        }
    };
}

on_every_backend!(
    recompress_rewrites_only_what_gets_smaller_and_continues_where_it_stopped,
    a_dry_run_changes_nothing,
    unreadable_objects_are_left_alone_and_reported,
    old_rows_are_unknown_until_measured,
    estimating_is_read_only_and_remembered,
);

/// Text from a small vocabulary in a pseudo-random order, unique per `tag`: it compresses well, and better at
/// higher levels.
fn wordy(tag: &str, len: usize) -> Vec<u8> {
    const WORDS: [&str; 24] = [
        "layer", "mask", "curve", "brush", "export", "canvas", "node", "group", "alpha", "blend", "filter", "frame", "path", "stroke", "fill",
        "style", "color", "scale", "rotate", "offset", "guide", "grid", "snap", "undo",
    ];
    let mut rnd = vec![0u8; len];
    blake3::Hasher::new().update(tag.as_bytes()).finalize_xof().fill(&mut rnd);
    let mut text = Vec::with_capacity(len + 16);
    for b in rnd {
        text.extend_from_slice(WORDS[usize::from(b) % WORDS.len()].as_bytes());
        text.push(b' ');
        if text.len() >= len {
            break;
        }
    }
    text.truncate(len);
    text
}

fn noise(tag: &str, len: usize) -> Vec<u8> {
    let mut data = vec![0u8; len];
    blake3::Hasher::new().update(tag.as_bytes()).finalize_xof().fill(&mut data);
    data
}

fn key_of(data: &[u8]) -> BlobKey {
    BlobKey { kind: BlobKind::Chunk, id: *blake3::hash(data).as_bytes() }
}

/// Stores a chunk through `store` and records it as the server does: its logical size, then what was written.
async fn add(env: &Env, store: &Store, data: &[u8]) -> BlobKey {
    let key = key_of(data);
    let written = store.put(key, data).await.unwrap();
    let id = craft_cloud_proto::ChunkId::from_bytes(key.id);
    env.meta.add_chunk(env.space, &id, data.len() as u64 + 1, 0).await.unwrap();
    env.meta.set_physical(BlobKind::Chunk, &key.id, written.physical_len, written.level).await.unwrap();
    key
}

/// Stores a chunk and records only its logical size, as a server before sizes were tracked did.
async fn add_old(env: &Env, data: &[u8]) -> BlobKey {
    let key = key_of(data);
    env.store.put(key, data).await.unwrap();
    let id = craft_cloud_proto::ChunkId::from_bytes(key.id);
    env.meta.add_chunk(env.space, &id, data.len() as u64 + 1, 0).await.unwrap();
    key
}

async fn raw(env: &Env, key: BlobKey) -> Bytes {
    env.raw.get_raw(&key.path()).await.unwrap().unwrap()
}

async fn rows(env: &Env) -> Vec<BlobRow> {
    env.meta.blob_page(None, 10_000).await.unwrap()
}

async fn recompress(env: &Env, level: u8, dry_run: bool, limit: Option<u64>) -> storage_tools::RecompressReport {
    storage_tools::recompress(&env.meta, &env.store, level, dry_run, limit, &mut Vec::new()).await.unwrap()
}

async fn recompress_rewrites_only_what_gets_smaller_and_continues_where_it_stopped(env: Env) {
    let low = env.store.clone().with_level(1);
    let mut texts = Vec::new();
    for n in 0..5 {
        let data = wordy(&format!("text {n}"), 200 * 1024);
        texts.push((add(&env, &low, &data).await, data));
    }
    // Not worth rewriting: bytes zstd can't shrink, and a blob under 4 KiB.
    let random = noise("random", 64 * 1024);
    let random_key = add(&env, &low, &random).await;
    let tiny = wordy("tiny", 1000);
    let tiny_key = add(&env, &low, &tiny).await;
    let before: Vec<Bytes> = {
        let mut all = Vec::new();
        for (key, _) in &texts {
            all.push(raw(&env, *key).await);
        }
        all
    };
    let random_before = raw(&env, random_key).await;
    let tiny_before = raw(&env, tiny_key).await;

    // The first bounded run stops early and the next one goes on.
    let first = recompress(&env, 19, false, Some(1)).await;
    assert!(first.stopped_at_limit && first.scanned == 1, "{first:?}");
    let rest = recompress(&env, 19, false, None).await;
    assert!(!rest.stopped_at_limit);
    assert_eq!(first.rewritten + rest.rewritten, 5, "{first:?} {rest:?}");
    assert_eq!(rest.scanned, 7, "every row is looked at");
    for ((key, data), old) in texts.iter().zip(&before) {
        let now = raw(&env, *key).await;
        assert!(now.len() < old.len(), "{} -> {}", old.len(), now.len());
        assert_eq!(env.store.get(*key).await.unwrap(), Bytes::from(data.clone()), "still the same bytes");
    }
    assert_eq!(raw(&env, random_key).await, random_before);
    assert_eq!(raw(&env, tiny_key).await, tiny_before);

    // The database follows: size of the object now, level 19, for everything the pass finished.
    for row in rows(&env).await {
        let object = raw(&env, BlobKey { kind: row.kind, id: row.id }).await;
        assert_eq!(row.physical_size, Some(object.len() as u64));
        assert_eq!(row.level, Some(19));
        assert!(row.stored_size > 0);
    }

    // Idempotent: a second pass skips everything it did, rewrites nothing and changes no object.
    let snapshot = rows(&env).await;
    let again = recompress(&env, 19, false, None).await;
    assert_eq!((again.rewritten, again.kept, again.already_done, again.scanned), (0, 0, 7, 7), "{again:?}");
    assert_eq!(rows(&env).await, snapshot);
    // A lower level has nothing to do either.
    let lower = recompress(&env, 9, false, None).await;
    assert_eq!((lower.rewritten, lower.already_done), (0, 7));
    // Blobs recorded at a lower level are tried again at a higher one, and a higher level that gains less than 1%
    // leaves the object alone.
    env.meta.set_physical(BlobKind::Chunk, &texts[0].0.id, before[0].len() as u64, 3).await.unwrap();
    let retry = recompress(&env, 19, false, None).await;
    assert_eq!((retry.scanned, retry.already_done, retry.kept, retry.rewritten), (7, 6, 1, 0), "{retry:?}");

    // Never deletes: every blob is still stored and readable.
    for (key, data) in &texts {
        assert_eq!(env.store.get(*key).await.unwrap(), Bytes::from(data.clone()));
    }
    assert_eq!(env.store.get(random_key).await.unwrap(), Bytes::from(random));
    assert_eq!(env.store.get(tiny_key).await.unwrap(), Bytes::from(tiny));
    // The pass refuses a level the store would not write.
    assert!(storage_tools::recompress(&env.meta, &env.store, 22, false, None, &mut Vec::new()).await.is_err());
    assert!(storage_tools::recompress(&env.meta, &env.store, 0, false, None, &mut Vec::new()).await.is_err());
}

async fn a_dry_run_changes_nothing(env: Env) {
    let low = env.store.clone().with_level(1);
    let mut keys = Vec::new();
    for n in 0..3 {
        keys.push(add(&env, &low, &wordy(&format!("dry {n}"), 128 * 1024)).await);
    }
    let mut objects = Vec::new();
    for key in &keys {
        objects.push(raw(&env, *key).await);
    }
    let snapshot = rows(&env).await;
    let report = recompress(&env, 19, true, None).await;
    assert_eq!(report.rewritten, 3, "{report:?}");
    assert!(report.bytes_after < report.bytes_before);
    for (key, object) in keys.iter().zip(&objects) {
        assert_eq!(&raw(&env, *key).await, object, "no object written");
    }
    assert_eq!(rows(&env).await, snapshot, "no row written");
    let mut printed = Vec::new();
    storage_tools::print_recompress(&report, 19, true, &mut printed).unwrap();
    let printed = String::from_utf8(printed).unwrap();
    assert!(printed.contains("would rewrite 3") && printed.contains("dry run: nothing was changed"), "{printed}");
    // And a real pass afterwards still does all of it: the dry run marked nothing as done.
    assert_eq!(recompress(&env, 19, false, None).await.rewritten, 3);
}

async fn unreadable_objects_are_left_alone_and_reported(env: Env) {
    let low = env.store.clone().with_level(1);
    let good = wordy("good", 128 * 1024);
    let good_key = add(&env, &low, &good).await;
    // A stored object whose bytes no longer match its ID, one that is not zstd at all, and one with no object.
    let wrong = wordy("wrong", 128 * 1024);
    let wrong_key = add(&env, &low, &wrong).await;
    let mut flipped = raw(&env, wrong_key).await.to_vec();
    let last = flipped.len() - 1;
    flipped[last] ^= 0xff;
    env.raw.put_raw(&wrong_key.path(), Bytes::from(flipped.clone())).await.unwrap();
    let junk = wordy("junk", 128 * 1024);
    let junk_key = add(&env, &low, &junk).await;
    env.raw.put_raw(&junk_key.path(), Bytes::from_static(b"\x01not a zstd stream")).await.unwrap();
    let gone = wordy("gone", 128 * 1024);
    let gone_key = add(&env, &low, &gone).await;
    env.store.delete(gone_key).await.unwrap();
    let snapshot = rows(&env).await;

    let good_before = raw(&env, good_key).await;
    let report = recompress(&env, 19, false, None).await;
    assert_eq!((report.rewritten, report.failed, report.missing), (1, 2, 1), "{report:?}");
    assert!(report.failures.iter().any(|f| f.contains(&wrong_key.path())), "{:?}", report.failures);
    assert!(report.failures.iter().any(|f| f.contains(&junk_key.path())), "{:?}", report.failures);
    assert!(raw(&env, good_key).await.len() < good_before.len());
    assert_eq!(raw(&env, wrong_key).await.to_vec(), flipped, "left as found");
    assert_eq!(raw(&env, junk_key).await.as_ref(), b"\x01not a zstd stream");
    assert!(env.raw.get_raw(&gone_key.path()).await.unwrap().is_none(), "not recreated");
    // Their rows are untouched (nothing claims they were handled).
    for row in rows(&env).await.iter().filter(|r| r.id != good_key.id) {
        assert!(snapshot.contains(row), "{row:?}");
    }
    let mut printed = Vec::new();
    storage_tools::print_recompress(&report, 19, false, &mut printed).unwrap();
    let printed = String::from_utf8(printed).unwrap();
    assert!(printed.contains("left alone, not readable") && printed.contains("1 without an object"), "{printed}");
}

async fn old_rows_are_unknown_until_measured(env: Env) {
    // Rows from before sizes were tracked: unknown, not zero and not the logical size.
    let old_data = wordy("old", 100 * 1024);
    let old_key = add_old(&env, &old_data).await;
    let row = rows(&env).await.remove(0);
    assert_eq!((row.physical_size, row.level), (None, None));
    let totals = env.meta.storage_totals().await.unwrap();
    assert_eq!((totals.blobs, totals.known_blobs, totals.known_physical_bytes, totals.known_logical_bytes), (1, 0, 0, 0));
    assert_eq!(totals.logical_bytes, old_data.len() as u64 + 1);

    // A new write is known; the totals say how much of the data they know.
    let new_data = wordy("new", 100 * 1024);
    let new_key = add(&env, &env.store, &new_data).await;
    let totals = env.meta.storage_totals().await.unwrap();
    assert_eq!((totals.blobs, totals.known_blobs), (2, 1));
    let new_object = raw(&env, new_key).await;
    assert_eq!((totals.known_physical_bytes, totals.known_logical_bytes), (new_object.len() as u64, new_data.len() as u64 + 1));
    assert_eq!(totals.logical_bytes, old_data.len() as u64 + new_data.len() as u64 + 2);

    // `store measure` fills in the old one from the object, without changing its level (still unknown).
    let (filled, missing) = storage_tools::measure(&env.meta, &env.store, &mut Vec::new()).await.unwrap();
    assert_eq!((filled, missing), (1, 0));
    let old_row = rows(&env).await.into_iter().find(|r| r.id == old_key.id).unwrap();
    assert_eq!((old_row.physical_size, old_row.level), (Some(raw(&env, old_key).await.len() as u64), None));
    assert_eq!(env.meta.storage_totals().await.unwrap().known_blobs, 2);
    // Nothing left to measure.
    assert_eq!(storage_tools::measure(&env.meta, &env.store, &mut Vec::new()).await.unwrap(), (0, 0));

    // An old row is also filled in by a recompress pass (it is unknown, so it is not skipped).
    let other = add_old(&env, &wordy("other old", 100 * 1024)).await;
    let report = recompress(&env, 19, false, None).await;
    assert_eq!((report.already_done, report.scanned), (0, 3), "{report:?}");
    let other_row = rows(&env).await.into_iter().find(|r| r.id == other.id).unwrap();
    assert_eq!((other_row.physical_size, other_row.level), (Some(raw(&env, other).await.len() as u64), Some(19)));
}

async fn estimating_is_read_only_and_remembered(env: Env) {
    // Nothing to sample on an empty server.
    assert!(storage_tools::estimate(&env.meta, &env.store, 10, &[3]).await.unwrap().is_none());

    let low = env.store.clone().with_level(1);
    for n in 0..6 {
        add(&env, &low, &wordy(&format!("est {n}"), 150 * 1024)).await;
    }
    add(&env, &low, &noise("est noise", 80 * 1024)).await;
    let rows_before = rows(&env).await;
    let mut objects_before = Vec::new();
    for row in &rows_before {
        objects_before.push(raw(&env, BlobKey { kind: row.kind, id: row.id }).await);
    }

    let estimate = storage_tools::estimate(&env.meta, &env.store, 50, &[1, 19]).await.unwrap().unwrap();
    assert!(estimate.sampled_blobs >= 1 && estimate.sampled_blobs <= 7, "{estimate:?}");
    assert_eq!(estimate.levels.iter().map(|l| l.level).collect::<Vec<_>>(), vec![1, 19]);
    assert!(estimate.levels[1].saving_percent >= estimate.levels[0].saving_percent, "{estimate:?}");
    assert!(estimate.levels[0].saving_percent > 30.0 && estimate.levels[0].mib_per_second > 0.0, "{estimate:?}");
    assert!(estimate.current_saving_percent > 30.0);

    // Read only: no object and no row changed, and the estimate row is the only thing written.
    assert_eq!(rows(&env).await, rows_before);
    for (row, old) in rows_before.iter().zip(&objects_before) {
        assert_eq!(&raw(&env, BlobKey { kind: row.kind, id: row.id }).await, old);
    }
    assert!(storage_tools::last_estimate(&env.meta).await.unwrap().is_none(), "estimating alone stores nothing");

    // Remembered with its date, and an unparsable value is no estimate rather than an error.
    storage_tools::save_estimate(&env.meta, &estimate, 1234).await.unwrap();
    let (kept, at) = storage_tools::last_estimate(&env.meta).await.unwrap().unwrap();
    assert_eq!((kept, at), (estimate, 1234));
    env.meta.set_server_value(ESTIMATE_KEY, "{not json", 99).await.unwrap();
    assert!(storage_tools::last_estimate(&env.meta).await.unwrap().is_none());
}

#[test]
fn levels_are_parsed_and_checked() {
    assert_eq!(storage_tools::parse_levels("1,3, 9,19,3").unwrap(), vec![1, 3, 9, 19]);
    for bad in ["", ",", "0", "20", "22", "x", "3,-1", "300"] {
        assert!(storage_tools::parse_levels(bad).is_err(), "{bad:?}");
    }
}

#[tokio::test]
async fn compression_none_is_a_setting_the_pass_respects_in_the_cli() {
    use std::io::Cursor;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let run = |args: Vec<String>| async move {
        use clap::Parser;
        let cli = craft_cloud_server::cli::Cli::try_parse_from(std::iter::once("craft-cloud-server".to_owned()).chain(args)).unwrap();
        let mut out = Vec::new();
        let result = craft_cloud_server::cli::run(cli.command, &mut Cursor::new(b"admin password 1\n".to_vec()), &mut out).await;
        (result, String::from_utf8(out).unwrap())
    };
    let (init, _) = run(vec!["init".into(), "--dir".into(), dir.to_str().unwrap().into(), "--admin".into(), "ana".into()]).await;
    init.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap().to_owned();

    // Some blobs, written at level 1 through the config's own store.
    let config = craft_cloud_server::config::Config::load(&config_path).unwrap();
    let (meta, store) = craft_cloud_server::cli::open(&config).await.unwrap();
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let low = store.with_level(1);
    for n in 0..3 {
        let data = wordy(&format!("cli {n}"), 128 * 1024);
        let key = key_of(&data);
        let written = low.put(key, &data).await.unwrap();
        meta.add_chunk(ana.space, &craft_cloud_proto::ChunkId::from_bytes(key.id), data.len() as u64 + 1, 0).await.unwrap();
        meta.set_physical(BlobKind::Chunk, &key.id, written.physical_len, written.level).await.unwrap();
    }

    let (dry, out) = run(vec!["store".into(), "--config".into(), c.clone(), "recompress".into(), "--level".into(), "19".into(), "--dry-run".into()]).await;
    dry.unwrap();
    assert!(out.contains("would rewrite 3") && out.contains("dry run"), "{out}");
    let (done, out) = run(vec!["store".into(), "recompress".into(), "--level".into(), "19".into(), "--config".into(), c.clone()]).await;
    done.unwrap();
    assert!(out.contains("rewrote 3"), "{out}");
    let (again, out) = run(vec!["store".into(), "--config".into(), c.clone(), "recompress".into(), "--level".into(), "19".into()]).await;
    again.unwrap();
    assert!(out.contains("rewrote 0") && out.contains("skipped 3 done before"), "{out}");
    let (bad_level, _) = run(vec!["store".into(), "--config".into(), c.clone(), "recompress".into(), "--level".into(), "25".into()]).await;
    assert!(bad_level.is_err());

    let (est, out) = run(vec!["store".into(), "--config".into(), c.clone(), "estimate".into(), "--sample".into(), "10".into(), "--levels".into(), "1,19".into()]).await;
    est.unwrap();
    assert!(out.contains("level  saving") && out.contains("kept for the admin overview"), "{out}");
    assert!(storage_tools::last_estimate(&meta).await.unwrap().is_some());

    // "none" in the config wins: the pass would write compressed objects, so it refuses.
    let mut text = std::fs::read_to_string(&config_path).unwrap();
    text.push_str("compression = \"none\"\n");
    std::fs::write(&config_path, text).unwrap();
    let (refused, _) = run(vec!["store".into(), "--config".into(), c.clone(), "recompress".into(), "--level".into(), "19".into()]).await;
    assert!(matches!(refused, Err(craft_cloud_server::cli::CliError::Usage(m)) if m.contains("none")));
    // Measuring reads nothing and still works.
    let (measured, out) = run(vec!["store".into(), "--config".into(), c, "measure".into()]).await;
    measured.unwrap();
    assert!(out.contains("measured 0 blobs"), "{out}");
    assert_eq!(Compression::None, craft_cloud_server::config::Config::load(&config_path).unwrap().storage.compression().into());
}
