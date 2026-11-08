//! Team spaces (migration 0015): the migration from a filled 0014 database, the table constraints, who has which role
//! where, the member changes with their audit events, and the last-owner rule under concurrent changes. SQLite always
//! runs; Postgres runs when `CRAFT_CLOUD_TEST_PG` holds an admin URL (`CRAFT_CLOUD_REQUIRE_BACKENDS` makes a missing one a
//! failure), as in `behaviour.rs`.

use craft_cloud_meta::{
    ActionFilter, AdminActor, AdminVia, AuditEvent, AuditQuery, Meta, MetaError, Role, SpaceActor, UserId, UserRecord, WriteGuard,
};
use craft_cloud_proto::api::SpaceRole;
use craft_cloud_proto::{DeviceId, SpaceId, SpaceName};
use sqlx::{AnyPool, Row};

const NOW: i64 = 1_700_000_000_000;
const ADMIN_HASH: &str = "$argon2id$admin";

struct Db {
    meta: Meta,
    url: String,
    postgres: bool,
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

/// A fresh, empty SQLite database: its URL and the guard that removes it.
fn sqlite_database() -> (String, Guard) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("meta.db").display());
    (url, Guard::Sqlite(dir))
}

/// A fresh, empty Postgres database: its URL, and the guard that drops it.
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
    Some((format!("{base}/{name}"), Guard::Postgres { admin_url, name }))
}

async fn sqlite() -> Db {
    let (url, guard) = sqlite_database();
    Db { meta: Meta::open(&url).await.unwrap(), url, postgres: false, _guard: guard }
}

async fn postgres() -> Option<Db> {
    let (url, guard) = postgres_database().await?;
    Some(Db { meta: Meta::open(&url).await.unwrap(), url, postgres: true, _guard: guard })
}

/// A second, raw connection pool on the database, for what the store's API does not offer: setting columns, breaking
/// rules on purpose.
async fn raw(url: &str) -> AnyPool {
    sqlx::any::install_default_drivers();
    sqlx::any::AnyPoolOptions::new().max_connections(4).connect(url).await.unwrap()
}

async fn run(pool: &AnyPool, sql: String) -> Result<(), sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await.map(|_| ())
}

macro_rules! on_every_backend {
    ($($scenario:ident),* $(,)?) => {
        mod on_sqlite {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() { let db = super::sqlite().await; super::$scenario(&db).await; } )*
        }
        mod on_postgres {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                let Some(db) = super::postgres().await else { eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set"); return };
                super::$scenario(&db).await;
            } )*
        }
    };
}

on_every_backend!(
    the_table_constraints_hold,
    the_access_matrix,
    members_come_and_go_with_their_events,
    the_command_line_and_admins_create_and_manage_spaces,
    the_last_owner_stays,
    review_fixes_of_the_routes_hold,
    an_owner_who_cannot_sign_in_is_not_counted,
    concurrent_changes_never_leave_a_space_without_an_owner,
    guarded_writes_check_the_role_in_their_own_transaction,
    concurrent_guarded_uploads_do_not_deadlock,
);

// ---- helpers ----

/// The review of the routes (P1-3): a role too low inside the transaction is `ReadOnly`, an invitation to an archived space
/// is no invitation, the command line accepts a pending invitation, and the administrator's search is not a name.
async fn review_fixes_of_the_routes_hold(db: &Db) {
    let meta = &db.meta;
    let (ana, ben, cy) = (user(meta, "ana").await, user(meta, "ben").await, user(meta, "cy").await);
    let space = team(meta, "Studio", &ana).await;
    join(meta, space, &ana, &ben, SpaceRole::Editor).await;

    // A role that is too low: an editor and an owner of an archived space (capped at viewer) are `ReadOnly`, not
    // `Forbidden`; an account with no access at all still learns nothing.
    assert!(matches!(meta.rename_team_space(&by(&ben), space, &team_name("Other"), NOW + 5).await, Err(MetaError::ReadOnly)));
    meta.set_space_archived(&by(&ana), space, true, NOW + 6).await.unwrap();
    assert!(matches!(meta.rename_team_space(&by(&ana), space, &team_name("Other"), NOW + 7).await, Err(MetaError::ReadOnly)));
    assert!(matches!(meta.rename_team_space(&by(&cy), space, &team_name("Other"), NOW + 7).await, Err(MetaError::NotFound("space"))));
    meta.set_space_archived(&by(&ana), space, false, NOW + 8).await.unwrap();

    // An invitation to an archived space can be neither accepted nor declined (it is not listed either): no such space.
    meta.invite_member(&by(&ana), space, cy.id, SpaceRole::Viewer, NOW + 9).await.unwrap();
    meta.set_space_archived(&by(&ana), space, true, NOW + 10).await.unwrap();
    assert!(matches!(meta.accept_invitation(cy.id, None, space, NOW + 11).await, Err(MetaError::NotFound("space"))));
    assert!(matches!(meta.decline_invitation(cy.id, None, space, NOW + 11).await, Err(MetaError::NotFound("space"))));
    assert_eq!(meta.space_access(cy.id, space).await.unwrap(), None, "nothing was accepted");
    meta.set_space_archived(&by(&ana), space, false, NOW + 12).await.unwrap();

    // The command line sets a role and accepts a pending invitation in one step (whoever runs it has the files).
    let pending = meta.list_members(space).await.unwrap().into_iter().find(|m| m.user == cy.id).unwrap();
    assert!(!pending.accepted());
    let done = meta.change_member_role(&SpaceActor::Cli, space, cy.id, SpaceRole::Viewer, NOW + 13).await.unwrap();
    assert!(done.accepted());
    assert_eq!(meta.space_access(cy.id, space).await.unwrap(), Some(SpaceRole::Viewer));
    // Again: nothing to change, nothing written.
    let before = space_events(meta).await.len();
    meta.change_member_role(&SpaceActor::Cli, space, cy.id, SpaceRole::Viewer, NOW + 14).await.unwrap();
    assert_eq!(space_events(meta).await.len(), before);

    // The administrator's search is folded text, not a name: one letter and padded text work.
    assert_eq!(meta.admin_list_spaces(None, Some("  STU "), 10).await.unwrap().spaces.len(), 1);
    assert_eq!(meta.admin_list_spaces(None, Some("u"), 10).await.unwrap().spaces.len(), 1);
    assert!(meta.admin_list_spaces(None, Some("0194a2d0-1c3e-7b5a-9d41-2f6e8a7b3c10"), 10).await.unwrap().spaces.is_empty());
    assert!(matches!(meta.admin_list_spaces(None, Some(&"x".repeat(65)), 10).await, Err(MetaError::Invalid(_))));
    // A name shaped like a space ID is refused, so a space is never named like another.
    assert!(SpaceName::new("0194a2d0-1c3e-7b5a-9d41-2f6e8a7b3c10").is_err());
}

/// An owner whose account is disabled or marked for removal has no access, so the administrator's list does not count it
/// (the space may have nobody left to manage it), and the owners of a page come in one query with their account's state.
async fn an_owner_who_cannot_sign_in_is_not_counted(db: &Db) {
    let meta = &db.meta;
    let (boss, actor) = admin_actor(meta, "boss").await;
    let _ = boss;
    let (ana, ben, cy) = (user(meta, "ana").await, user(meta, "ben").await, user(meta, "cy").await);
    let (solo, pair, empty) = (team(meta, "Solo", &ana).await, team(meta, "Pair", &ben).await, team(meta, "Empty", &cy).await);
    meta.set_owner_accepted(&actor, pair, ana.id, NOW + 1).await.unwrap();
    let owners = |page: Vec<craft_cloud_meta::SpaceSummary>| page.into_iter().map(|s| (s.name, s.owners)).collect::<Vec<_>>();
    let all = || async { owners(meta.admin_list_spaces(None, None, 10).await.unwrap().spaces) };
    assert_eq!(all().await, [("Empty".to_owned(), 1), ("Pair".to_owned(), 2), ("Solo".to_owned(), 1)]);

    meta.admin_disable_user(&actor, ana.id, NOW + 2).await.unwrap();
    meta.admin_remove_user(&actor, cy.id, NOW + 3).await.unwrap();
    assert_eq!(all().await, [("Empty".to_owned(), 0), ("Pair".to_owned(), 1), ("Solo".to_owned(), 0)]);
    assert_eq!(meta.admin_space(solo).await.unwrap().owners, 0);
    assert_eq!(meta.admin_space(pair).await.unwrap().members, 2, "the member is still a member");

    // The owners of the listed spaces: one query, every accepted owner with the state of the account, by space and name.
    let found = meta.space_owners(&[solo, pair, empty]).await.unwrap();
    let mut seen: Vec<_> = found.iter().map(|o| (o.space, o.username.as_str(), o.active())).collect();
    seen.sort_by_key(|(space, name, _)| (space.to_string(), *name));
    let mut expected = vec![(solo, "ana", false), (pair, "ana", false), (pair, "ben", true), (empty, "cy", false)];
    expected.sort_by_key(|(space, name, _)| (space.to_string(), *name));
    assert_eq!(seen, expected);
    assert!(found.iter().any(|o| o.username == "ana" && o.disabled_ms.is_some() && o.removed_ms.is_none()));
    assert!(found.iter().any(|o| o.username == "cy" && o.removed_ms.is_some()));
    assert!(meta.space_owners(&[]).await.unwrap().is_empty());
    assert!(meta.space_owners(&[pair]).await.unwrap().iter().all(|o| o.space == pair));
    // An invited owner is no owner yet.
    meta.invite_member(&by(&ben), pair, user(meta, "dee").await.id, SpaceRole::Owner, NOW + 4).await.unwrap();
    assert_eq!(meta.space_owners(&[pair]).await.unwrap().len(), 2);

    // Naming a new owner brings the space back.
    meta.set_owner_accepted(&actor, solo, ben.id, NOW + 5).await.unwrap();
    assert_eq!(meta.admin_space(solo).await.unwrap().owners, 1);
}

async fn user(meta: &Meta, name: &str) -> UserRecord {
    meta.create_user(name, "$argon2id$stub", Role::Member, NOW).await.unwrap()
}

async fn device(meta: &Meta, u: &UserRecord) -> DeviceId {
    let token = *blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes();
    meta.create_device(u.id, "laptop", &token, NOW).await.unwrap()
}

async fn admin_actor(meta: &Meta, name: &str) -> (UserRecord, AdminActor) {
    let u = meta.create_user(name, ADMIN_HASH, Role::Admin, NOW).await.unwrap();
    let device = device(meta, &u).await;
    (u.clone(), AdminActor { user: u.id, via: AdminVia::Device(device), password_hash: ADMIN_HASH.to_owned(), two_factor: false })
}

fn team_name(name: &str) -> SpaceName {
    SpaceName::new(name).unwrap()
}

fn by(u: &UserRecord) -> SpaceActor {
    SpaceActor::User { user: u.id, device: None }
}

/// A team space made on the command line with `owner` as its first owner.
async fn team(meta: &Meta, name: &str, owner: &UserRecord) -> SpaceId {
    meta.create_team_space(&SpaceActor::Cli, &team_name(name), owner.id, None, NOW).await.unwrap()
}

/// `u` invited by the owner `by_owner` with `role`, and accepting.
async fn join(meta: &Meta, space: SpaceId, by_owner: &UserRecord, u: &UserRecord, role: SpaceRole) {
    meta.invite_member(&by(by_owner), space, u.id, role, NOW + 1).await.unwrap();
    assert_eq!(meta.accept_invitation(u.id, None, space, NOW + 2).await.unwrap(), role);
}

async fn space_events(meta: &Meta) -> Vec<AuditEvent> {
    let query = AuditQuery { action: ActionFilter::parse("space."), limit: 1000, ..AuditQuery::default() };
    let mut events = meta.audit_list(&query).await.unwrap();
    let cli = AuditQuery { action: ActionFilter::parse("cli."), limit: 1000, ..AuditQuery::default() };
    events.extend(meta.audit_list(&cli).await.unwrap().into_iter().filter(|e| e.action.as_str().starts_with("cli.space_")));
    events.sort_by_key(|e| (e.at_ms, e.id));
    events
}

fn names(events: &[AuditEvent]) -> Vec<&'static str> {
    events.iter().map(|e| e.action.as_str()).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A blob literal of `len` bytes made of `tag` and `n`, in the backend's syntax.
fn blob(pg: bool, tag: u8, n: u8, len: usize) -> String {
    let mut bytes = vec![0u8; len];
    bytes[0] = tag;
    bytes[1] = n;
    if pg { format!("'\\x{}'::bytea", hex(&bytes)) } else { format!("X'{}'", hex(&bytes)) }
}

// ---- the migration ----

const SQLITE_0014: [&str; 14] = [
    include_str!("../migrations/sqlite/0001_init.sql"),
    include_str!("../migrations/sqlite/0002_stored_bytes.sql"),
    include_str!("../migrations/sqlite/0003_live.sql"),
    include_str!("../migrations/sqlite/0004_join.sql"),
    include_str!("../migrations/sqlite/0005_live_documents.sql"),
    include_str!("../migrations/sqlite/0006_save_records.sql"),
    include_str!("../migrations/sqlite/0007_devices_v2.sql"),
    include_str!("../migrations/sqlite/0008_admin_resets.sql"),
    include_str!("../migrations/sqlite/0009_audit_events.sql"),
    include_str!("../migrations/sqlite/0010_two_factor.sql"),
    include_str!("../migrations/sqlite/0011_web_sessions.sql"),
    include_str!("../migrations/sqlite/0012_versions_node_seq.sql"),
    include_str!("../migrations/sqlite/0013_trash.sql"),
    include_str!("../migrations/sqlite/0014_manifest_index.sql"),
];

const POSTGRES_0014: [&str; 14] = [
    include_str!("../migrations/postgres/0001_init.sql"),
    include_str!("../migrations/postgres/0002_stored_bytes.sql"),
    include_str!("../migrations/postgres/0003_live.sql"),
    include_str!("../migrations/postgres/0004_join.sql"),
    include_str!("../migrations/postgres/0005_live_documents.sql"),
    include_str!("../migrations/postgres/0006_save_records.sql"),
    include_str!("../migrations/postgres/0007_devices_v2.sql"),
    include_str!("../migrations/postgres/0008_admin_resets.sql"),
    include_str!("../migrations/postgres/0009_audit_events.sql"),
    include_str!("../migrations/postgres/0010_two_factor.sql"),
    include_str!("../migrations/postgres/0011_web_sessions.sql"),
    include_str!("../migrations/postgres/0012_versions_node_seq.sql"),
    include_str!("../migrations/postgres/0013_trash.sql"),
    include_str!("../migrations/postgres/0014_manifest_index.sql"),
];

/// A database at schema version 14 with rows in every kind of table that holds user data: two accounts with devices,
/// their spaces with a folder, a file with a version, a deleted file, blobs, live documents with updates, an audit event,
/// an invite and the manifest index.
async fn database_at_0014(url: &str, pg: bool) -> AnyPool {
    let pool = raw(url).await;
    run(&pool, "CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL)".to_owned()).await.unwrap();
    for (index, sql) in (if pg { POSTGRES_0014 } else { SQLITE_0014 }).iter().enumerate() {
        for statement in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            run(&pool, statement.to_owned()).await.unwrap();
        }
        run(&pool, format!("INSERT INTO schema_version (version) VALUES ({})", index + 1)).await.unwrap();
    }
    let id = |tag: u8, n: u8| blob(pg, tag, n, 16);
    let hash = |tag: u8, n: u8| blob(pg, tag, n, 32);
    let (ana, ben) = (id(1, 1), id(1, 2));
    let (space_a, space_b) = (id(2, 1), id(2, 2));
    let sql = vec![
        format!(
            "INSERT INTO users (id, username, password_hash, role, created_ms) VALUES ({ana}, 'ana', 'h1', 'admin', 111), ({ben}, 'ben', 'h2', 'member', 222)"
        ),
        format!(
            "INSERT INTO spaces (id, owner, epoch, seq, oldest_seq, root, stored_bytes, live_bytes, quota_bytes) VALUES \
             ({space_a}, {ana}, {e1}, 7, 2, {r1}, 12345, 77, 99999999), ({space_b}, {ben}, {e2}, 0, 0, {r2}, 0, 0, NULL)",
            e1 = id(3, 1),
            r1 = id(4, 1),
            e2 = id(3, 2),
            r2 = id(4, 2)
        ),
        format!(
            "INSERT INTO devices (id, user_id, name, token_hash, created_ms, last_used_ms, revoked) VALUES ({d1}, {ana}, 'laptop', {t1}, 1, 2, 0), ({d2}, {ben}, 'pc', {t2}, 3, 4, 1)",
            d1 = id(5, 1),
            t1 = hash(6, 1),
            d2 = id(5, 2),
            t2 = hash(6, 2)
        ),
        format!(
            "INSERT INTO nodes (space_id, id, parent, name, fold_key, is_dir, version, deleted, changed_seq, deleted_ms) VALUES \
             ({space_a}, {root}, NULL, '', '', 1, NULL, 0, 0, NULL), \
             ({space_a}, {dir}, {root}, 'Docs', 'docs', 1, NULL, 0, 3, NULL), \
             ({space_a}, {file}, {dir}, 'a.txt', 'a.txt', 0, {v1}, 0, 5, NULL), \
             ({space_a}, {gone}, {root}, 'old.txt', 'old.txt', 0, {v2}, 1, 6, 1234), \
             ({space_b}, {root_b}, NULL, '', '', 1, NULL, 0, 0, NULL)",
            root = id(4, 1),
            dir = id(7, 1),
            file = id(7, 2),
            gone = id(7, 3),
            v1 = id(8, 1),
            v2 = id(8, 2),
            root_b = id(4, 2)
        ),
        format!(
            "INSERT INTO versions (space_id, id, node, parent_version, epoch, manifest, size, mtime_ms, exec, device, created_ms, seq) VALUES \
             ({space_a}, {v1}, {file}, NULL, {e1}, {m1}, 10, 100, 0, {d1}, 5, 5), \
             ({space_a}, {v2}, {gone}, NULL, {e1}, {m2}, 20, 200, 1, {d1}, 6, 6)",
            v1 = id(8, 1),
            v2 = id(8, 2),
            file = id(7, 2),
            gone = id(7, 3),
            e1 = id(3, 1),
            m1 = hash(9, 1),
            m2 = hash(9, 2),
            d1 = id(5, 1)
        ),
        format!(
            "INSERT INTO blobs (kind, id, stored_size, file_size, created_ms) VALUES (0, {c1}, 100, 90, 1), (1, {m1}, 50, NULL, 2)",
            c1 = hash(10, 1),
            m1 = hash(9, 1)
        ),
        format!(
            "INSERT INTO space_blobs (space_id, kind, id) VALUES ({space_a}, 0, {c1}), ({space_a}, 1, {m1}), ({space_b}, 0, {c1})",
            c1 = hash(10, 1),
            m1 = hash(9, 1)
        ),
        format!(
            "INSERT INTO live_docs (space_id, id, kind, node, created_ms, head_seq, bytes) VALUES ({space_a}, {doc}, 'document', {file}, 9, 2, 33)",
            doc = id(11, 1),
            file = id(7, 2)
        ),
        format!(
            "INSERT INTO live_updates (space_id, doc, seq, bytes, device, client_seq, created_ms) VALUES ({space_a}, {doc}, 1, {u1}, {d1}, 1, 10), ({space_a}, {doc}, 2, {u2}, {d1}, 2, 11)",
            doc = id(11, 1),
            u1 = id(12, 1),
            u2 = id(12, 2),
            d1 = id(5, 1)
        ),
        format!("INSERT INTO live_spaces (space_id, generation) VALUES ({space_a}, 4)"),
        format!("INSERT INTO audit_events (id, at_ms, action, outcome) VALUES ({a1}, 5, 'auth.login', 'success')", a1 = id(13, 1)),
        format!("INSERT INTO invites (user_id, code_hash, expires_ms) VALUES ({ben}, {c}, 99)", c = hash(14, 1)),
        format!("INSERT INTO manifest_chunks (manifest, ordinal, chunk) VALUES ({m1}, 0, {c1})", m1 = hash(9, 1), c1 = hash(10, 1)),
        format!("INSERT INTO manifest_index (manifest, chunk_count, indexed_ordinals, state) VALUES ({m1}, 1, 1, 2)", m1 = hash(9, 1)),
    ];
    for statement in sql {
        run(&pool, statement.clone()).await.unwrap_or_else(|e| panic!("{e}: {statement}"));
    }
    pool
}

/// Every column of a row as text, `NULL` for none.
fn cells(row: &sqlx::any::AnyRow) -> String {
    use sqlx::Row;
    (0..row.columns().len())
        .map(|i| {
            if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
                return v.map_or("NULL".to_owned(), |v| v.to_string());
            }
            if let Ok(v) = row.try_get::<Option<i32>, _>(i) {
                return v.map_or("NULL".to_owned(), |v| v.to_string());
            }
            if let Ok(v) = row.try_get::<Option<i16>, _>(i) {
                return v.map_or("NULL".to_owned(), |v| v.to_string());
            }
            if let Ok(v) = row.try_get::<Option<String>, _>(i) {
                return v.map_or("NULL".to_owned(), |v| format!("{v:?}"));
            }
            if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(i) {
                return v.map_or("NULL".to_owned(), |v| hex(&v));
            }
            panic!("a column of a type the test cannot read");
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// Every row of every table the migration must not touch, as text, rows sorted: equal before and after means nothing
/// else changed.
async fn fingerprint(pool: &AnyPool, pg: bool) -> Vec<(String, Vec<String>)> {
    let list = if pg {
        "SELECT CAST(table_name AS TEXT) AS name FROM information_schema.tables WHERE table_schema = current_schema() AND table_type = 'BASE TABLE'"
    } else {
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'"
    };
    let mut tables: Vec<String> = sqlx::query(list).fetch_all(pool).await.unwrap().iter().map(|r| r.try_get::<String, _>("name").unwrap()).collect();
    tables.retain(|t| !["spaces", "schema_version", "space_members"].contains(&t.as_str()));
    tables.sort();
    let mut out = Vec::new();
    for table in tables {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT * FROM {table}"))).fetch_all(pool).await.unwrap();
        let mut rows: Vec<String> = rows.iter().map(cells).collect();
        rows.sort();
        out.push((table, rows));
    }
    out
}

async fn old_space_columns(pool: &AnyPool) -> Vec<String> {
    let rows = sqlx::query("SELECT id, owner, epoch, seq, oldest_seq, root, stored_bytes, live_bytes, quota_bytes FROM spaces ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap();
    rows.iter().map(cells).collect()
}

async fn count(pool: &AnyPool, sql: &str) -> i64 {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_owned())).fetch_one(pool).await.unwrap().try_get::<i64, _>(0).unwrap()
}

async fn the_migration_from_a_filled_0014_database(url: &str, pg: bool) {
    let pool = database_at_0014(url, pg).await;
    let fingerprint_before = fingerprint(&pool, pg).await;
    assert!(fingerprint_before.iter().any(|(t, rows)| t == "versions" && rows.len() == 2));
    for filled in [
        "users",
        "devices",
        "nodes",
        "versions",
        "blobs",
        "space_blobs",
        "live_docs",
        "live_updates",
        "live_spaces",
        "audit_events",
        "invites",
        "manifest_chunks",
    ] {
        assert!(fingerprint_before.iter().any(|(t, rows)| t == filled && !rows.is_empty()), "{filled} is empty");
    }
    let spaces_before = old_space_columns(&pool).await;
    assert_eq!(spaces_before.len(), 2);
    pool.close().await;

    let meta = Meta::open(url).await.unwrap();
    let pool = raw(url).await;
    assert_eq!(count(&pool, "SELECT MAX(version) FROM schema_version").await, 15);
    assert_eq!(fingerprint(&pool, pg).await, fingerprint_before, "a table other than spaces changed");
    assert_eq!(old_space_columns(&pool).await, spaces_before, "a column of spaces that existed before changed");

    // Every space is a personal one with its owner and its owner's creation time; the new columns hold nothing else.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM spaces WHERE kind = 'personal' AND owner IS NOT NULL AND name IS NULL AND name_fold IS NULL AND archived_ms IS NULL"
        )
        .await,
        2
    );
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM spaces s JOIN users u ON u.id = s.owner WHERE s.created_ms = u.created_ms").await, 2);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM space_members").await, 0);

    // The foreign keys are intact.
    if !pg {
        let broken = sqlx::query("PRAGMA foreign_key_check").fetch_all(&pool).await.unwrap();
        assert!(broken.is_empty(), "foreign_key_check found {} rows", broken.len());
    }
    for table in ["nodes", "versions", "space_blobs", "live_docs", "live_spaces"] {
        assert_eq!(count(&pool, &format!("SELECT COUNT(*) FROM {table} WHERE space_id NOT IN (SELECT id FROM spaces)")).await, 0, "{table}");
    }
    // ... and enforced: a node of a space that does not exist is refused.
    let orphan = run(&pool, format!("INSERT INTO nodes (space_id, id, parent, name, fold_key, is_dir, version, deleted, changed_seq) VALUES ({}, {}, NULL, '', '', 1, NULL, 0, 0)", blob(pg, 99, 99, 16), blob(pg, 98, 98, 16))).await;
    assert!(orphan.is_err(), "an orphan node was accepted");

    // The old accounts and spaces work through the store as before.
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    assert_eq!(meta.space_access(ana.id, ana.space).await.unwrap(), Some(SpaceRole::Owner));
    assert_eq!(meta.space_info(ana.space).await.unwrap().seq, 7);
    let ben = meta.user_by_name("ben").await.unwrap().unwrap();
    assert_eq!(meta.space_access(ben.id, ana.space).await.unwrap(), None);
    // ... and a team space can be made beside them.
    let space = meta.create_team_space(&SpaceActor::Cli, &team_name("Studio"), ana.id, None, NOW).await.unwrap();
    assert_eq!(meta.space_access(ana.id, space).await.unwrap(), Some(SpaceRole::Owner));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_migration_from_a_filled_0014_database_on_sqlite() {
    let (url, _guard) = sqlite_database();
    the_migration_from_a_filled_0014_database(&url, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_migration_from_a_filled_0014_database_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    the_migration_from_a_filled_0014_database(&url, true).await;
}

// ---- constraints ----

/// The table's own rules, tried with raw SQL: what the checks and indexes of 0015 refuse and what they allow.
async fn the_table_constraints_hold(db: &Db) {
    let pg = db.postgres;
    let meta = &db.meta;
    let pool = raw(&db.url).await;
    let ana = user(meta, "ana").await;
    let id = |n: u8| blob(pg, 50, n, 16);
    let space = |n: u8, owner: &str, kind: &str, name: &str, fold: &str| {
        format!(
            "INSERT INTO spaces (id, owner, epoch, seq, oldest_seq, root, kind, name, name_fold) VALUES ({}, {owner}, {}, 0, 0, {}, '{kind}', {name}, {fold})",
            id(n),
            id(n + 100),
            id(n + 50)
        )
    };
    let owner = |u: &UserRecord| blob_of(pg, &u.id.0);
    // A personal space needs its owner, and a team space must not have one.
    assert!(run(&pool, space(1, "NULL", "personal", "NULL", "NULL")).await.is_err(), "a personal space without an owner");
    assert!(run(&pool, space(2, &owner(&ana), "team", "'T'", "'t'")).await.is_err(), "a team space with an owner");
    // A team space needs both names, and a personal one has none.
    assert!(run(&pool, space(3, "NULL", "team", "NULL", "NULL")).await.is_err(), "a team space without a name");
    assert!(run(&pool, space(4, "NULL", "team", "'T'", "NULL")).await.is_err(), "a team space without a folded name");
    // The same rules on a space that exists: ana's personal space cannot gain a name, lose its owner or become a team space.
    let hers = blob_of(pg, ana.space.as_uuid());
    assert!(run(&pool, format!("UPDATE spaces SET name = 'T', name_fold = 't' WHERE id = {hers}")).await.is_err(), "a personal space with a name");
    assert!(run(&pool, format!("UPDATE spaces SET owner = NULL WHERE id = {hers}")).await.is_err(), "a personal space losing its owner");
    assert!(run(&pool, format!("UPDATE spaces SET kind = 'team' WHERE id = {hers}")).await.is_err(), "a personal space turned into a team space");
    assert!(run(&pool, space(6, "NULL", "shared", "'T'", "'t'")).await.is_err(), "an unknown kind");
    // A second personal space for one account is refused (ana has hers from sign-up).
    assert!(run(&pool, space(7, &owner(&ana), "personal", "NULL", "NULL")).await.is_err(), "a second personal space");
    // Team spaces: two with one folded name are refused, different ones are fine, and they have no owner column value.
    run(&pool, space(8, "NULL", "team", "'Studio'", "'studio'")).await.unwrap();
    assert!(run(&pool, space(9, "NULL", "team", "'STUDIO'", "'studio'")).await.is_err(), "a second team space of one folded name");
    run(&pool, space(10, "NULL", "team", "'Other'", "'other'")).await.unwrap();
    // `space_members`: the role is a closed list, a member appears once, and both ends must exist.
    let member = |space: u8, user: &str, role: &str| {
        format!("INSERT INTO space_members (space_id, user_id, role, invited_ms) VALUES ({}, {user}, '{role}', 1)", id(space))
    };
    assert!(run(&pool, member(8, &owner(&ana), "admin")).await.is_err(), "an unknown role");
    run(&pool, member(8, &owner(&ana), "editor")).await.unwrap();
    assert!(run(&pool, member(8, &owner(&ana), "viewer")).await.is_err(), "a second row for one member");
    assert!(run(&pool, member(11, &owner(&ana), "viewer")).await.is_err(), "a member of a space that does not exist");
    assert!(run(&pool, member(8, &blob(pg, 77, 77, 16), "viewer")).await.is_err(), "a member that is no account");
    // A space with members cannot be deleted from under them.
    assert!(run(&pool, format!("DELETE FROM spaces WHERE id = {}", id(8))).await.is_err(), "a space with members was deleted");
}

fn blob_of(pg: bool, id: &uuid::Uuid) -> String {
    let hex = hex(id.as_bytes());
    if pg { format!("'\\x{hex}'::bytea") } else { format!("X'{hex}'") }
}

// ---- access ----

/// `space_access` for every kind of account in every kind of space.
async fn the_access_matrix(db: &Db) {
    let meta = &db.meta;
    let pool = raw(&db.url).await;
    let pg = db.postgres;
    let ana = user(meta, "ana").await;
    let ben = user(meta, "ben").await;
    let cy = user(meta, "cy").await;
    let dee = user(meta, "dee").await;
    let eve = user(meta, "eve").await;
    let fay = user(meta, "fay").await;
    let space = team(meta, "Studio", &ana).await;
    join(meta, space, &ana, &ben, SpaceRole::Viewer).await;
    join(meta, space, &ana, &cy, SpaceRole::Editor).await;
    join(meta, space, &ana, &dee, SpaceRole::Owner).await;
    // fay is invited and has not answered.
    meta.invite_member(&by(&ana), space, fay.id, SpaceRole::Editor, NOW + 3).await.unwrap();
    let access = |u: &UserRecord, s: SpaceId| {
        let meta = meta.clone();
        let id = u.id;
        async move { meta.space_access(id, s).await.unwrap() }
    };

    // The team space, by role.
    assert_eq!(access(&ana, space).await, Some(SpaceRole::Owner));
    assert_eq!(access(&ben, space).await, Some(SpaceRole::Viewer));
    assert_eq!(access(&cy, space).await, Some(SpaceRole::Editor));
    assert_eq!(access(&dee, space).await, Some(SpaceRole::Owner));
    assert_eq!(access(&fay, space).await, None, "a pending invitation gives no access");
    assert_eq!(access(&eve, space).await, None, "a stranger");

    // Personal spaces belong to their owners and nobody else, whatever team spaces exist.
    assert_eq!(access(&ana, ana.space).await, Some(SpaceRole::Owner));
    assert_eq!(access(&ben, ana.space).await, None);
    assert_eq!(access(&eve, ben.space).await, None);
    assert_eq!(access(&ben, ben.space).await, Some(SpaceRole::Owner));

    // A space that does not exist is the same answer as no access, and so is an account that does not exist.
    assert_eq!(meta.space_access(ana.id, SpaceId::generate()).await.unwrap(), None);
    assert_eq!(meta.space_access(UserId(uuid::Uuid::now_v7()), space).await.unwrap(), None);

    // An archived team space gives at most the viewer role; a personal space is never archived.
    run(&pool, format!("UPDATE spaces SET archived_ms = 5 WHERE id = {}", blob_of(pg, space.as_uuid()))).await.unwrap();
    assert_eq!(access(&ana, space).await, Some(SpaceRole::Viewer));
    assert_eq!(access(&ben, space).await, Some(SpaceRole::Viewer));
    assert_eq!(access(&cy, space).await, Some(SpaceRole::Viewer));
    assert_eq!(access(&fay, space).await, None);
    assert_eq!(access(&eve, space).await, None);
    assert_eq!(access(&ana, ana.space).await, Some(SpaceRole::Owner));
    run(&pool, "UPDATE spaces SET archived_ms = NULL".to_owned()).await.unwrap();
    assert_eq!(access(&cy, space).await, Some(SpaceRole::Editor));

    // A disabled account has no access anywhere, a removed one neither; the member rows stay, so enabling restores them.
    run(&pool, format!("UPDATE users SET disabled_ms = 9 WHERE id = {}", blob_of(pg, &cy.id.0))).await.unwrap();
    assert_eq!(access(&cy, space).await, None);
    assert_eq!(access(&cy, cy.space).await, None, "its personal space too");
    run(&pool, format!("UPDATE users SET disabled_ms = NULL, removed_ms = 9 WHERE id = {}", blob_of(pg, &cy.id.0))).await.unwrap();
    assert_eq!(access(&cy, space).await, None);
    run(&pool, format!("UPDATE users SET removed_ms = NULL WHERE id = {}", blob_of(pg, &cy.id.0))).await.unwrap();
    assert_eq!(access(&cy, space).await, Some(SpaceRole::Editor));
    assert_eq!(meta.list_members(space).await.unwrap().len(), 5);

    // Roles follow the changes at once.
    meta.change_member_role(&by(&ana), space, cy.id, SpaceRole::Viewer, NOW + 10).await.unwrap();
    assert_eq!(access(&cy, space).await, Some(SpaceRole::Viewer));
    meta.remove_member(&by(&ana), space, cy.id, NOW + 11).await.unwrap();
    assert_eq!(access(&cy, space).await, None);
    // The ordering of the roles is the ordering of permissions.
    assert!(SpaceRole::Viewer < SpaceRole::Editor && SpaceRole::Editor < SpaceRole::Owner);
}

// ---- members and their events ----

async fn members_come_and_go_with_their_events(db: &Db) {
    let meta = &db.meta;
    let pool = raw(&db.url).await;
    let pg = db.postgres;
    let ana = user(meta, "ana").await;
    let ben = user(meta, "ben").await;
    let cy = user(meta, "cy").await;
    let gone = user(meta, "gone").await;
    let off = user(meta, "off").await;
    let eve = user(meta, "eve").await;
    let device_ana = device(meta, &ana).await;
    let space = team(meta, "Studio", &ana).await;
    let owner = SpaceActor::User { user: ana.id, device: Some(device_ana) };

    // Whoever is not an owner may not manage; a stranger learns nothing (the answer is the one for no such space).
    meta.invite_member(&owner, space, ben.id, SpaceRole::Editor, NOW + 1).await.unwrap();
    assert!(matches!(meta.invite_member(&by(&eve), space, cy.id, SpaceRole::Viewer, NOW + 2).await, Err(MetaError::NotFound("space"))));
    // ben is only invited so far: still not a member who could invite.
    assert!(matches!(meta.invite_member(&by(&ben), space, cy.id, SpaceRole::Viewer, NOW + 2).await, Err(MetaError::NotFound("space"))));
    // One answer for an account that does not exist, one that is disabled and one that is marked for removal.
    run(&pool, format!("UPDATE users SET disabled_ms = 1 WHERE id = {}", blob_of(pg, &off.id.0))).await.unwrap();
    run(&pool, format!("UPDATE users SET disabled_ms = 1, removed_ms = 1 WHERE id = {}", blob_of(pg, &gone.id.0))).await.unwrap();
    for target in [UserId(uuid::Uuid::now_v7()), off.id, gone.id] {
        assert!(matches!(meta.invite_member(&owner, space, target, SpaceRole::Viewer, NOW + 3).await, Err(MetaError::NotFound("user"))));
    }
    // A second invitation, or an invitation of a member, is a conflict.
    assert!(matches!(meta.invite_member(&owner, space, ben.id, SpaceRole::Viewer, NOW + 3).await, Err(MetaError::Conflict(_))));
    assert!(matches!(meta.invite_member(&owner, space, ana.id, SpaceRole::Viewer, NOW + 3).await, Err(MetaError::Conflict(_))));
    // The pending invitation shows in the list, not as a member.
    let list = meta.list_members(space).await.unwrap();
    assert_eq!(
        list.iter().map(|m| (m.username.as_str(), m.role, m.accepted())).collect::<Vec<_>>(),
        [("ana", SpaceRole::Owner, true), ("ben", SpaceRole::Editor, false)]
    );
    assert_eq!(list[1].invited_by, Some(ana.id));
    assert_eq!(list[0].invited_by, None, "made on the command line");

    // Accepting turns it into access; again, or without an invitation, finds none and writes nothing.
    let before = space_events(meta).await.len();
    // An account with no row at all gets the answer for a space that does not exist, so it learns nothing about the space.
    let nowhere = craft_cloud_proto::SpaceId::generate();
    for space in [space, nowhere] {
        assert!(matches!(meta.accept_invitation(cy.id, None, space, NOW + 4).await, Err(MetaError::NotFound("space"))));
        assert!(matches!(meta.decline_invitation(cy.id, None, space, NOW + 4).await, Err(MetaError::NotFound("space"))));
        assert!(matches!(meta.leave_space(cy.id, None, space, NOW + 4).await, Err(MetaError::NotFound("space"))));
    }
    assert_eq!(meta.accept_invitation(ben.id, None, space, NOW + 5).await.unwrap(), SpaceRole::Editor);
    assert!(matches!(meta.accept_invitation(ben.id, None, space, NOW + 6).await, Err(MetaError::NotFound("invitation"))));
    assert!(matches!(meta.decline_invitation(ben.id, None, space, NOW + 6).await, Err(MetaError::NotFound("invitation"))));
    assert_eq!(space_events(meta).await.len(), before + 1);

    // An editor may not manage the members: a member whose role is too low is `ReadOnly` (403 `read_only`).
    assert!(matches!(meta.invite_member(&by(&ben), space, cy.id, SpaceRole::Viewer, NOW + 7).await, Err(MetaError::ReadOnly)));
    assert!(matches!(meta.change_member_role(&by(&ben), space, ben.id, SpaceRole::Owner, NOW + 7).await, Err(MetaError::ReadOnly)));
    assert!(matches!(meta.remove_member(&by(&ben), space, ana.id, NOW + 7).await, Err(MetaError::ReadOnly)));

    // Decline deletes the invitation, and the account can be invited again.
    meta.invite_member(&owner, space, cy.id, SpaceRole::Viewer, NOW + 8).await.unwrap();
    meta.decline_invitation(cy.id, None, space, NOW + 9).await.unwrap();
    assert_eq!(meta.space_access(cy.id, space).await.unwrap(), None);
    meta.invite_member(&owner, space, cy.id, SpaceRole::Viewer, NOW + 10).await.unwrap();
    // Withdrawing a pending invitation is a removal; an account that is only invited cannot "leave".
    assert!(matches!(meta.leave_space(cy.id, None, space, NOW + 11).await, Err(MetaError::NotFound("member"))));
    meta.remove_member(&owner, space, cy.id, NOW + 12).await.unwrap();
    assert!(matches!(meta.remove_member(&owner, space, cy.id, NOW + 13).await, Err(MetaError::NotFound("member"))));

    // A role change returns the row; the role the member has already is no change and writes nothing.
    let changed = meta.change_member_role(&owner, space, ben.id, SpaceRole::Viewer, NOW + 14).await.unwrap();
    assert_eq!((changed.role, changed.accepted()), (SpaceRole::Viewer, true));
    let events = space_events(meta).await.len();
    assert_eq!(meta.change_member_role(&owner, space, ben.id, SpaceRole::Viewer, NOW + 15).await.unwrap().role, SpaceRole::Viewer);
    assert_eq!(space_events(meta).await.len(), events);
    assert!(matches!(meta.change_member_role(&owner, space, eve.id, SpaceRole::Viewer, NOW + 15).await, Err(MetaError::NotFound("member"))));

    // Leaving: the member's own row goes, and the files of the space are untouched.
    meta.leave_space(ben.id, None, space, NOW + 16).await.unwrap();
    assert_eq!(meta.space_access(ben.id, space).await.unwrap(), None);
    assert!(matches!(meta.leave_space(ben.id, None, space, NOW + 17).await, Err(MetaError::NotFound("space"))));

    // A device that was signed out meanwhile no longer acts.
    meta.revoke_device(ana.id, device_ana).await.unwrap();
    assert!(matches!(meta.invite_member(&owner, space, eve.id, SpaceRole::Viewer, NOW + 18).await, Err(MetaError::DeviceRevoked)));

    // The events: every change once, by the right actor, about the space, with typed details and no name in the row.
    let events = space_events(meta).await;
    assert_eq!(
        names(&events),
        [
            "cli.space_created",
            "space.member_invited",
            "space.member_joined",
            "space.member_invited",
            "space.member_declined",
            "space.member_invited",
            "space.member_removed",
            "space.member_role_changed",
            "space.member_left",
        ]
    );
    assert!(events.iter().all(|e| e.target_kind == "space" && e.target_id == Some(*space.as_uuid())));
    assert_eq!(events[0].actor_user, None);
    assert_eq!(events[0].details, format!(r#"{{"quota_bytes":null,"user":"{}"}}"#, ana.id.0));
    assert_eq!((events[1].actor_user, events[1].actor_device), (Some(ana.id), Some(device_ana)));
    assert_eq!(events[1].details, format!(r#"{{"role_to":"editor","user":"{}"}}"#, ben.id.0));
    assert_eq!(events[2].actor_user, Some(ben.id));
    assert_eq!(events[7].details, format!(r#"{{"role_from":"editor","role_to":"viewer","user":"{}"}}"#, ben.id.0));
    assert_eq!(events[8].actor_user, Some(ben.id));
    assert!(events.iter().all(|e| !e.details.contains("Studio")));
}

async fn the_command_line_and_admins_create_and_manage_spaces(db: &Db) {
    let meta = &db.meta;
    let ana = user(meta, "ana").await;
    let ben = user(meta, "ben").await;
    let (boss, admin) = admin_actor(meta, "boss").await;

    // An account does not create team spaces; a bad quota and an owner that is no account are refused.
    assert!(matches!(meta.create_team_space(&by(&ana), &team_name("Mine"), ana.id, None, NOW).await, Err(MetaError::Forbidden(_))));
    let admin_actor = SpaceActor::Admin(admin.clone());
    assert!(matches!(meta.create_team_space(&admin_actor, &team_name("Small"), ana.id, Some(5), NOW).await, Err(MetaError::Invalid(_))));
    assert!(matches!(
        meta.create_team_space(&admin_actor, &team_name("Nobody"), UserId(uuid::Uuid::now_v7()), None, NOW).await,
        Err(MetaError::NotFound("user"))
    ));

    // An administrator creates one with its first owner already a member.
    let space = meta.create_team_space(&admin_actor, &team_name("Straße"), ana.id, Some(8 * 1024 * 1024), NOW + 1).await.unwrap();
    assert_eq!(meta.space_access(ana.id, space).await.unwrap(), Some(SpaceRole::Owner));
    // The administrator has no access of their own to the content.
    assert_eq!(meta.space_access(boss.id, space).await.unwrap(), None);
    // The name is unique in any case; the failed attempts left nothing behind.
    for clash in ["STRASSE", "straße", "Strasse"] {
        assert!(
            matches!(meta.create_team_space(&admin_actor, &team_name(clash), ben.id, None, NOW + 2).await, Err(MetaError::Conflict(_))),
            "{clash}"
        );
    }
    assert_eq!(meta.all_spaces().await.unwrap().len(), 4, "three personal spaces and the one team space");
    // It has a root like every space and a quota of its own.
    let info = meta.space_info(space).await.unwrap();
    assert_eq!((info.seq, info.oldest_seq), (0, 0));
    assert_eq!(meta.tree(space, None, 10).await.unwrap().nodes.len(), 1);

    // An administrator invites; the invitation needs the account's acceptance, and a stale admin is refused.
    let member = meta.invite_member(&admin_actor, space, ben.id, SpaceRole::Editor, NOW + 3).await.unwrap();
    assert!(!member.accepted());
    assert_eq!(meta.space_access(ben.id, space).await.unwrap(), None);
    let stale = AdminActor { password_hash: "$argon2id$changed".to_owned(), ..admin.clone() };
    assert!(matches!(meta.invite_member(&SpaceActor::Admin(stale), space, ana.id, SpaceRole::Viewer, NOW + 4).await, Err(MetaError::Conflict(_))));
    let not_admin = AdminActor { user: ana.id, ..admin.clone() };
    assert!(meta.invite_member(&SpaceActor::Admin(not_admin), space, ana.id, SpaceRole::Viewer, NOW + 4).await.is_err());

    // The command line adds a member who is accepted at once, and changes roles without being a member.
    let cy = user(meta, "cy").await;
    let added = meta.invite_member(&SpaceActor::Cli, space, cy.id, SpaceRole::Viewer, NOW + 5).await.unwrap();
    assert!(added.accepted() && added.invited_by.is_none());
    assert_eq!(meta.space_access(cy.id, space).await.unwrap(), Some(SpaceRole::Viewer));
    meta.change_member_role(&SpaceActor::Cli, space, cy.id, SpaceRole::Editor, NOW + 6).await.unwrap();
    meta.remove_member(&SpaceActor::Cli, space, cy.id, NOW + 7).await.unwrap();

    // A personal space has no members to manage.
    assert!(matches!(meta.invite_member(&SpaceActor::Cli, ana.space, ben.id, SpaceRole::Viewer, NOW + 8).await, Err(MetaError::NotFound("space"))));
    assert!(matches!(meta.list_members(ana.space).await, Err(MetaError::NotFound("space"))));

    let events = space_events(meta).await;
    assert_eq!(
        names(&events),
        ["space.created", "space.member_invited", "cli.space_member_changed", "cli.space_member_changed", "cli.space_member_changed"]
    );
    assert_eq!(events[0].actor_user, Some(boss.id));
    assert_eq!(events[0].details, format!(r#"{{"quota_bytes":8388608,"user":"{}"}}"#, ana.id.0));
    assert!(events[2..].iter().all(|e| e.actor_user.is_none() && e.actor_device.is_none()));
    assert_eq!(events[3].details, format!(r#"{{"role_from":"viewer","role_to":"editor","user":"{}"}}"#, cy.id.0));
}

// ---- the last owner ----

async fn the_last_owner_stays(db: &Db) {
    let meta = &db.meta;
    let ana = user(meta, "ana").await;
    let ben = user(meta, "ben").await;
    let space = team(meta, "Studio", &ana).await;
    let owner = by(&ana);

    // The only owner cannot be lowered, removed or leave, by themselves, by the command line or by anyone.
    assert!(matches!(meta.change_member_role(&owner, space, ana.id, SpaceRole::Editor, NOW + 1).await, Err(MetaError::LastOwner)));
    assert!(matches!(meta.change_member_role(&SpaceActor::Cli, space, ana.id, SpaceRole::Editor, NOW + 1).await, Err(MetaError::LastOwner)));
    assert!(matches!(meta.remove_member(&SpaceActor::Cli, space, ana.id, NOW + 1).await, Err(MetaError::LastOwner)));
    assert!(matches!(meta.remove_member(&owner, space, ana.id, NOW + 1).await, Err(MetaError::LastOwner)));
    assert!(matches!(meta.leave_space(ana.id, None, space, NOW + 1).await, Err(MetaError::LastOwner)));
    assert_eq!(meta.space_access(ana.id, space).await.unwrap(), Some(SpaceRole::Owner));

    // An invited owner does not count until they accept.
    meta.invite_member(&owner, space, ben.id, SpaceRole::Owner, NOW + 2).await.unwrap();
    assert!(matches!(meta.change_member_role(&owner, space, ana.id, SpaceRole::Editor, NOW + 3).await, Err(MetaError::LastOwner)));
    assert!(matches!(meta.leave_space(ana.id, None, space, NOW + 3).await, Err(MetaError::LastOwner)));
    // ... and a pending invitation can be changed and withdrawn without the rule.
    meta.change_member_role(&owner, space, ben.id, SpaceRole::Viewer, NOW + 3).await.unwrap();
    meta.change_member_role(&owner, space, ben.id, SpaceRole::Owner, NOW + 3).await.unwrap();
    meta.accept_invitation(ben.id, None, space, NOW + 4).await.unwrap();

    // With a second owner the first may be lowered; then the second is the last.
    meta.change_member_role(&owner, space, ana.id, SpaceRole::Editor, NOW + 5).await.unwrap();
    assert!(matches!(meta.change_member_role(&by(&ben), space, ben.id, SpaceRole::Viewer, NOW + 6).await, Err(MetaError::LastOwner)));
    assert!(matches!(meta.leave_space(ben.id, None, space, NOW + 6).await, Err(MetaError::LastOwner)));
    // ana is an editor now and can leave freely.
    meta.leave_space(ana.id, None, space, NOW + 7).await.unwrap();
    assert!(meta.list_members(space).await.unwrap().iter().all(|m| m.user == ben.id));
}

/// Two owners lower each other, or leave, at the same time, many times over: never both succeed, and the space always keeps
/// an accepted owner. On Postgres the space's row lock orders them (and, for the commands where no user lock separates
/// the two, nothing else does: the rounds that use the command line twice or two leaves test exactly that lock; the
/// locks of the two accounts already order an owner lowering or removing the other). On SQLite the process's single writer
/// orders them by design, so a mutation that removes the row lock cannot fail there. Both tasks wait at a barrier so
/// that they start their transactions together.
async fn concurrent_changes_never_leave_a_space_without_an_owner(db: &Db) {
    let meta = &db.meta;
    let ana = user(meta, "ana").await;
    let ben = user(meta, "ben").await;
    // Mostly the cases that depend on the space's row lock (0: the command line twice, 3: both leave).
    const HOW: [u8; 6] = [0, 3, 0, 3, 1, 2];
    for round in 0..48 {
        let space = team(meta, &format!("Studio {round}"), &ana).await;
        join(meta, space, &ana, &ben, SpaceRole::Owner).await;
        let (a, b) = (ana.clone(), ben.clone());
        let (m1, m2) = (meta.clone(), meta.clone());
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let attempt = |meta: Meta, actor: SpaceActor, target: UserId, how: u8, barrier: std::sync::Arc<tokio::sync::Barrier>| async move {
            barrier.wait().await;
            match how {
                // The command line lowers the owner it is told to; only the rule can stop both.
                0 => meta.change_member_role(&actor, space, target, SpaceRole::Editor, NOW + 20).await.map(|_| ()),
                // An owner lowers the other one; the loser is also no longer an owner when its turn comes.
                1 => meta.change_member_role(&actor, space, target, SpaceRole::Viewer, NOW + 20).await.map(|_| ()),
                2 => meta.remove_member(&actor, space, target, NOW + 20).await,
                _ => match actor {
                    SpaceActor::User { user, device } => meta.leave_space(user, device, space, NOW + 20).await,
                    _ => Ok(()),
                },
            }
        };
        let how = HOW[round % HOW.len()];
        let (actor1, actor2) = if how == 0 { (SpaceActor::Cli, SpaceActor::Cli) } else { (by(&a), by(&b)) };
        let t1 = tokio::spawn(attempt(m1, actor1, b.id, how, barrier.clone()));
        let t2 = tokio::spawn(attempt(m2, actor2, a.id, how, barrier));
        let (r1, r2) = (t1.await.unwrap(), t2.await.unwrap());
        let ok = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        let refused = |r: &Result<(), MetaError>| {
            matches!(r, Err(MetaError::LastOwner | MetaError::Forbidden(_) | MetaError::ReadOnly | MetaError::NotFound(_)))
        };
        assert!(ok == 1 && (refused(&r1) || refused(&r2)), "round {round} ({how}): {r1:?} / {r2:?}");
        let owners = meta.list_members(space).await.unwrap().into_iter().filter(|m| m.role == SpaceRole::Owner && m.accepted()).count();
        assert_eq!(owners, 1, "round {round} ({how}): {r1:?} / {r2:?}");
    }
}

// ---- guarded writes ----

/// A write through a handle from `Meta::guarded` reads the caller's role again inside its own transaction, so a role lowered,
/// a member removed or a space archived after the server's access check (and before the write) is respected: the write is
/// refused and changes nothing. The same write through the plain handle goes through, which shows it is the guard that refuses.
async fn guarded_writes_check_the_role_in_their_own_transaction(db: &Db) {
    let meta = &db.meta;
    let ana = user(meta, "ana").await;
    let ben = user(meta, "ben").await;
    let space = team(meta, "Studio", &ana).await;
    join(meta, space, &ana, &ben, SpaceRole::Editor).await;
    let dev = device(meta, &ben).await;
    let root = meta.space_info(space).await.unwrap().root;
    let mkdir = |name: &str| {
        vec![craft_cloud_proto::Op::Mkdir {
            node: craft_cloud_proto::NodeId::generate(),
            parent: root,
            name: craft_cloud_proto::Name::new(name).unwrap(),
        }]
    };
    let chunk = craft_cloud_proto::ChunkId::from_bytes([3; 32]);
    let editor = meta.guarded(WriteGuard { user: ben.id, min: SpaceRole::Editor });

    // As an editor every write goes through.
    editor.commit(space, dev, &mkdir("one"), NOW + 3).await.unwrap();
    editor.add_chunk(space, &chunk, 10, NOW + 3).await.unwrap();
    let seq = meta.space_info(space).await.unwrap().seq;

    // The role is lowered after the handle was made: every write is refused, and nothing changed.
    meta.change_member_role(&by(&ana), space, ben.id, SpaceRole::Viewer, NOW + 4).await.unwrap();
    assert!(matches!(editor.commit(space, dev, &mkdir("two"), NOW + 5).await, Err(MetaError::ReadOnly)));
    assert!(matches!(editor.add_chunk(space, &craft_cloud_proto::ChunkId::from_bytes([4; 32]), 10, NOW + 5).await, Err(MetaError::ReadOnly)));
    assert!(matches!(editor.add_chunks(space, &[(craft_cloud_proto::ChunkId::from_bytes([5; 32]), 10)], NOW + 5).await, Err(MetaError::ReadOnly)));
    assert!(matches!(editor.restore_from_trash(space, dev, craft_cloud_proto::NodeId::generate(), NOW + 5).await, Err(MetaError::ReadOnly)));
    assert!(matches!(editor.request_purge(space, craft_cloud_proto::NodeId::generate(), NOW + 5).await, Err(MetaError::ReadOnly)));
    assert_eq!(meta.space_info(space).await.unwrap().seq, seq, "a refused write takes no sequence number");
    assert_eq!(meta.missing_chunks(space, &[craft_cloud_proto::ChunkId::from_bytes([4; 32])]).await.unwrap().len(), 1, "nothing was recorded");
    // A viewer's handle for a read-level minimum still works, and the plain handle is not guarded.
    meta.guarded(WriteGuard { user: ben.id, min: SpaceRole::Viewer }).commit(space, dev, &mkdir("three"), NOW + 6).await.unwrap();
    meta.commit(space, dev, &mkdir("four"), NOW + 6).await.unwrap();

    // Removed: no access at all, which is the answer for a space that is not there.
    meta.remove_member(&SpaceActor::Cli, space, ben.id, NOW + 7).await.unwrap();
    assert!(matches!(editor.commit(space, dev, &mkdir("five"), NOW + 8).await, Err(MetaError::NotFound("space"))));

    // Archived: at most a viewer, so even the owner's write is refused.
    let owner = meta.guarded(WriteGuard { user: ana.id, min: SpaceRole::Editor });
    owner.commit(space, dev, &mkdir("six"), NOW + 9).await.unwrap();
    run(&raw(&db.url).await, format!("UPDATE spaces SET archived_ms = 1 WHERE id = {}", blob_of(db.postgres, space.as_uuid()))).await.unwrap();
    assert!(matches!(owner.commit(space, dev, &mkdir("seven"), NOW + 10).await, Err(MetaError::ReadOnly)));
    meta.guarded(WriteGuard { user: ana.id, min: SpaceRole::Viewer }).commit(space, dev, &mkdir("eight"), NOW + 10).await.unwrap();
}

/// Uploads by editors of one team space, started together: each records a blob and charges the space's byte count, which
/// updates the space's row. A guard that took a weaker lock on that row first would make two of them deadlock on
/// Postgres (one answer is a database error); all of them must succeed.
async fn concurrent_guarded_uploads_do_not_deadlock(db: &Db) {
    let meta = &db.meta;
    let ana = user(meta, "ana").await;
    let ben = user(meta, "ben").await;
    let cy = user(meta, "cy").await;
    let space = team(meta, "Studio", &ana).await;
    join(meta, space, &ana, &ben, SpaceRole::Editor).await;
    join(meta, space, &ana, &cy, SpaceRole::Editor).await;
    for round in 0..20u8 {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let upload = |who: UserId, tag: u8, barrier: std::sync::Arc<tokio::sync::Barrier>| {
            let meta = meta.guarded(WriteGuard { user: who, min: SpaceRole::Editor });
            async move {
                barrier.wait().await;
                meta.add_chunk(
                    space,
                    &craft_cloud_proto::ChunkId::from_bytes([
                        tag, round, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    ]),
                    10,
                    NOW + 5,
                )
                .await
            }
        };
        let (a, b) = (tokio::spawn(upload(ben.id, 1, barrier.clone())), tokio::spawn(upload(cy.id, 2, barrier)));
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert!(a.is_ok() && b.is_ok(), "round {round}: {a:?} / {b:?}");
    }
}
