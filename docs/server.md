# Running the server

Set up the server first; the agent ([`agent.md`](agent.md)) syncs folders with it.

```sh
cargo build --release -p craft-cloud-server
# Create a data directory (SQLite + local disk) and the first admin. The password is read from stdin.
printf '%s\n' 'choose a password' | ./target/release/craft-cloud-server init --dir ./cc-data --admin you
./target/release/craft-cloud-server serve --config ./cc-data/config.toml
```

The server speaks plain HTTP on `127.0.0.1:8080`. To reach it from other machines, put a TLS reverse proxy
(Caddy, nginx, Traefik …) in front of it:

1. Set `listen` to the proxy-facing address, and set `behind_proxy = true`.
2. Put the proxy's address in `trusted_proxies`. The server refuses to start behind a proxy without it,
   since every client would then share the proxy's address and a few wrong passwords would lock everyone
   out of signing in. If that is really what you want, set `ignore_forwarded_for = true` instead (and leave
   `trusted_proxies` empty: the server refuses both together).
3. Make the proxy set a request header timeout. The server cuts off request bodies that are too slow to
   arrive itself, and limits each account's uploads, so one slow device only slows its own account.

For a non-loopback `--listen`, `init` also needs `--behind-proxy` and the proxy's address, for example
`init --dir ./cc-data --admin you --listen 0.0.0.0:8080 --behind-proxy --trusted-proxy 10.0.0.2`.

For Postgres or S3, edit `[database]` and `[storage]` in `config.toml`.

To cap what one account may store, set `stored_bytes_per_user` (in bytes; there is no cap by default) in
`config.toml`. It counts every chunk and manifest an account uploaded once, and nothing frees space until
garbage collection arrives, including what a refused file stored before it was refused. A file that would go
over it is skipped by the agent, which says so; the rest of the folder keeps syncing. Nearly full counts as
full: with less than 4 MiB (one chunk) left, the agent pauses and says the account's storage is full.
A cap under 4 MiB is refused when the server starts, since not even one chunk would fit. Per-user quotas on
file sizes come later, with history.

Live documents have a cap of their own: `live_bytes_per_user` (in bytes; 1 GiB, `1_073_741_824`, by default) is
the most one account's live documents (recent-file lists and documents of files) may hold together. It counts
every update in their logs, including the ones a snapshot has replaced: those are kept until retention rules
arrive, so the total only goes down when the admin resets a document (`live reset-doc`). An update that would go
over it is refused; the agent keeps it on the machine and tries again later, and the app shows its changes as not
yet on the server. Snapshots of live documents are uploads like files' content, so they count under
`stored_bytes_per_user` instead. An account may also hold updates in at most 10,000 documents of files; the first
change to one more is refused the same way.

To add accounts: `printf '%s\n' 'their password' | craft-cloud-server user --config cc-data/config.toml add name`.
`--config` can come before or after `add`.

Or invite someone, with no password to hand over: `craft-cloud-server user --config cc-data/config.toml invite name`
prints a join string (`cloud.example.org/ABCD-EFGH-JKMN-PQRS`) and its link, good once for 7 days, to enter on their
first computer (`craft-cloud-agent join …`). It makes the account if there is none; they choose its password when
they join. For an account that exists, the invite just adds a computer, without its password. Set
`public_url = "https://cloud.example.org"` (the address computers reach the server at) at the top of `config.toml`
to have the whole join string printed; without it, `invite` prints the code and you put the address in front. A new
invite replaces the old one, and `user invite --revoke name` ends it. Every later computer joins with a code from one
that is signed in (`craft-cloud-agent add-computer`, see [`agent.md`](agent.md)). Codes that don't work are refused alike (wrong,
expired or used), and an address that tries ten in a minute is held off for the rest of it.

## Version history

The server keeps every version of every file; nothing is thinned or removed yet. A signed-in device can list a file's
versions (`GET /api/v1/nodes/{id}/history`) and make an old one current again (`POST /api/v1/nodes/{id}/restore`),
which adds a new version at the end of the file's history rather than rewinding it, so other computers pick it up
through the normal change feed. This needs no setting and no database migration, and an older server answers
`not_found` for both paths. The agent and the apps do not use them yet.

## The trash

A deleted file or folder stays in the trash, and in the server's records, until it is removed for good. A signed-in
device lists the trash (`GET /api/v1/trash`), brings a node back (`POST /api/v1/trash/{id}/restore`, which names it
differently if its name has been taken meanwhile) or takes it out of the trash (`DELETE /api/v1/trash/{id}`). Taking
it out only marks it: **nothing is deleted from the server yet**, no version or chunk is freed, and the node can't be
brought back from the trash afterwards. The only setting is the number of days a deleted node waits, shown to clients as
`purge_after_ms`; it removes nothing yet:

```toml
[retention]
trash_days = 30   # 1 to 3650
```

A file brought back gets a new version with its old content, so another computer's delete that was sent just before can't delete it again. Update the agents with or soon after the server: an older agent can delete a *folder* a second time if that folder was brought back right after a delete whose answer it never heard (the folder is empty and can be brought back again).

The change adds two columns and an index to the metadata (migration 0009 in the proposal). An older server refuses the
upgraded database and rejects the `[retention]` section, so take a backup before upgrading and do not roll back.

## Checking the storage

`craft-cloud-server verify --config cc-data/config.toml` checks the database and the blob storage against each other
and prints what it finds; `--json` prints a machine-readable report and `--skip-chunk-reads` skips reading every chunk
object (which hashes it, and takes as long as reading all your data). It changes nothing and exits with an error if
it found a problem. It opens a SQLite database read-only: the database file and its WAL keep their bytes, also after a
crash that left committed data in the WAL, which it reads. Opening may leave an empty `-wal` and a `-shm` file behind
(SQLite's rebuilt index of the WAL, not data). The report also says how many manifests have a complete reference index, which is what a later
cleanup would need before it could free anything. After upgrading, run `craft-cloud-server index build --config
cc-data/config.toml` once: it indexes the manifests stored before the index existed, writing only that index, in small
steps, and it can be stopped and run again. Nothing in the server frees storage yet.

The change adds two tables to the metadata (migration 0010 in the proposal). An older server refuses the upgraded database,
so take a backup before upgrading and do not roll back.

`craft-cloud-server gc --dry-run --config cc-data/config.toml` inventories stored chunks and manifests, compares them with
the database and prints a preview; `--json` prints schema version 1. It changes no data, on SQLite or on PostgreSQL.
Like `verify`, it opens a SQLite database that exists already and creates nothing; opening a WAL-mode database may leave
an empty `-wal` file and a `-shm` file behind (SQLite's rebuilt index of the WAL, not data), and the database file and
the WAL's contents stay as they were, also after a crash that left data in the WAL. Store objects with no `blobs` row are orphan candidates, but need two runs apart to establish a future
first-sight grace period. Recorded chunks no complete indexed manifest names are candidates only when every manifest index
is complete and database checks pass. They come from one read-only database snapshot; the store listing is a separate,
time-point diagnostic. A future collector must recheck roots before claiming a chunk. Current versions, saved versions,
trash items, live snapshots and import snapshots protect their chunks. Unrooted manifests are protected because they may be live document assets. Without
`--dry-run`, the command refuses: deleting is not implemented yet. Listing errors fail the command; they do not mean the
store is empty. Nothing is deleted.

## Restoring a backup

With the server stopped, restore the database and the chunk storage from the backup, then run
`craft-cloud-server reset-epoch --config cc-data/config.toml --all`, and only then start `serve` again. Every
account then starts a new epoch, so each synced machine reconciles with the restored state instead of trusting its
own change counter. `reset-epoch --config … name` resets one account. (A reset run while the server is up still
works: open live connections end at their next ping, within 30 s, and connect again.)

## A full recents document

An app's recents log holds at most 16 MiB of updates per account. Once it is full, machines keep their new recents
locally and say so. `craft-cloud-server live --config cc-data/config.toml reset-doc name wordcraft` empties that log;
every machine then sends up what the emptied log lacks, so nothing any machine holds is lost, and file syncing isn't
affected. A document whose own history is over 16 MiB stays full until compacting the log is supported. An account
may have 64 live documents holding updates and 1,024 in all (one per app today); past that, new ones stay on the
machines. Live documents can't be deleted yet.
