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
| Executable bit | Synced on Unix-like systems. Windows has none: an edit made there keeps the bit the file had, and a new file from Windows isn't executable. No other permissions. |
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
  A tracked folder at its place whose recorded local ID is gone, or is on a folder at an untracked place with no
  link in it (renamed, with a new folder made under the old name), takes the ID it has now (not while a folder
  can't be listed), so a later rename of it is a move.
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
  the whole parent chain is restored** with them. If the chain can't come back where it was (a live node
  took a folder's old name), the edit is uploaded as a new file in a newly created folder instead.
- **Move or create into a deleted directory**: same rule, the directory chain is restored and the node lands
  where the user put it.
- The device part of a conflict name is cleaned with the name rules. If the name passes 255 bytes, the
  original stem is shortened at a character boundary, never the extension or the suffix.

**Download**: long-poll `GET /changes?since=N&wait=30`. For each change, reuse chunks already on disk, fetch
the rest, assemble in a temp file on the same volume, check `file_hash`, rename into place.
- **Reused local chunks are re-hashed when read.** The source file may have changed since the state DB saw it.
- **Never overwrite a local file with unsynced edits**: that's a conflict.
- **Rename blocked on Windows** (an app has the file open): keep the assembled temp file, show "in use, retried"
  for <file>, and land the kept file on a later pass if its version is still current, without downloading again.
  The old file stays usable until then.

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

### Engine rules

The engine (`craft-cloud-sync`) is pure logic over an `Fs` and a `Transport` trait. It keeps a **remote
mirror** (the server tree as last seen) and a **base** (what both sides agreed on, per node: place, version,
size, mtime, local file ID, content hash, and when it was recorded). One pass pulls changes, applies remote
changes locally, then pushes local changes. These rules came out of the simulation; each one fixed a seed.

- **Content is checked before anything is overwritten or deleted.** Size and mtime are only a quick filter.
  Before a remote change replaces or removes a local file, the file is hashed against the base hash. A file
  that can't be read (another program holds it locked) never counts as unchanged.
- **Content streams.** The `Fs` trait opens files for reading, reads ranges, and writes through a temporary
  writer that lands atomically. Uploads chunk the file as a stream, then read back only the chunks the server
  lacks, each checked against its ID (a mismatch means the file changed: retried next pass). Downloads check
  every chunk and the whole-file hash before the file lands. No file is ever held whole in memory.
- **Unreadable files are skipped** with a reason in the report and retried next pass.
- **Racy and replaced files are hashed.** When size and mtime match the base but the file's ID changed
  (replaced), or its mtime is within `racy_window_ms` (default 2 s) of when the base was recorded, the
  content is hashed to decide whether it changed.
- **Case twins**: a local name that differs from an incoming one only by case or normalisation is moved aside
  before a write, mkdir or rename, so it's never overwritten. During a scan, the tracked name wins and the
  newcomer is skipped with a reason. A link skipped that way still hides what is tracked at its name (in a
  folder the user renamed neither name is tracked yet, so a case twin can win the name).
- **Moves name the server's location**: a Move op's source is where the remote mirror has the node.
- **Moved on both sides**: the server's move stands. The local copy is found by local file ID and moved to the
  server's place, hopping through a conflict name when one path contains the other or the rename is
  case-only. Folder moves and new folders are ordered by target depth.
- **A tracked node that lost its place** (its folder was moved aside this pass) is looked up by local ID the
  same way. If it isn't found, it counts as deleted here, and its base follows the server's location so the
  push can send the delete.
- **Tracked occupants**: before a node takes a path another tracked node holds (a new node, a server move, a
  download), that one is settled first: removed if it was deleted on the server and holds the content both
  sides agreed on (decided by content, as for a delete), otherwise moved aside. A tracked folder that holds a
  tracked file or folder that turned into a link is never settled; the newcomer waits. Settling isn't held
  back by the mass-delete guard: nothing is lost, every removed version is in the server's history.
- **Folder adoption**: an untracked local folder at the server's move target is adopted, unless another
  tracked folder owns its ID.
- **Edit wins over delete.** An Update of a node the server deleted is retried once with the server's current
  version. If the node still can't come back (its folder's old name is taken now, say), the engine forgets the
  node and its dead folders, and the next pass uploads the file as a new one.
- **Deleted folder with new local files**: the engine forgets it and the push creates it again. A deleted
  folder that still holds an edited tracked file stays, and that file's upload restores it.
- **Refused ops** are dropped from the batch and the rest resubmitted. A Move refused with NameTaken moves the
  local target aside. A refusal that names no operation in the batch fails the pass instead of looping.
- **Edit wins and parents**: new content arriving in a folder deleted here recreates the folder.
- **Downloads land only on what was checked**: the local file is stat'ed before it is verified, and
  `Fs::finish_write` lands the download only if the path still holds exactly that file (or nothing, when it was
  cleared for the download), as one compare-and-replace: on Linux and macOS the download and the file are
  exchanged in one step and the one that came out is compared, then exchanged back if it isn't the expected
  one; on Windows and filesystems without an exchange the file is first moved to a name next to it (never
  replacing anything), checked, and put back if it isn't. If an app saved in between, the download doesn't land,
  the base keeps the old version, and the next pass sees an edit on both sides.
- **A landing in steps survives a crash**: the moved file's name and both files' stats are recorded in the
  agent's temporary folder first. A crash between the steps leaves both files on disk, never neither; before the
  next pass the moved file goes back if nothing took its place, or goes if the download is in place, and is
  otherwise left as a file the scan syncs (it may be a save; nothing removes such files blindly).
- **Rename blocked**: a file a program holds open (Windows), or that may not be replaced, keeps the download
  in the temporary folder, keyed by node, version and manifest; the node is skipped with the reason ("in use by
  another program" for a sharing violation on Windows, "read-only here, not replaced" for a Windows read-only file,
  which is the user saying "don't change this" and is never replaced, "can't be replaced here (no permission)" for a
  file or folder that may not be written on Unix) and a later pass lands the kept file without downloading again,
  as long as that version is still current and the kept file still hashes to the version's file hash (otherwise it
  is downloaded again). A download that met a changed file is kept the same way. Kept downloads of versions that
  are no longer current are dropped.
- **A reconcile cut short stays a reconcile**: after an epoch change the state remembers that a reconcile is
  pending until a pass has applied the server's tree, so no later pass trusts the cleared base.
- **Same content is no conflict**: a file changed on both sides whose local content equals the server's new
  version (a lost commit reply, the same edit on two machines) takes the server's version, with no conflict
  copy.
  It also covers an untracked file with the server's content already at the server's place: it is adopted,
  unless its file ID belongs to another tracked file the server still has (the user moved that one there): two
  nodes never share one file. The moved file then becomes a conflict copy even though it holds the same content
  (the server holds that content on two nodes); accepted.
- **Same content is no change**: a new server version whose manifest and exec bit equal the base's (a file saved
  again unchanged elsewhere) is recorded without touching the local file: no download, no hash, and no conflict
  copy even for a file that can't be read here. A move that comes with it applies as usual.
- **Deleted folders and ignored files**: a folder the server deleted is removed together with any OS
  metadata files in it (`.DS_Store`, `Thumbs.db`, `desktop.ini`). If it holds other ignored files (`~$*`,
  `*.tmp`, `*.part`, `.~lock.*`, which may belong to an open document), it stays on disk, forgotten. A
  tracked folder the server deleted that stands in a newcomer's way is removed the same way if bare, moved
  aside otherwise. Known gap: once the ignored files go, the empty folder is created on the server again
  (a "deleted remotely, kept locally" marker comes with the agent's state DB).
- **Folders holding only ignored files don't sync**: a new local folder whose whole subtree is ignored names
  isn't created on the server. An empty new folder still is.
- **Moved files are checked like any other**: the edit check of a moved file uses the racy and replaced-file
  rules.
- **Filesystem errors are per node.** A filesystem error while applying one remote change (a file an app holds
  open, a folder that can't be listed, a rename or write that fails) skips that node with a reason in the
  report. Its base stays as it was, so the next pass tries again, and every node below it in the server's tree
  waits for it. Only a sync root that can't be listed, and server errors, fail the pass.
- **Folders with unknown contents**: a folder the scan can't list (or that vanished during the scan), or one
  nested deeper than the tree allows, hides its contents. Nothing tracked inside it counts as missing, so the
  push never deletes it on the server. A new folder that can't be listed isn't created on the server until it
  can be.
- **Edited there, moved here**: a tracked file missing from its base path while the server has a new version
  is looked up by local ID, or through the nearest folder above it that is found by local ID (a file an atomic
  save replaced). A path another tracked node holds doesn't count. Unchanged: it takes the server's version
  where it is, or where the server moved it if the server moved it too, and the push sends the local move.
  Same content as the server's: adopted. Edited: it becomes a conflict copy next to where the user put it, and
  the server's version lands at the same place. Not found: it was deleted here, and the edit wins.
  Ignored names don't count: a file renamed to one (an app's temp name mid-save) isn't the tracked file moved.
- **A relocate is recorded at once**: the base follows a file moved to the server's place before its download
  starts, so a download that fails doesn't make the next pass take the unchanged file for a conflict.
- **The mass-delete guard covers both directions**: when the server's deletes would remove more tracked files
  here than the thresholds allow, all of them wait for approval. They stay on disk and in the base, so nothing
  goes back up, and the report lists them as pending local deletes. Approval releases both directions.
- **A first sync cut short starts over**: the space is recorded only after the first full listing succeeds,
  so a failed listing doesn't turn the next pass into a reconcile.
- **The engine never acts on a link.** It never renames, removes, or writes onto or through a symlink,
  junction or special file, whatever the reason: a server change, a conflict copy, a case twin, or making a
  folder. A change that would have to (to the linked path, anything below it, or a name that clashes with it)
  is skipped with a reason and waits. Server deletes below a link wait too. A tracked file or folder that
  turned into a link (or a name this computer can't use) is never treated as deleted, and once it is a real
  file or folder again, the waiting changes apply like any others.
  Such a node is also found through a folder above it that the user moved (by local ID). A folder the
  server deleted that still holds one stays tracked until it is settled, and the push sends nothing for a
  node the server no longer has.
- **Multi-step changes check first.** A change that takes more than one step (a rename through a temporary
  name, a conflict copy and then the server's version) checks everything it will need before the first
  step. If any step would act on a link, nothing is done and the change waits.
- **Deleting is decided by content.** A local file the server deleted is removed if it holds exactly the
  content both sides agreed on, even if its time or file ID changed. Only different content counts as an
  edit, and an edit wins over the delete.
  A folder at a deleted folder's path that another tracked folder owns (by local ID) isn't the deleted one.
- **Edits are decided by content too.** A file that only got a new time or file ID (saved again with the same
  bytes, put back from a copy) isn't an edit, so a new server version simply replaces it, with no conflict copy.
- **Approval covers what was shown.** A mass-delete approval releases exactly the deletions that were listed
  (both directions). Deletions that appear afterwards wait for their own approval. Approvals given before a pass
  add up.
- **Checkpoints**: the state is saved during a pass at safe points (it is consistent at each, as after a failed
  pass): once the server's changes are applied, before a commit that deletes files, and after every commit is
  recorded. A crash loses at most what the pass did since its last checkpoint. A checkpoint that can't be saved
  stops the pass, so no commit goes without its record.
- **Our own deletes stay ours**: the files a commit deletes are written down, with the version each delete
  expected, before it goes, and cleared when it is answered. If the answer never came (a crash, a cut connection)
  and the server's tree lacks a node, or has it at another version (an edit elsewhere won over the delete and
  brought it back), the delete went through: the node is let go before anything else and nothing local is
  removed or replaced for it. The push sent the delete because the file was missing here, so whatever is at its
  place now isn't that file, and it goes up as new; a node brought back arrives as a new one at its own place.
- **Conflict names are free names**: a conflict copy never takes a name a tracked node has as its place, so
  settling that node later never touches the copy.
- **Crash recovery.** After a crash, the next passes redo the lost bookkeeping. Content already on both sides
  is adopted where the engine can tell. Where it can't (a file the crashed pass touched and the user then
  locked, moved or edited), it keeps a conflict copy; a delete the crashed pass had applied can also come
  back. Nothing is lost. The simulation crashes machines at random calls, between filesystem steps (also between
  the steps of a landing) and right after a commit applies on the server, before its answer; a crashed machine
  restarts from its last checkpoint. After a crash, an untracked file already at the server's place
  with the server's content is adopted before a file found by its local ID moves there; the file found by ID
  stays where it is and goes up as a new file.
- **Known gaps, for the agent milestone (M3b)**: a tracked folder the user moves into a folder the scan can't
  list looks deleted, so the push deletes it on the server (its files go up again as new ones once the folder
  can be listed; history keeps the old versions). While any folder can't be listed, a node that has to be
  found by local ID waits. After a node is skipped partway (moved aside, then its download failed), the push
  can send the move aside as a user move, so the server node takes the conflict name. If the user renames a
  folder holding a link while the server adds something to that folder, the engine makes the folder again at
  its old place and the node behind the link is deleted on the server; the file goes up as new once it is
  real again, and history keeps the old version.

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

### The agent, first cut (M3b1)

M3b is split like M3: **M3b1** is an agent you run by hand, **M3b2** makes it run on its own (watcher,
stability window, daemon loop with backoff, simulated network). M3b1 is done when two machines sync one
folder through a real server by running `craft-cloud-agent sync`.

- **`apps/craft-cloud-agent`**, a library plus a thin `main.rs`, like the server. Modules: `disk` (the real
  `Fs`), `http` (the HTTP `Transport`), `account` (sign-in and saved state), `root` (the root marker), `cli`.
- **Real `Fs`** over a root directory:
  - File IDs from the `file-id` crate (inode on Unix, NTFS file ID on Windows), folded to a `u64`.
  - Writes go to `<root>/.craft-cloud/tmp/`, are flushed to disk, get their mtime and exec bit, then land at
    the target only on what the engine checked (since M3b2a: an exchange and a comparison on Linux and macOS,
    steps through a `.cc-aside` name on Windows; see the engine rules). Hashing opens files with every share flag
    on Windows, so it never blocks an app's save, rename or delete.
  - `rename` never replaces: the OS's no-replace rename on Linux and macOS; on Windows a file gets a new hard
    link (refused atomically if the name is taken) and loses its old name; folders, FAT/exFAT and read-only files
    fall back to a check right before the rename, with the race documented.
  - Symlinks and other special files are `Other` and never followed. `.craft-cloud/` is ignored by the
    engine already.
  - The sync folder itself may be a link (it is the user's choice) and is followed; links inside it are
    `Other`. `.craft-cloud/` and its `tmp/` are created owner-only, and a linked `.craft-cloud/` is refused.
  - The same conformance tests run against `MemFs` and the real `Fs`, on Ubuntu and Windows in CI.
- **HTTP `Transport`**: reqwest with rustls, device token as `Authorization: Bearer`. HTTPS is required,
  except to a loopback address. Server answers map onto `TransportError` (refused batch with its
  conflicts, missing chunks, `resync_required`, 401, 5xx and network errors as `Unavailable`). Bodies and
  lists are capped as on the server.
  The client ignores proxy settings (`HTTP_PROXY` and friends), so plain HTTP to a loopback server never
  leaves the machine; HTTPS proxy support can come later.
- **Saved state**, per synced folder, in the user data dir:
  - `account.json`: server URL, user, device ID, device name and the folder. Written owner-only.
  - The device token sits in the same owner-only file. An OS keychain can come later.
  - `state.json`: the engine's `State` with a schema version, written atomically (temp, flush, rename)
    after every pass, failed or not, and at the engine's checkpoints during a pass (M3b2a).
  - One document, not a table per node: the engine works on the whole state in memory anyway. A
    per-node store comes when measurements ask for it (the spec's SQLite state DB is deferred).
  - A crash loses at most the bookkeeping since the pass's last checkpoint and never data. The next passes
    redo it (see "Crash recovery" in the engine rules).
- **Root marker** `.craft-cloud/root.json` (`{space, root}`): written when a folder is first set up. Every
  later run checks it first and stops with a clear message if it's missing or names another space.
- **CLI**:
  - `login <url> --user <name> [--folder <path>] [--device-name <name>]`, with the password read from
    stdin. It creates the device and sets up the folder and its marker. The folder must be empty or new,
    or `--adopt` must be passed to sync an existing one.
  - `sync [--approve-deletes]`: one pass, printed as a short report. Exit codes tell apart done, failed,
    needs approval, and needs you.
  - `status`: account, folder, last pass, pending deletes, skipped files.
  - `logout`: revokes the device on the server and removes the saved account. Local files stay.
- **The data folder and the synced folder never nest**: the data folder holds the device token, so `login`
  refuses a synced folder that contains it or lies inside it (compared as the OS resolves them, links followed),
  and `sync` stops with exit code 3. The path is resolved one name at a time: what exists is made canonical, and
  a `.` or `..` in the part that doesn't exist yet is worked out by name, as the OS will once the folders are made
  (the canonical part holds no links, so that is exact, also for a `..` that climbs above it).
- **Every pass checks the server**: `/api/info` at login and before every sync pass; a server that no longer
  speaks the agent's protocol or chunker fails the pass with exit code 3.
- **One agent at a time**: the agent holds an OS file lock on its data folder and on
  `<folder>/.craft-cloud/lock` while it works; a second one stops with exit code 3. A filesystem without
  locks gets a warning.
- **The root lists only while its marker is there**, so a folder emptied or unmounted mid-pass fails the pass.
- **Permissions**: a downloaded file keeps the permissions of the file it replaces (Unix); only the executable
  bit syncs.
- **Exit codes**: 0 done; 1 failed, try again (offline, server busy); 2 deletions wait for approval; 3 needs
  you: a safety stop (folder or marker missing, another agent running) or an account problem retrying won't
  fix (device revoked, saved files damaged or from a newer agent, server protocol mismatch, damaged data
  from the server, the server or a proxy refusing a request with another 4xx, the data and synced folders
  nested). A mistyped command exits 1.
- **`logout --local`** forgets the account without the server; the device stays valid until an admin revokes
  it.
- **Not in M3b1, planned for M3b2**: blocking disk I/O moved off the async runtime; closing the link race
  with `openat`-style walking (needs an adversary swapping folders for links mid-operation, and Windows has no
  safe equivalent; not in M3b2a). The atomic compare-and-replace when a download lands, with "rename blocked on
  Windows, keep the temp file and retry", came with M3b2a. Documented limits: a volume mounted inside the folder, FAT/exFAT
  file IDs that change on rename (moves become delete + new), Windows read-only files (never replaced; the
  download waits, kept, until the attribute is cleared), and
  `--data-dir` outside the user profile isn't owner-only on Windows.
- **Dependencies**: no `dirs`/`directories` (they pull in an MPL-2.0 helper); the data dir comes from
  environment variables (`%LOCALAPPDATA%`, `~/Library/Application Support`, `$XDG_DATA_HOME`).
- **Conflict copy times** use the machine's local time. If the local offset can't be read safely, UTC is
  used and marked with `Z` (`2026-10-11 14-03Z`), read with `jiff`.
- **Tests**:
  - The real-`Fs` conformance suite.
  - End-to-end: two agents with temp folders against the real server on a loopback port, through the
    CLI's own functions: first sync, edits both ways, a conflict, a move, mass-delete approval, a missing
    root marker stopping the pass, and a server restore (epoch reset).
  - The simulation's crash action.

### Hardening and the agent on its own (M3b2)

M3b2 is split in two. **M3b2a** hardens the engine and the real disk; **M3b2b** makes the agent run by itself.

**M3b2a: hardening**
- **The known gaps first**, each pinned by its simulation seed and a scenario test: a tracked link shadowed by a
  case clash (seed 45982), the crash cases that leave content only in history (seeds 34283, 34673), a `..` in a
  login folder that doesn't exist yet (normalised before the nesting check), adoption that only happens when no
  tracked node owns the found file's local ID, and the simulation's bookkeeping for a folder remade in a failed
  pass (seed 32649).
- **Same content is no change**: a server version whose manifest and exec bit equal the base's is recorded
  without touching the local file (no download, no conflict copy, works for locked files too).
- **Downloads land atomically on what was checked** (the compare-and-replace promised since M3a):
  - Linux: `renameat2(RENAME_EXCHANGE)` swaps the downloaded file with the occupant; the occupant that came
    out is compared with the expected stat; if it changed, it is swapped back and the download waits.
  - macOS: the same with `renamex_np(RENAME_SWAP)`.
  - Windows and filesystems without a swap: the occupant is first renamed to a `.cc-aside` name next to it
    (never replacing anything; on NTFS through a hard link, which can't replace), checked, then the download
    is renamed into place; if the check fails, the occupant is put back. A landing record lets the next run
    finish or undo the steps after a crash: both files stay on disk, never neither.
  - A read-only file (Windows) is never replaced: "read-only here, not replaced", and the download is kept.
  - The engine's `Fs::finish_write` takes the expected occupant (`None`: the path must be empty).
- **Windows "rename blocked"** (an app holds the target open): the downloaded temp file is kept, the node is
  skipped with "in use, retried", and the next pass lands the kept file if its server version is still current,
  without downloading again. A kept file is hashed again before it lands.
- **State checkpoints**: the state is saved at safe points during a pass (after each commit), and the deletes a
  pass sent are remembered with the version they expected, so a crash right after a commit can't make the next
  pass undo or repeat it wrongly.
- **Simulation**: MemFs models the swap, a blocked rename, and a crash between the steps of the Windows path;
  seeds 0–63999 pass with and without crashes.
- Not in M3b2a: closing the race between the link check and an operation with `openat`-style walking. It needs
  an adversary swapping folders for links mid-operation, and Windows has no safe equivalent; documented.

**M3b2b: the agent on its own**
- `craft-cloud-agent run`: a foreground loop (the OS autostart entry comes with packaging). Sync passes run on
  a dedicated thread with its own current-thread runtime, so the blocking disk work never stalls the network.
- **Triggers**: a file watcher (`notify`, CC0) on the folder, the server's long-poll `GET /changes?wait=30`,
  and a full rescan every 10 minutes as a safety net. Events are collected and a pass runs when things settle.
- **Stability**: a changed file syncs only when size and mtime are unchanged across two looks at least 5 s
  apart and, on Windows, it opens with an exclusive share mode for a moment (a sharing violation means a writer
  still has it open). Unstable files are skipped with "still changing" and picked up by the next trigger.
- **Backoff**: transient failures retry with exponential backoff and jitter, capped at 5 minutes; account-level
  and safety stops pause the loop and say why (`status` shows it); the mass-delete guard waits for
  `craft-cloud-agent sync --approve-deletes` (run from another terminal; the loop picks the approval up).
- **Status file**: the loop writes its state (idle, syncing, offline retrying, paused + reason, last pass) next
  to `state.json` for `status` to read. The local control API stays M7.
- **Simulated network**: `turmoil` (MIT) runs agents and the server over a simulated network with partitions,
  delays and dropped connections, on top of the existing in-process simulation.

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
- **Deterministic simulation** of the sync engine: several agents and the server in one process, with an
  in-memory `Fs` per machine and an in-process transport over the real metadata and blob stores that can
  fail calls on a schedule. Random seeds drive edits, moves, renames, deletes, files held locked by another
  program, and failed passes. After each
  run: every machine converges, no file content the engine saw is lost (unless a user overwrote or deleted
  it), and after every finished pass each tracked node still has a place. A failing seed replays exactly.
  Half the seeds turn the racy window off to exercise the size-and-time fast path. Machines also crash at
  random calls and between filesystem steps and restart from their last saved state. Seeds run in parallel
  (`SIM_JOBS`) against an in-memory test server; seeds 0–3999 take under a minute. A simulated network
  (`turmoil`, MIT) comes with M3b2.
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
| M3a | Sync engine on `Fs`/`Transport` + simulation harness | DONE (2026-10-12) |
| M3b1 | Agent, first cut: real `Fs`, HTTP transport, saved state, root marker, CLI; two machines sync a folder by hand | DONE (2026-10-12) |
| M3b2a | Hardening: the known gaps, same content = no change, atomic download landing, Windows rename-blocked retry | IN PROGRESS (prototype verified, plan next) |
| M3b2b | Agent on its own: `run` loop, watcher, long-poll, stability window, backoff, status file, simulated network | TODO |
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
