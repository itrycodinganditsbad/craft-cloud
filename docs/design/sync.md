# Craft Cloud — Design (Phase 1: Sync)

> Self-hosted sync and storage for the Craft App Suite. Repo:
> [itrycodinganditsbad/craft-cloud](https://github.com/itrycodinganditsbad/craft-cloud) (public, MIT OR
> Apache-2.0).


## Goals

Requirements:
- Your files, app settings and asset libraries follow you across machines (**Sync**), then sharing, review
  and team libraries (**Collaboration**).
- **Self-host only.** No hosted service, no account with anyone, nothing phones home.
- Built for **both** a solo artist with a few machines and a small studio (about 2–50 accounts).
- Sync v1 covers all four: project files, app settings, asset libraries, and large media (multi-GB).
- Conflicts: **keep both copies + full history.** No locking in v1.

Assumptions:
- Serves the whole Craft App Suite, not only ArtCraft.
- The apps stay local-first and fully usable offline; the cloud is optional.
- craft-launcher becomes the UI for sign-in and sync status.

Done means: two machines signed into one self-hosted server, an edit in PhotoCraft on one shows up on the
other without the user doing anything, a 20 GB FilmCraft project re-uploads only what changed, and no
sequence of offline edits, crashes, pulled cables, server restores or unplugged drives loses a version.

## Phases

1. **Sync** (this page): accounts, storage, versioned folder sync, selective sync.
2. **Collaboration** (own spec, later): sharing, review comments, team spaces and libraries, maybe
   per-folder locking. Depends on Phase 1.

## Naming and clean room

- Product name **Craft Cloud**, crates and binaries `craft-cloud-*`. Not an ArtCraft Team project, so no
  ArtCraft name, logo or marks as branding (each Craft app's `docs/brand/LICENSE-brand.txt`). Can be renamed
  if it ever moves to `storytold`.
- Our own protocol. Never reverse engineer any other sync service.
- No code copied, ported or paraphrased from GPL/AGPL/LGPL/MPL projects. Permissive dependencies only.
- Full rules live in the repo's `AGENTS.md`.

## Architecture

Approach: **a sync agent that syncs a folder; the launcher only controls it.** Picked over per-app SDKs
(12+ codebases to change) and a virtual drive (per-OS drivers, weak offline).

```
 ┌──────────────── each machine ────────────────┐        ┌──── self-hosted ─────────────┐
 │ Craft apps ──(plain files)──┐                │        │ craft-cloud-server           │
 │                             ▼                │ HTTPS  │  ├─ auth (users, devices)    │
 │ craft-launcher ──local API──► craft-cloud-   │◄──────►│  ├─ metadata (SQLite or PG)  │
 │                               agent ◄─watch─ │        │  └─ chunks (disk or S3)      │
 │                                 └ state DB   │        └──────────────────────────────┘
 │                          ~/Craft Cloud/      │
 └──────────────────────────────────────────────┘
```

**Repos share a protocol, not code** (Craft rule). Everything Craft Cloud lives in the craft-cloud repo.
craft-launcher links none of it; it talks to the agent over a documented local API.

Units, one job each:

| Unit | Kind | Job | Depends on |
|---|---|---|---|
| `craft-cloud-proto` | crate | API types, manifests, opaque chunk IDs, path and name rules, protocol version, errors | nothing in the workspace |
| `craft-cloud-chunker` | crate | Content-defined chunking (FastCDC, versioned parameters) + BLAKE3; pure, no I/O | nothing in the workspace |
| `craft-cloud-store` | crate | `Store` (checks size, hash and format byte) over a `Backend` trait: local disk (own atomic writer), S3-compatible (`object_store`), memory (tests) | proto |
| `craft-cloud-meta` | crate | `Meta`: users, devices, spaces, nodes, versions, per-space blobs, the batch commit, the change feed; one set of queries on SQLite or Postgres (sqlx `Any`) | proto |
| `craft-cloud-server` | app | HTTP API, auth, retention, GC, admin CLI | proto, store, meta |
| `craft-cloud-sync` | crate | The sync engine: scan, diff, upload, download, conflicts. Talks to the disk only through an `Fs` trait and to the server only through a `Transport` trait, so it can run inside a simulation | proto, chunker |
| `craft-cloud-agent` | app | Background process per user: watcher, state DB, real `Fs`/`Transport`, local control API, CLI | sync |

Launcher UI (later, a PR to `storytold/craft-launcher`): sign-in, status icon, selective-sync picker,
history and restore, conflict list. App changes (later, small PRs per app): settings and library paths
can point into the synced folder, and saves are atomic.

## Data model

- **Space**: a user's tree. One per user in Phase 1; team spaces come in Phase 2. Each space has an
  **epoch** (UUID) that changes whenever the server's history for it may have gone backwards (restore from
  backup, restored VM snapshot, `admin reset-epoch`). Every response carries it.
- **Node**: a file or directory with a **stable ID** that never changes. Its location is two mutable
  attributes, `parent_id` and `name`. A rename or folder move is one metadata change, history follows the
  node, and a 10k-file folder moves atomically.
- **The client assigns node IDs** (UUIDv7) when it creates a node, so an offline batch can say "mkdir
  `Shots/`, then add 40 files into it" before the server has seen the directory. IDs are unique per space
  (`(space_id, node_id)` is the key); the server rejects a duplicate and never trusts an ID to reach into
  another space.
- **Version** (files only): `{id, node_id, parent_version, epoch, mtime, exec_bit, device, manifest_id}`.
  Immutable. `epoch` is the space epoch it was committed in.
- **Manifest**: a file's content: chunker parameter set, size, `file_hash` (BLAKE3 of the whole file, checked
  after assembly) and the ordered chunk list. A compact binary encoding (`CCM1`), named by the BLAKE3 of
  that encoding (`ManifestId`), uploaded on its own before the commit. A 20 GB file has about 20,000
  chunks, far too many for a JSON commit body; a commit just references the manifest ID. Identical content
  has the same manifest. Limits: files up to 256 GiB, at most 2^20 chunks per manifest. `file_hash` is the BLAKE3 of the whole file, checked after assembly.
- **Chunk**: up to 4 MiB of bytes. Its **ID is opaque** in the protocol: today the BLAKE3 of the plaintext,
  later possibly a keyed hash for end-to-end encryption, without a protocol redesign. Stored chunks carry a
  format byte (plain today; room for compression or encryption).
- **Chunker parameters** are fixed and named: `fastcdc-v1` = gear table from BLAKE3 key derivation with
  the context `craft-cloud 2026-10-11 fastcdc-v1 gear table`, masks on the top hash bits (21 before the
  average, 19 after), min 256 KiB, avg 1 MiB, max 4 MiB, normalisation level 1. Every version records its parameter set. Changing parameters means a new
  name, never an edit, so old versions stay readable and dedup within a parameter set stays intact.
- **Change feed**: every commit gives the space its next sequence number, and every node it touches records
  that number (`changed_seq`). Deleted nodes stay as tombstones, so "what changed since N" is one indexed
  query that reports deletions too, with no separate log to grow. When tombstones are purged (M4), the space's
  `oldest_seq` moves up; asking for changes older than that, or newer than the current sequence (the server
  was restored), returns `resync_required` and the client does a full listing.
- **Depth**: folders nest at most 256 levels below the root. A mkdir, or a folder move into a parent, deeper
  than that is refused (`too_deep`); files may sit in the deepest folder. (A moved folder's own subtree isn't
  measured, so it can end up deeper; operations below 256 levels are then refused.) Ancestor checks and
  restore walks are one bounded recursive query each, so the cost of an operation doesn't grow with the tree.
- **Names** are NFC-normalised. The server rejects anything that can't exist on every supported OS:
  `/`, `\`, NUL, `<>:"|?*`, control characters, trailing dot or space, Windows reserved names (`CON`, `NUL`,
  `COM1`…), over 255 UTF-8 bytes. **Uniqueness inside a directory is by Unicode full case folding + NFC**,
  so `Foo.psd` and `foo.psd` can't both exist (they couldn't on Windows or macOS anyway).

## Filesystem semantics

| Thing | Rule |
|---|---|
| Symlinks, junctions | Not synced. Listed once as "skipped: link". Never followed. |
| Empty directories | Synced (they are nodes). |
| Executable bit | Synced on Unix-like systems; ignored on Windows. No other permissions. |
| Extended attributes, ACLs, resource forks | Not synced. |
| macOS bundles (`.app`, `.fcpbundle`-style directory packages) | Treated as normal directories. Changes inside one go up in a single batch commit, so other machines never see half a bundle. |
| Hard links | Each path syncs as its own file. |
| Files the agent can't read (locked, permission denied) | Skipped with a reason, retried later. |
| Sparse files | Synced as their full contents. |

## Sync flow

**Root marker**: `<sync root>/.craft-cloud/root.json` holds the space ID and a root ID. If it is missing or
doesn't match (drive not mounted, folder wiped, wrong folder), the agent **stops** and asks; it never reads
an empty folder as "delete everything".

**Detect a local change**
- The watcher plus a full rescan every 10 minutes as a safety net.
- A file counts as **stable** when size and mtime are unchanged across two checks at least 5 s apart and, on
  Windows, it opens with an exclusive share mode (a sharing violation means a writer still has it open).
  A render that writes for 20 minutes is simply not stable for 20 minutes.
- **After hashing, stat again.** If size or mtime moved during chunking, throw the result away and retry
  later.
- **Never block an app's save (Windows).** The share-mode probe opens the file only for a moment and closes
  it straight away. Hashing opens with `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` and relies on
  the re-stat after hashing. The agent never holds a handle that blocks writers, renames or deletes.
- **Racy mtime rule**: the state DB lets an unchanged size + mtime skip hashing only if that mtime is older
  than the scan that recorded it by more than the filesystem's timestamp granularity (2 s on FAT/exFAT,
  1 s fallback when unknown). Anything closer is treated as dirty and hashed.
- **Path first, then file ID.** A file that appears at a path the agent already tracks is a **new version of
  the node at that path**, whatever its file ID. That is what an atomic save looks like (write temp, rename
  over the original; or original → backup, temp → original, delete backup), and it must not break history.
  Intermediate states inside the stability window are never committed.
- **Local moves**: only when the target path did *not* exist before does the agent match by file ID (inode /
  NTFS file ID) plus size and mtime, and send a move of the existing node instead of a delete and a new file.

**Upload**
1. Chunk and hash (streaming; never the whole file in memory).
2. `POST /chunks/missing` with the chunk IDs; upload the missing ones in parallel over HTTP/2. Each chunk is
   its own request, so a cut connection resumes from the next missing chunk. Then `PUT /manifests/{id}`.
3. `POST /commit` with **one or many** operations (new version, create directory, move, delete), each with
   its expected current version. The batch is one transaction: all apply or none. A project folder or a
   bundle appears on other machines in one piece.
   - **Batch cap**: 1,000 operations or 1 MiB of JSON per commit. A bigger change (the first upload of a
     big library) goes in several batches, parents before children, and **does not** appear in one piece on
     other machines. That is accepted.
   - **One conflict fails the batch.** The server answers with every conflicting operation, not just the
     first. The agent resolves those (conflict copies, see below) and resubmits the rest as a new batch.
   - **Bundles** (macOS packages, and any directory with a known package extension) go up only when the whole
     directory is stable, and in one batch when it fits under the cap.

**Conflicts**: the server refuses an operation that doesn't match the current state. Nothing is ever lost;
everything stays in history. Conflict copies are named `name (conflict, <device>, 2026-10-11 14-03).ext`.
- **Edit versus edit** (expected version isn't current): the agent downloads theirs to the original name,
  commits its own as a new node under the conflict name, and tells the user.
- **Name collision** (two devices create `notes.txt` in the same folder offline, or move different nodes to
  the same name; compared by case fold + NFC): the second one to commit gets the conflict name.
- **Move cycle** (A into B on one device, B into A on another): the server rejects the move that would make
  a cycle; the first one stands and the agent reports the other.
- **Move versus move** of the same node: the server's move stands; the local one is redone if it still makes
  sense, otherwise reported.
- **Delete versus edit**: the edit wins and the node comes back. **If its parent directories were deleted,
  the whole parent chain is restored** with them.
- **Move or create into a deleted directory**: same rule, the directory chain is restored and the node lands
  where the user put it.
- The device part of a conflict name is cleaned with the name rules. If the name passes 255 bytes, the
  original stem is shortened at a character boundary, never the extension or the suffix.

**Download**: long-poll `GET /changes?since=N&wait=30`. For each change, reuse chunks already on disk, fetch
the rest, assemble in a temp file on the same volume, check `file_hash`, rename into place.
- **Reused local chunks are re-hashed when read.** The source file may have changed since the state DB saw it.
- **Never overwrite a local file with unsynced edits**: that's a conflict.
- **Rename blocked on Windows** (an app has the file open): keep the assembled temp file, show "waiting for
  <file> to close", retry with backoff. The old file stays usable until then.

**Epoch change**: if a response carries a different epoch from the one in the state DB, the agent does a
**full reconcile**: it lists the server tree and compares it with local files.
- **Missing on the server**: re-uploaded.
- **Different on the server**: if the server's current version was committed in the **new** epoch (another
  device already reconciled it), this is a normal edit-versus-edit conflict and the local copy becomes a
  conflict copy. Otherwise the local file is uploaded as a new version, and the server's copy stays in history.
- **Never replace or delete a local file during a reconcile.**
- Intended consequence: files deleted after the backup point come back. Delete them again if you meant it.

**Mass-delete guard**: if one pass would delete more than 500 files or more than 20 % of the synced files
(either direction), the agent pauses and asks, showing what would go. Both thresholds are settings.

**Offline**: operations queue in the state DB and go up through the same path when the server is back.

**Selective sync**: each folder is **synced** (full local copy) or **cloud-only** (listed but not on disk).
Switching to cloud-only removes local files only after confirming every one is on the server. On a new
device, folders over a size threshold (default 5 GB) start cloud-only and the user picks.

**Settings and libraries** are plain files under `.settings/<app>/` and `Libraries/`. No special path in the
engine; a settings conflict is a normal conflict copy and the app decides what to read.

**Ignored by default**: `.DS_Store`, `Thumbs.db`, `desktop.ini`, `~$*`, `*.tmp`, `*.part`, `.~lock.*`, the
agent's own `.craft-cloud/` folder. A `.craftignore` file adds patterns.

## Server

- One binary, `craft-cloud-server` (axum, tokio, rustls). Linux x86_64 and aarch64 binaries, a Docker image,
  an example systemd unit.
- **TLS**: either give it a certificate (rustls; arrives with packaging in M6), or run plain HTTP behind a
  reverse proxy (M2b). Plain HTTP binds to loopback only unless `behind_proxy = true` is set in the config.
  The proxy is also what protects against slow clients: it must set request header and body timeouts (the
  server itself has none until built-in TLS arrives). The server bounds per-user load itself: at most 4
  manifest uploads at once, at most 16 waiting change-feed requests per space, and missing-chunk lists of at
  most 10,000 IDs. `trusted_proxies` (a list of addresses) decides whose
  `X-Forwarded-For` is believed; without it the peer address is used.
- **Version negotiation**: `GET /api/info` returns the server's protocol version range, chunker parameter
  sets and limits. The agent checks it at login and on every start, and refuses with a clear message if
  there's no overlap.
- **Metadata: SQLite or Postgres**, chosen in the config. Both run the same queries (sqlx `Any` driver) and the
  same test suite. sqlx (MIT/Apache) with **one migration directory per backend**; no query relies on one
  dialect's quirks without a test on both.
  - SQLite (default, solo and small studios): WAL mode, `busy_timeout` set, **one write transaction at a
    time** (a single async lock around every write, `BEGIN IMMEDIATE`); reads use a small pool.
  - Postgres (bigger studios, or when the admin already runs one): a normal pool; a commit runs in one
    transaction that first locks its space's row (`SELECT … FOR UPDATE`), so commits to one space run one at
    a time and compare-and-swap holds under concurrency. Tree and change-feed reads use one
    `REPEATABLE READ` snapshot.
  - Chunk I/O never happens inside a DB transaction, on either backend.
- **Chunks: local disk or S3-compatible**, chosen in the config.
  - Disk (own writer): `chunks/ab/cd/<id>`, written temp + fsync + rename + directory fsync, including any
    shard directory the write created.
  - S3 through `object_store` (Apache-2.0).
  - S3 (AWS, MinIO, Garage, Backblaze B2, Cloudflare R2…): one object per chunk, `<prefix>/ab/cd/<id>`. A PUT
    is atomic, so there's no temp step. Credentials come from the config file or the standard environment
    variables, never from clients.
  - Every chunk is verified on write and on read, on both.
- **Dedup without leaking**: chunks are stored once on disk, but `chunks/missing` answers per space, and
  **quota counts logical bytes per space**. Neither reveals whether someone else has the same data.
- **Uploads and commits check what they reference**: a manifest upload is refused with the list of missing
  chunks unless every chunk it lists is already in the space; a commit refuses a version whose manifest isn't
  in the space (`missing_manifest` conflict). Both checks run inside the write transaction (GC treats
  manifests like chunks), and the agent uploads what's missing and retries. GC and commit share a **generation counter**: GC marks with generation G and
  only sweeps chunks unreferenced at G and not touched by any upload or commit since, so a commit can't
  race a sweep.
- **Accounts**: usernames are unique ignoring ASCII case (stored lowercase), so `Ana` and `ana` are one account.
  `admin` and `member` roles. Passwords Argon2id. Login rate-limited per client address and
  per account. Passwords are 8 to 1024
  bytes; admin CLI commands read them from standard input, never from arguments.
- **Devices**: the agent logs in once with username and password and gets a 256-bit device token, stored
  only as a hash. The device list shows name, created and **last used**. Revoke from the admin CLI or from
  your own device list. `last used` is
  written at most once a minute per device.
- **Setup**: `craft-cloud-server init` writes the config, creates the data directory and the first admin.
  Solo use is that one admin account and nothing more.
- **Quotas**: optional per user, in logical bytes. A commit over quota is refused whole.
- **Retention by thinning**: keep every version from the last 24 h, then one per hour for 7 days, then one
  per day for 30 days; the current version is always kept. Deleted nodes sit in a trash under the same
  rule. All three windows are config.
- **Garbage collection**: daily, mark and sweep with the generation rule above.
- **Backup**: `craft-cloud-server backup <dir>` pauses GC, snapshots the metadata (SQLite online backup, or
  `pg_dump` in one snapshot transaction for Postgres), then copies chunks (immutable, so anything copied
  after the snapshot is a superset), then resumes GC. With S3, `--metadata-only` skips the chunk copy for
  admins who back up the bucket themselves (bucket versioning or replication); GC must stay paused until
  that bucket backup is done, or the admin keeps the bucket's own deleted-object retention longer than the
  GC grace. **Restore** sets a new epoch for every space.
- **Admin CLI**: `user add|remove|reset-password|quota`, `device list|revoke`, `gc`, `verify` (re-hash every
  chunk), `backup <dir>`, `restore <dir>`, `reset-epoch`.
- No web UI in v1. No telemetry, no update check.

API (`/api/v1`, JSON except chunk and manifest bodies; every signed-in response carries the space's epoch in
the `craft-epoch` header, and commits/tree/changes carry it in the body too). Errors are
`{error, message, conflicts?, missing?}` with stable codes (`conflict` 409, `missing_chunks` 409,
`resync_required` 410, `unauthorized` 401, `rate_limited` 429, `invalid` 400, `not_found` 404, `too_large` 413,
`username_taken` 409, `internal` 500; the codes are constants in `craft-cloud-proto::api::codes`). Unknown routes and
methods answer `not_found` / `invalid` (405) the same way:

| Call | Purpose |
|---|---|
| `GET /info` (also unversioned at `/api/info`) | protocol versions, chunker parameter sets, limits (no auth); a client probes `/api/info` first |
| `GET /space` | the signed-in user's space: space ID, root node ID, epoch, current seq, username |
| `POST /auth/login` | username + password + device name → device ID + token (shown once); 10 tries per account and 30 per address per 5 minutes; a wrong password and an unknown user look the same |
| `GET /tree?after_seq=…&after_node=…&limit=…` | full listing for a first sync or a reconcile, in pages of up to 5,000 (cursor = changed seq + node ID) |
| `GET /changes?since=N&after_seq=…&after_node=…&limit=…&wait=30` | changes after N, in pages, long-poll up to 60 s; `resync_required` if N is too old or ahead of the server |
| `POST /chunks/missing` | which of these chunk IDs (up to 10,000) this space lacks |
| `PUT /chunks/{id}` / `GET /chunks/{id}` | one chunk, max 4 MiB, checked by the server; you can only download chunks your space uploaded |
| `PUT /manifests/{id}` / `GET /manifests/{id}` | one manifest (binary); the server checks the ID, the limits and that every chunk is present |
| `POST /commit` | batch of version/mkdir/move/delete operations, one transaction; `409 conflict` listing every conflicting operation (a version whose manifest isn't uploaded is a `missing_manifest` conflict) |
| `GET /nodes/{id}/history` / `POST /nodes/{id}/restore` | list versions / make an old one current (M4) |
| `GET /devices`, `DELETE /devices/{id}` | your devices |

**Client contract** (what the sync agent relies on):

- Probe `GET /api/info` and refuse to sync when the protocol ranges don't overlap.
- The epoch in a response **body** (`commit`, `tree`, `changes`) wins over the `craft-epoch` header; the header is
  read when the request starts and can be older after a long-poll.
- Page a listing by following `next` (`after_seq` + `after_node`) until it is absent, then resume the change feed
  from the `seq` of the **first** page.
- Node and version IDs are made by the client, so a commit retried after a timeout is safe: `node_exists` /
  `version_exists` conflicts for exactly the retried operations mean the first attempt applied.
- A long-poll refused because 16 are already waiting on the space answers `429 rate_limited` with `Retry-After`;
  wait that long before polling again.
- Unknown JSON fields are ignored in both directions, so later protocol versions can add fields.
- After an admin restores a backup they run `craft-cloud-server reset-epoch --all`; agents then reconcile.

## Agent and launcher API

- `craft-cloud-agent` runs per user session. Autostart only if the user turns it on (launchd agent,
  Windows `Run` key, XDG autostart).
- Sync folder default `~/Craft Cloud/`. State DB (SQLite) in the user data dir: per node the server ID, last
  synced version, chunk list, size, mtime, file ID and when it was recorded.
- Temp files go in `<sync root>/.craft-cloud/tmp`, on the same volume so the final rename is atomic.
- **Local control API**: HTTP on `127.0.0.1`, random port, random token per start, written with
  `{port, token}` to an owner-only file in the user data dir. Hardened against web pages reaching it:
  - the `Host` header must be exactly `127.0.0.1:<port>`;
  - any request with an `Origin` header is refused;
  - the token is accepted only in a header, never in the query string;
  - never another bind address, never logged.

  Calls: `status`, `login`, `logout`, `folders` (get/set synced or cloud-only), `history`, `restore`,
  `conflicts`, `pending-deletes` (approve or reject a mass-delete pause), `pause`, `resume`.
- **CLI**: `craft-cloud-agent login <url> | status | pin <folder> | unpin <folder> | history <file> |
  restore <file> <version> | approve-deletes | pause | resume`. Same operations as the local API, so it is
  usable and testable before any launcher work.
- **The launcher integration** is a separate PR to `storytold/craft-launcher`, later. The launcher would also
  be the natural installer for the agent, once maintainers agree.
- **App contract** (separate small PRs per app, later): an option for where settings and libraries live,
  and saves that write a temp file and rename it.

## Errors

- **Transient** (network down, timeouts, `5xx`, server restarting): retry with exponential backoff and
  jitter, capped at 5 minutes. Status shows "Offline, retrying"; local work carries on.
- **Per-file permanent** (name rejected, case collision, too big for quota, permission denied, file locked):
  that file is skipped and listed with the reason; everything else keeps syncing.
- **Account-level** (token revoked, server gone, quota full, protocol mismatch): pause syncing, say so
  clearly, never delete local files because of it.
- **Safety stops** (root marker missing, mass-delete guard): pause and ask. Never resolved automatically.
- **Local disk full**: stop downloads, keep uploads going, tell the user.
- **Corrupt chunk on the server** (`verify` or a read finds a mismatch): the server marks it bad and returns
  an error; any agent that still has those bytes re-uploads them (repair). `verify` reports files that can't
  be repaired.
- **Clocks**: server sequence numbers decide ordering. Local mtimes are used only to notice changes and for
  display.
- Server and agent never panic (repo rules); every request body, list and size is capped.

## Testing

- **Chunker**: golden test vectors pin `fastcdc-v1` exactly (boundaries for fixed inputs never change);
  an insert or delete in the middle changes only nearby chunks; sizes stay within min and max; property
  tests on random data.
- **Names**: property tests that the validator rejects every unsafe form, and that case-fold + NFC
  uniqueness matches expectations for tricky cases (`ß`/`SS`, Turkish dotted I, combining accents).
- **Server**: integration tests against an in-process server, **run once per backend combination**
  (SQLite + disk always; Postgres and S3 in CI through service containers, locally when
  `CRAFT_CLOUD_TEST_PG` / `CRAFT_CLOUD_TEST_S3_*` are set; CI sets `CRAFT_CLOUD_REQUIRE_BACKENDS` so a
  missing backend fails instead of skipping). The S3 test server is SeaweedFS (Apache-2.0), run as a
  container, never linked. Cases: auth, quota, batch commit
  atomicity, `missing_chunks`, GC never removes a referenced or protected chunk while commits run,
  thinning, `backup` + `restore` gives the same state with a new epoch, `resync_required`.
- **Deterministic simulation** of the sync engine: several agents and the server in one process with a
  simulated clock, network and filesystem (`turmoil`, MIT, for the network; an in-memory `Fs` for disks).
  Random seeds drive edits, moves, deletes, disconnects, crashes, server restores and unplugged roots. After
  each run, check against a simple reference model: no version lost, every machine converges, nothing
  deleted that the user didn't delete. A failing seed replays exactly.
- **Scripted scenarios** on top, as readable regression tests: concurrent edit → conflict copy; offline edits
  on both sides; upload cut halfway then resumed; delete versus edit; folder move during an upload; epoch
  change with a newer local file and with another device already reconciled; mass delete paused; Windows
  rename blocked; atomic save (temp → rename over) keeps one node and its history; offline mkdir + files in
  one batch with client IDs; two devices create the same name offline; move cycle; edit inside a remotely
  deleted folder restores the parent chain; a batch over the cap; an app saving while the agent hashes
  (Windows) never fails.
- CI on Ubuntu and Windows (repo `ci.yml`); macOS in release builds, as in the other Craft repos.

## Milestones

| # | Milestone | Status |
|---|---|---|
| M0 | Repo, conventions, CI | DONE (2026-10-11) |
| M1 | `proto` + `chunker` (names, IDs, `fastcdc-v1` with golden vectors) | DONE (2026-10-11, PR #1) |
| M2 | Server core: info, auth, devices, chunks, batch commit, changes, tree, epoch; SQLite + Postgres, disk + S3 | DONE (2026-10-11; M2a storage, M2b server) |
| M3 | Sync engine on `Fs`/`Transport` + simulation harness + agent CLI; two machines sync a folder | TODO |
| M4 | History, restore, trash, thinning, GC, admin CLI, backup/restore | TODO |
| M5 | Selective sync, large-media polish, ignore rules | TODO |
| M6 | Packaging: binaries, Docker, systemd, built-in TLS, self-hosting guide | TODO |
| M7 | Local control API + launcher integration PR | TODO |

## Out of scope for Phase 1

Hosted service; collaboration and sharing; locking; web UI; mobile; virtual files and on-demand download;
end-to-end encryption (not built, but chunk IDs are opaque and stored chunks have a format byte so it can
come later); per-chunk compression (same format byte); a packed small-chunk upload endpoint (HTTP/2 parallel
requests until measurements say otherwise); compatibility with any other sync service.

## Open questions

- Would the `storytold` maintainers take a launcher integration, and the launcher installing the agent?
