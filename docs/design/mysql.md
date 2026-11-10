# MariaDB and MySQL metadata backend

> Design draft. No implementation approved. Goal: decide whether Craft Cloud should support a MySQL-family metadata database alongside SQLite and Postgres.

## Recommendation and scope

Recommend **MariaDB only, initially 10.5 or later**, with a pinned minimum chosen after SQLx 0.9 integration testing. MariaDB 10.5 added `INSERT ... RETURNING` and `DELETE ... RETURNING`; verify exact supported forms, version boundaries, and SQLx 0.9 behavior before implementation. These forms reduce workarounds for insert/delete result retrieval. MySQL 8.0 has no general DML `RETURNING`; it needs extra selects or transaction redesign for every current use. Supporting both engines means testing two subtly different servers and maintaining a compatibility matrix. MariaDB-only gives a smaller first implementation. If MySQL users are a required audience, reconsider after a prototype establishes shared SQL compatibility. MariaDB compatibility claims and version requirements remain **to verify**.

Do not imply support for arbitrary MySQL-compatible forks. Pin server versions and storage engine requirements; require InnoDB for transactional tables.

## Current backend surface

`crates/meta/src/lib.rs:23-24,98,122,140-143,256` uses SQLx `AnyPool`, installs default drivers, chooses pool sizes by `Dialect`, and stores `Any` transactions. Root `Cargo.toml:56` enables SQLx 0.9 `any`, `sqlite`, and `postgres`, but not `mysql`. `crates/meta/src/schema.rs:10-25` embeds six migration files per existing dialect; `schema.rs:29-79` migrates each in a transaction and serializes only Postgres migrations with an advisory lock. SQLite supports one process per file by current comment at `schema.rs:42`.

Line references are to this branch, which is based on `main`. The open team-space and compression pull requests add more SQL, among it `crates/meta/src/usage_estimate.sql` and further `ON CONFLICT`, `RETURNING` and advisory-lock uses (about 11, 11 and 4 in the full stack); count again on the base the work starts from.

## Dialect mapping

Line references below point to current source. The same source usually contains several SQL fragments, so review the full function before changing it.

| Current feature and uses | MariaDB proposal | MySQL proposal / concern |
|---|---|---|
| `ON CONFLICT DO NOTHING`: `crates/meta/src/blobs.rs:79,87,197,201`; live document inserts/upsert: `crates/meta/src/live.rs:171,260,406` | `INSERT ... ON DUPLICATE KEY UPDATE` can suppress duplicate-key errors, but has no exact `DO NOTHING` equivalent. A no-op assignment can still trigger update semantics, affect affected-row counts, fire triggers, and obscure which unique constraint caused collision. Verify SQLx row counts and whether guarded insert/select is safer. At `crates/meta/src/live.rs:406`, preserve atomic increment semantics explicitly. | Same syntax family, same semantic differences. MySQL-specific SQL, no partial-index conflict target support. `nodes_live_names` uses partial unique index (`crates/meta/migrations/postgres/0001_init.sql:41`, `crates/meta/migrations/sqlite/0001_init.sql:41`); redesign as generated/nullable key or transactional check. Generated-column/index design to verify for MariaDB minimum. |
| `RETURNING`: `crates/meta/src/blobs.rs:110,201`; `crates/meta/src/join.rs:170,195`; `crates/meta/src/live.rs:497,559` | Candidate `INSERT`/`DELETE ... RETURNING` on MariaDB 10.5+, but UPDATE support and SQLx decoding/driver behavior need verification. For unsupported shapes, do the mutation and a follow-up `SELECT` within the same transaction while holding the relevant row lock. | No DML `RETURNING` in MySQL 8.0. Rewrite update-return-value in `crates/meta/src/blobs.rs:110` and `crates/meta/src/live.rs:559` as locked transaction plus select; rewrite invite consumption in `crates/meta/src/join.rs:170,195` using conditional mutation and row-count checks with same-transaction result reads. Preserve one-use atomicity under concurrency. |
| `FOR UPDATE`: `crates/meta/src/tree.rs:32-35` (`for_update` branch) | Supported by InnoDB in a transaction. Ensure statements use InnoDB and acquire lock in intended transaction. | Same broad approach. Under default REPEATABLE READ, range scans and gap locks may lock more than expected. Prefer exact primary/unique-key predicates and test contention. |
| Advisory locks: `schema.rs:36,65` | `GET_LOCK(name, timeout)` / `RELEASE_LOCK(name)` are connection-scoped, not transaction-scoped. Hold one physical connection for acquire, migration, release. Handle timeout (`NULL`/failure) as a clear startup error. Verify lock name length and MariaDB semantics. | Same API family, but lock behavior differs across MySQL versions and server settings; verify exact semantics and return values. Never return a still-locked session to pool; detach/close as Postgres path does at `schema.rs:65-70`. |
| Placeholders and driver | Existing queries use `$1` positional binds; SQLx `Any` may rewrite placeholders, but test MySQL driver behavior first. | Prefer SQLx `MySql` concrete queries if `Any` does not support the feature or argument/row types. |
| Types and names | Convert `BYTEA` to `BLOB`/`VARBINARY` sized to each encoded ID/hash; keep payloads needing larger values in appropriate blob type. `BIGINT` maps signed 64-bit values, matching Rust `i64`; audit unsigned conversions. Use `utf8mb4`. Use explicit binary/case-sensitive collation for `username`, `name`, and `fold_key` where byte semantics are required. Path uniqueness is currently on folded key, so collation must not silently fold or normalize it again. Exact collation and binary type sizes **to verify**. | Same general mapping. MySQL's default collations are often case-insensitive; never rely on defaults for path identity. Long indexed path/name values can exceed InnoDB index byte limits under `utf8mb4`; current `nodes_live_names` index spans `space_id`, `parent`, `fold_key` and will need bounded key encoding or a separate digest with collision-safe equality check. Exact limits depend on row format/server settings: **to verify**. |
| Transactions and isolation | Use explicit transactions around multi-statement replacements for `RETURNING` and conflict handling. InnoDB default REPEATABLE READ differs from Postgres behavior; gap/next-key locks may affect concurrent insert and range queries. Set/assume isolation deliberately and test concurrent commit, invite, quota, and live-doc operations. | Same. Do not rely on Postgres serial/advisory-lock behavior as equivalent to row locks. |

`ON CONFLICT` appears at seven source lines; `RETURNING` appears at six source lines in `blobs.rs`, `join.rs`, and `live.rs`. These counts do not include any missing usage-estimate SQL. Search all SQL when implementing, including migrations and tests.

## Migrations and data safety

MySQL-family DDL commonly commits implicitly. Existing `schema.rs` behavior (each whole migration wrapped with DDL and `schema_version` insert in one transaction) cannot promise atomic rollback on these servers. A crash or DDL error can leave schema changes without a matching version row.

Before first migration and every upgrade, require a restorable database backup/dump. If Craft Cloud owns the backup step, the server refuses to migrate until a backup is confirmed. If the backup is the operator's job, the install guide makes it mandatory and the startup error after a failed migration names the recovery action. A warning alone is not the never-lose-data guarantee.

Use small, idempotent migration steps and a durable progress record per step. On restart, inspect actual schema state and resume only when every completed/uncompleted step is unambiguous. If schema differs from recorded progress, stop with an actionable “migration incomplete; restore backup or use documented repair” error. Never guess, drop/recreate user tables, or mark an uncertain step complete. Keep old data readable until replacement is verified. Design and test recovery from process termination after each DDL statement before release.

Add MariaDB migration files alongside existing SQLite/Postgres sets, registered in `schema.rs`; do not generate dialect SQL by blind string substitution. Migration lock must cover version inspection through completion. Several servers must use the same named lock and connection. Verify DDL online/locking behavior per supported server version.

## Code structure and estimate

Keep public `Meta` methods shared. First prototype whether SQLx `Any` 0.9 handles MariaDB connection options, bind types, `RETURNING`, row counts, and parameter rewriting. If it does, add `Dialect::MariaDb`, driver feature, URL parsing, pool configuration, and per-dialect migration list. Isolate differing SQL behind a small dialect module with named builders for insert-ignore/upsert, returned mutations, migration lock, and schema DDL. Do not build a general SQL templating framework.

If `Any` cannot cover required behavior cleanly, use a backend-specific connection/query abstraction at the metadata boundary; avoid copying every metadata method. Keep shared transaction logic and branch only at statements whose semantics differ. Query macros are not currently the pattern in the touched functions.

Expected implementation footprint: root `Cargo.toml` and `Cargo.lock`; `crates/meta/Cargo.toml`; `crates/meta/src/lib.rs`, `schema.rs`, likely new dialect/SQL helper module, and query-bearing `blobs.rs`, `join.rs`, `live.rs`, `tree.rs`, plus any hidden usage-estimate SQL once located; new MariaDB migration set; server config parsing and examples; backend tests and CI workflow; `docs/server.md` and VPS setup guidance. Roughly 20-35 query sites across current SQL, transaction, migration, test, and integration code. Estimate 5-8 engineering days for MariaDB-only prototype through CI-ready implementation, excluding migration recovery design, review, and release validation. Both engines likely add 3-5 days plus ongoing dual-server verification. Estimates are rough.

## Several servers on one database

`docs/server.md` already documents a shared Postgres database as a deployment option; read its current `[database]` section before implementation. For MariaDB, require distinct server data directories and shared database schema, same migration-lock name, and compatible binary versions. Serialize migrations with connection-scoped named lock. Normal row operations must remain safe across processes through database transactions and constraints, not process-local locks. Test two servers starting together and concurrent writes. Document that two servers using the same local SQLite file remain unsupported.

## Licences

Root manifest currently enables SQLx 0.9 `any`, `sqlite`, and `postgres` (`Cargo.toml:56`). `Cargo.lock` already contains `sqlx-mysql 0.9.0`, pulled into the lock graph by SQLx's declared packages; its recorded dependencies include `sha1 0.11.0`, `sha2 0.11.0`, `digest 0.11.3`, and shared SQLx/core crates (`Cargo.lock:5571-5592`). Lockfile membership does not prove feature is compiled or that every license passes.

`AGENTS.md` permits MIT, Apache-2.0, BSD, ISC, Zlib, Unicode, CC0, and named exceptions. `xtask/src/licenses.rs` enforces this allow-list from `cargo tree`, including build and dev dependencies. Before enabling MySQL feature, inspect normalized manifests for `sqlx-mysql` and every newly activated transitive dependency, then run `cargo xtask licenses` with feature enabled. No dependency license has been verified by this draft; mark all newly activated crate licenses **to verify**. Any license outside allow-list blocks adoption unless maintainers explicitly revise policy.

## Tests and CI

Extend the existing `.github/workflows/ci.yml` `backends` job, currently Postgres and S3, with a pinned MariaDB service and health check. Add `CRAFT_CLOUD_TEST_MYSQL` (or clearer MariaDB-specific `CRAFT_CLOUD_TEST_MARIADB`) URL. Keep optional local backend tests skippable; CI sets `CRAFT_CLOUD_REQUIRE_BACKENDS=1`, so missing service/config becomes failure. Cover all metadata integration tests, migrations from empty/current schema, concurrent migration startup, DML races that replace `RETURNING`/`ON CONFLICT`, duplicate and case-sensitive names, long paths, quota atomicity, invite one-use, server restart after every simulated partial DDL step, and two-server shared database use.

A third backend increases CI time and upkeep: service startup, image/version pinning, schema matrix, and regression coverage for every metadata operation. Avoid multiplying every unrelated platform job; keep backend matrix in its existing dedicated job. If both MySQL and MariaDB are supported, test both on every backend CI run or support claims lack evidence.

## Install, config, and data movement

`[database]` should accept a MariaDB URL such as `mysql://user:password@host:3306/database` (SQLx URL spelling and TLS options **to verify**), plus the existing SQLite/Postgres forms. Use environment-provided secrets in examples; never log URL credentials. Validate scheme against selected dialect and reject unsupported schemes with actionable config error.

Add install-time backend choice in VPS guide: SQLite default/easiest; Postgres; MariaDB for hosts already operating it. Provide backup, database/user creation, charset/collation, permissions, TLS, and restore steps. Avoid implying MariaDB required for a single-server install.

No automatic cross-backend move initially. State that changing URL does not move metadata or chunks. Either provide a separately scoped export/import tool later (with consistency snapshot, verified counts/hashes, rollback, and cutover procedure) or document a supported offline manual migration. Do not tell users to point an existing server at an empty backend.

## Effort, risks, and decision test

Proceed only if maintainers have a concrete user group, such as NAS deployments already running MariaDB and unwilling or unable to run Postgres. Largest risks: implicit-commit migration recovery, exact conflict semantics, case-sensitive path uniqueness, session lock lifecycle, and query behavior hidden by SQLx `Any`. Every future schema migration adds a third dialect and a recovery test. Supporting MySQL too adds another product/version matrix.

Recommendation: prototype MariaDB-only SQLx `Any` support, two-server migration lock, and one representative metadata transaction first. Stop if license check fails, if `Any` requires broad query duplication, or if DDL recovery cannot meet never-lose-data rules. Only then estimate full implementation and seek approval.

## Open questions

| Question | Recommendation | Evidence needed |
|---|---|---|
| MariaDB only, MySQL only, or both? | MariaDB only, initially. | User demand; compatibility test on chosen minimum version. |
| Minimum MariaDB version? | Start evaluation at 10.5; do not promise until verified. | SQLx support, DML `RETURNING` forms, lock and DDL behavior. |
| What is authoritative metadata backup before upgrade? | Require verified, restorable dump before migration. | Define operator workflow and whether setup tool can enforce it. |
| Can SQLx `Any` support all required SQL cleanly? | Keep it only if representative prototypes pass without semantic hacks. | Connection options, binds, row counts, `RETURNING`, error behavior. |
| How preserve partial unique path-name rule? | Case-sensitive bounded key plus collision-safe check. | DDL design for MariaDB and index budget. |
| Cross-backend migration? | No tool in initial backend release. | Demand and consistency-safe export/import design. |
