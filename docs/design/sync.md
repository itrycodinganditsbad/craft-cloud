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
  so `Foo.psd` and `foo.psd` can't both exist (they couldn't on Windows or macOS anyway). On Linux and Windows a
  local name that isn't NFC (a folder copied from a Mac) is another file than its NFC spelling, so it is skipped
  with its own reason ("isn't in the composed Unicode form (NFC) …; renaming it here fixes that"), never taken for
  a link; renaming it for the user is not done.

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
- A file counts as **stable** when size and mtime are unchanged for 2 s (across two checks, or by its mtime at
  the first, where file times can be trusted) and, on Windows, it opens while denying other writers (a sharing
  violation means a writer still has it open). See M3b2b below.
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
  can't be listed), so a later rename of it is a move. This is learnt at the start of every pass, before anything
  can fail, so a pass that fails (offline, a download cut off) still records it.
- **Local moves**: only when the target path did *not* exist before does the agent match by file ID (inode /
  NTFS file ID) plus size and mtime, and send a move of the existing node instead of a delete and a new file.

**Upload**
1. Chunk and hash (streaming; never the whole file in memory).
2. `POST /chunks/missing` with the chunk IDs; upload the missing ones, one at a time. Each chunk is its own
   request, so a cut connection resumes from the next missing chunk. Then `PUT /manifests/{id}`. Downloads fetch
   chunks one at a time too. A few at a time would help on fast links with long round trips, and is left for
   M5's large-media work (decided in M3c): concurrent requests make the simulations' injected failures depend on
   which request finishes first, so a seed would no longer replay exactly unless the harnesses order them, and
   the agent speaks HTTP/1.1 (each concurrent request is a connection of its own).
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
- **Rename blocked on Windows** (an app has the file open): keep the assembled temp file, skip the file with "in use by
  another program; the server's version is downloaded and kept, and lands once it can; tried again next pass", and land the kept file on a later pass if its version is still current, without downloading again.
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
agent's own `.craft-cloud/` folder. Patterns ignore ASCII case (`RENDER.TMP` is ignored too, as Windows and macOS
see one name; only ASCII letters fold, so a pattern never matches more than it says). A `.craftignore` file adds
patterns.

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
- **Files still being written wait**: before a changed file goes up, or is kept as a conflict copy for a server
  version, the engine asks the `Fs` whether it may still be changing (`Fs::still_changing`; the agent's loop
  answers with the stability window, a one-off `sync` trusts every file). Such a file is skipped with "still
  changing": its base stays as it was, so it is never taken for deleted and no server version replaces it; it is
  looked at again later. A server change that would move aside a file that differs from the base and may still
  be changing (in the change's place, as a case twin of it, or where a folder above it must go) waits too. Like
  the link checks, this is decided before the change's first step, so a change that waits never starts halfway
  (seed 44450). Not covered: files inside a folder that a change moves aside (a case twin folder, an untracked
  folder in the way), and a file whose own move the server refuses because the name is taken. On Unix the
  writer's handle follows the renamed file; on Windows the rename fails while a writer has it open, and the node
  is retried next pass. Nothing is lost either way. The simulation keeps random files "still being written" for
  a while, on the machine whose pass is next too, and checks that none of them goes up, is moved or is replaced
  by that pass.
- **A refused delete keeps its folders**: when the server refuses a file's delete (it was edited there), the
  deletes of the folders above it are dropped from the commit, in its batch and the later ones. The node stays
  tracked, so its folders do too (seed 44450).
- **Unreadable files are skipped** with a reason in the report and retried next pass.
- **Racy and replaced files are hashed.** When size and mtime match the base but the file's ID changed
  (replaced), or its mtime is within `racy_window_ms` (default 2 s) of when the base was recorded, the
  content is hashed to decide whether it changed.
- **Case twins**: a local name that differs from an incoming one only by case or normalisation is moved aside
  before a write, mkdir or rename, so it's never overwritten. During a scan, the tracked name wins and the
  newcomer is skipped with a reason. A link skipped that way still hides what is tracked at its name (in a
  folder the user renamed neither name is tracked yet, so a case twin can win the name).
- **A pass costs about the size of the folder** (M3c): the base keeps its indexes (each node's path and the
  nodes at each path, each folder's children, tracked files and folders by local ID) up to date as it changes,
  so a change moves only the paths below the changed node and nothing is rebuilt per node. The twin checks list a
  folder once a pass and keep that listing up to date with the engine's own changes; what an app does to the
  folder meanwhile isn't in it, which is safe because those checks only plan a change: the filesystem refuses one
  that meets something unexpected (a write lands only on what the engine checked, a rename never replaces), and
  the node waits for the next pass. A test pins it: a first sync of 5,000 files in 10 folders lists each folder
  about twice and builds the path index once.
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
  otherwise (its name taken by something other than the download, perhaps a save made after the crash) goes next
  to it under a conflict name, never replacing anything, so the scan syncs it as a copy and never as the tracked
  file renamed to a `.cc-aside` name; nothing removes such files blindly. If the user moved the folder before the
  next pass, the moved file is found there by the folder's ID (also recorded) and handled the same way.
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
  copy even for a file that can't be read here. A move that comes with it applies as usual. Accepted: a lost
  reply to a delete the server refused (another machine had saved the file again unchanged) is taken as "went
  through"; the next pass brings the file back. The delete is undone, nothing is lost.
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
  real again, and history keeps the old version. A tracked folder that became a link, is a real folder again
  and is moved before any pass looks at it as a real folder (a one-off `sync` run only afterwards, or the agent
  not running meanwhile; the loop's debounce narrows it) goes up as a delete plus a new folder: nothing local
  tells it apart from a new one, since it and the files in it all have new local IDs, so its files are uploaded
  as new nodes (the same content; history keeps the old versions). A node behind a link inside it isn't
  deleted while that link stands (below). When a pass, even a failed one, sees the folder in its place first,
  its ID is learnt (below) and the move is a move (seed 112337). The hold below misses a link made or renamed
  under another name in the same window (`Footage` moved away and replaced by a link `Footage (external)`;
  renaming a link counts as removing it), one standing at exactly the path where a link stood at the last
  finished push, and one a finished push already saw while the node was still behind another link (the folder
  copied back as `Clips` while its old link `Footage` stood, a link made in it, a pass, then the old link
  removed: the node is deleted on the server; history keeps it). A server rename of a folder this machine
  deleted, onto the name of a folder of this machine's own, makes that folder the moved one (folders hold no
  content); a file of its own in it under a tracked file's name then goes up as that file's next version (no
  conflict copy; history keeps the old one), unless the file's delete is held (below). It is fixed together
  with the follow-up on a server move onto a file of this machine's own (below). With the stored-bytes cap on, a
  new file bigger than the room left is skipped unread after a refusal earlier in the pass, even if it is a copy of
  something already stored and would fit by dedup; it goes up once the room grows or on a pass that meets no
  refusal before it (edits are not affected: they are skipped only when they could not fit against their own
  base).
- **A lost commit answer and a rename**: a commit whose answer was lost (a cut connection, a crash right after it
  applied) is found again next pass by place and content. If the user meanwhile renamed or moved a file that
  commit created, the next pass downloads the server's file at its old name, and the renamed one goes up as a new
  file once it stops changing: the rename becomes a copy (same content, both names on every machine) until the
  user removes one. Nothing is lost (seed 710 of the network simulation). Edits are not affected: an update whose
  answer was lost leaves the node tracked, so a renamed file is found by local ID. The follow-up is below.
- **Deletes behind a link are held**:
  - *Rule.* A link or special file is *new* when the last finished push saw none at its path. When a tracked
    node is missing (not at its place, not found by local ID, not behind a link, still on the server) and a new
    link's name, ignoring case, is the node's name or the name of a folder on its path where this machine last
    had it, that link may be the node, moved with its folder, and the node's server delete is held. A link that
    was there at the last finished push never holds anything.
  - *What is kept.* The link paths each finished push saw, replaced in the checkpoint after its last commit (a
    pass that fails or crashes leaves them as they were). Where this machine last had a missing node whose
    place the pull moved to follow another machine's move, and every folder the pull moved from its tracked place
    here (the folder found at its place may be a new one that took the name, while the user moved the real one
    away with the link in it; seed 9807 of the network simulation, which also needs no rename above the folder on
    the other side, nor a parent that still exists), until a push finishes. Each hold, by node ID with
    the paths of every new link that matched (an unrelated link with the same name may appear in the same
    window as the real one), saved before the pass commits anything. No folder identity goes into the rule.
  - *While it lasts.* The folders above the node wait with it. Every pass's summary, `status` and
    `status.json` list it with the paths of all its links. It lasts while any of its links stands where the
    hold last saw it (or can't be seen, or a folder can't be listed), or a link with that one's name stands at
    a path the last finished push saw no link at (the link's folder moved; every such path), and while the node
    isn't found here. A file at a held node's place with another local ID and other content isn't the node: it
    doesn't end the hold, and it waits while the hold stands.
  - *How it ends.* Every one of its links goes (removed, replaced by the real file, which goes up as a new file,
    or renamed itself), or the node is found here again (downloaded, put back, found moved by its local ID): the
    next pass judges it afresh by the usual rules (missing, it is deleted on the server, still under the
    mass-delete guard). A hold on a node the server deleted, or that this machine no longer tracks, is dropped.
    Holds an older agent saved load as one link's path and keep holding; its state has no record of known
    links, so every link counts as new once. The user can also approve the delete (below).
  - *Approval.* Every list of held deletes (a pass's summary, `status`) comes with a set of its own, apart from
    the mass-delete guard's (a hash of their nodes, paths and links), and says how to release it.
    `sync --approve-deletes=<set>` naming that set lets exactly those deletes go on the next pass; a running
    loop looks for such an approval while anything is held (every 30 s, or every 2 s while the guard's set
    waits too). The bare
    `--approve-deletes`, an approval of the guard's set, and a hold made after the approval never release a
    hold: it ends by approval only when the user names it. The bare flag says so, with the set to name, rather
    than "nothing was waiting". `run` lists the held deletes when their set changes (and says when the last one
    ends), not after every pass.
  - Holding never loses anything. A wrong hold (a link that appeared in the same window as an unrelated delete,
    sharing a name with its path) keeps one delete waiting, listed with its links, until its links go, the node
    turns up, or the user approves its set.
  - *Examples* (each a scenario test): a folder that was a link is put back from another drive, a link is made
    inside it and the folder is renamed, with no pass between (seed 112337's no-pass order); the same, moved
    onto the place of a tracked folder the user just deleted (seed 804 of fix round 4) or onto a name another
    machine just made or moved a folder to; then a new folder made at the old name (seeds 13116, 19051, 35808)
    or another tracked folder renamed onto it (seed 110570); the same while an unrelated folder can't be listed;
    another machine moving or renaming the node first (seed 804 of fix round 6). Once held, a new folder at the
    old place, another machine moving the node or renaming the link's folder (seed 3660), the engine moving that
    folder aside, or the user putting it back under a new name don't end it, and neither does removing an
    unrelated link with the same name that appeared in the same window, or that the hold followed to when the
    link's folder was renamed. Not held: a permanent `Footage` link, and a later delete of
    `ProjectB/Footage/x.mov`; a same-named link that was already there in another folder.
- **A server move onto a file of this machine's own keeps both**: when another machine moves a node this
  machine is missing (its user deleted or moved it away) onto a name where this machine has a file of its own
  (no tracked node's, by place or by local ID, and not the node's content), that file is moved aside as a
  conflict copy (it goes up as a new file) and the node lands at its name, as if it had still been here. The
  file is never taken for the node's next version. A link on the way, or a file there still being written,
  makes the server move wait (the file is never moved aside while it changes); for a node that isn't held, the
  push still sends this machine's delete, as when the target is free (history keeps the node; the file goes up
  as a new one once it settles). Which should win when a file of this machine's own is at the target, the
  delete or the server move, is a follow-up: one rule for the occupied and the free target, which also decides
  the folder case in Known gaps.
- **Folder IDs are learnt before anything can fail**: at the start of every pass, a tracked folder at its place
  under a local ID it didn't record (a link that became a real folder again, a folder put back from another
  drive) takes that ID, by the same rule as the push. A pass that then fails still leaves it in the state, so a
  later move of the folder is sent as a move, and a node behind a link inside it is never deleted on the server
  (seed 112337: a link that became a real folder, a pass that failed, then a link made inside it and the folder
  moved). The folder is scanned for this only when a quick look at the tracked folders finds a new ID that the
  last such scan didn't decline (a folder that can't be listed, a renamed folder holding a link, an ID another
  tracked folder has): a decline costs one scan, not one on every pass, and the push's own refresh takes the ID
  once it can be taken.

## Server

- One binary, `craft-cloud-server` (axum, tokio, rustls). Linux x86_64 and aarch64 binaries, a Docker image,
  an example systemd unit.
- **TLS**: either give it a certificate (rustls; arrives with packaging in M6), or run plain HTTP behind a
  reverse proxy (M2b). Plain HTTP binds to loopback only unless `behind_proxy = true` is set in the config.
  The proxy should set a request header timeout (the server has none for headers until built-in TLS arrives).
  The server bounds load itself, per user, so a slow or greedy client only ever slows its own account: every
  request body (chunk, manifest, and the JSON of sign-in, missing-chunk queries and commits) must arrive within
  60 s plus 30 s per MiB the route allows (3 minutes for a chunk, 90 s for a JSON body, about 20 minutes for the
  largest manifest; `408 timeout` after that), at most 8 such bodies per user are received at once (more
  wait their turn before their bodies are read), at most 4 manifests per user are decoded and checked at once
  (taken once the body is in; reading a manifest takes none), at most 16 change-feed requests wait per space, and
  missing-chunk lists hold at most 10,000 IDs. Password checks (Argon2id) run at most 4 at a time server-wide; a
  sign-in that finds them all busy is answered `429 rate_limited` with `Retry-After: 1`, and that answer doesn't
  count against its sign-in attempts (the slot is taken before the attempt is counted, and held across the user
  lookup, so a slow database gives busy answers rather than a queue). `trusted_proxies` (a
  list of addresses) decides whose `X-Forwarded-For` is believed; without it the peer address is used. With
  `behind_proxy = true` the server refuses to start without `trusted_proxies` (every client would share the
  proxy's address, so a few wrong passwords would lock the whole studio out of signing in) unless
  `ignore_forwarded_for = true` says that is meant; that setting is refused anywhere else (with `trusted_proxies`,
  whose `X-Forwarded-For` would still be believed, or without `behind_proxy`).
- **Version negotiation**: `GET /api/info` returns the server's protocol version range, chunker parameter
  sets and limits. The agent checks it at login and on every start, and refuses with a clear message if
  there's no overlap.
- **Metadata: SQLite or Postgres**, chosen in the config. Both run the same queries (sqlx `Any` driver) and the
  same test suite. sqlx (MIT/Apache) with **one migration directory per backend**; no query relies on one
  dialect's quirks without a test on both.
  - SQLite (default, solo and small studios): WAL mode, `busy_timeout` 5 s, **one write transaction at a
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
- **Dedup without leaking**: chunks are stored once on disk, but `chunks/missing` answers per space, and the
  stored-bytes cap counts every blob a space has at its stored size, even when another space stored the same bytes
  first (the logical-bytes quota counts per space too). Neither reveals whether someone else has the same data.
- **Uploads and commits check what they reference**: a manifest upload is refused with the list of missing
  chunks unless every chunk it lists is already in the space; a commit refuses a version whose manifest isn't
  in the space (`missing_manifest` conflict). The commit's check runs inside its write transaction (GC treats
  manifests like chunks); the manifest's chunk check runs just before its write transaction (for a manifest of a
  million chunks it is thousands of queries, which would hold every other write back on SQLite), and the
  generation counter below covers the gap once GC exists (an upload touches what it references). A manifest refused for missing chunks makes the agent send exactly the chunks named,
  once, and the manifest once more; a second refusal fails the pass, which is tried again later. GC and commit share a **generation counter**: GC marks with generation G and
  only sweeps chunks unreferenced at G and not touched by any upload or commit since, so a commit can't
  race a sweep.
- **Accounts**: usernames are unique ignoring ASCII case (stored lowercase), so `Ana` and `ana` are one account.
  `admin` and `member` roles. Passwords Argon2id. Login rate-limited per client address and
  per account. Passwords are 8 to 1024
  bytes; admin CLI commands read them from standard input, never from arguments.
- **Devices**: the agent logs in once with username and password and gets a 256-bit device token, stored
  only as a hash. The device list shows name, created and **last used**. Revoke from the admin CLI or from
  your own device list. `last used` is
  written at most once a minute per device, after the request is answered (it never waits for the write lock).
  Sign-in limits and that once-a-minute rule live in the server process: two server processes on one Postgres
  database (not a supported setup) would each count their own, so twice the sign-in attempts and twice the writes.
- **Setup**: `craft-cloud-server init` writes the config, creates the data directory and the first admin.
  Solo use is that one admin account and nothing more.
- **Join codes**: a machine can also get its device token with a code instead of a password. Codes are Crockford
  base32 (`0-9` and letters without I, L, O, U; on input case is ignored, O reads as 0, I and L as 1, dashes and spaces
  are dropped), shown in groups of four (`K7M4-Q9TX`), and stored only as their BLAKE3 hash (migration 0004:
  `invites (user_id, code_hash, expires_ms)` and `pairings (id, user_id, device_id, code_hash, expires_ms, used_by)`;
  expired rows are deleted on the next write to their table). What a person types is the server plus the code,
  `cloud.example.com/K7M4-Q9TX` (`https://` assumed), or the link `https://cloud.example.com/join/K7M4-Q9TX`.
  - An **add-a-computer code** (pairing) comes from a signed-in device: 8 characters (40 bits), 10 minutes, one use,
    one open per device (a new one replaces it). It dies with its device: revoking or signing out that device
    deletes it. The device polls it: waiting, used (with the new device's name) or expired.
  - An **invite** comes from the admin (`user invite <name>`): 16 characters (80 bits), 7 days, one use, one per
    account (a new one replaces it; `user invite --revoke` ends it). If the account doesn't exist it is created
    with no password (`password_hash = ''`, which no password matches); redeeming the invite then needs a new
    password, set in the transaction that creates the device. For an account with a password, nothing is asked.
  - **Redeeming** (`POST /join`) is one transaction: a conditional `DELETE … RETURNING` of the invite or
    `UPDATE … WHERE used_by IS NULL … RETURNING` of the pairing, then the new device, so of two redeems of one code
    at the same moment one gets the device and the other `invalid_code`, on SQLite and Postgres. A wrong, expired,
    used or malformed code, or a pairing whose device has signed out, is the same `404 invalid_code`, never saying
    which. Failed redeems are limited to 10 a minute per client address (behind trusted proxies as sign-ins are),
    then `429 rate_limited`, even for a code that works; a success, `password_required` and `weak_password` (only
    answered for a real, unused invite) don't count. With that limit, 40 bits and 10 minutes, a guess succeeds with
    odds below one in 10⁹.
  - `GET /join/{code}` (outside `/api`) is the plain page a link opens in a browser: the same text whatever the
    code, which it never looks up.
- **Stored-bytes cap** (M3c): `stored_bytes_per_user` in the config, off by default and refused at load when
  under one chunk (4 MiB), caps what one account
  stores: the stored size of every chunk and manifest it uploaded, each counted once (an account is one space,
  so per user and per space are the same until sharing exists). Uploads are unaccounted
  otherwise and nothing frees space before GC, so on a server shared with people the admin doesn't fully trust,
  this is what keeps one account from filling the disk for everyone. An upload that would go over it is refused
  before its bytes are stored (`507 quota_exceeded`, with `quota: {used, limit}`); the agent skips that file and
  lists it ("too big for the account's storage on the server"), and everything else keeps syncing; after such a
  refusal, a later file in the same pass is skipped the same way before it is read when it could not fit even
  if all of its previous version's content were reused (size over the room plus the previous size; a new file has
  none). So a file bigger than the room is read at most once per pass, while files within a chunk's bookkeeping of
  the room may still be read and refused. Nearly full counts as full: an account with less than one chunk (4 MiB)
  of room left pauses the agent (account-level, looked at again every ten minutes), so a full account isn't read
  through on every pass. The
  chunks a refused file stored before the refusal stay stored and counted until GC (M4) removes them. Off by default: a
  self-hosted server's accounts are usually the admin's own people, and a cap that refused a big library on its
  first upload would surprise more than it protects.
- **Quotas** (M4): optional per user, in logical bytes. A commit over quota is refused whole.
- **Retention by thinning** (M4): keep every version from the last 24 h, then one per hour for 7 days, then one
  per day for 30 days; the current version is always kept. Deleted nodes sit in a trash under the same
  rule. All three windows are config.
- **Garbage collection** (M4): daily, mark and sweep with the generation rule above. It also frees what a file
  refused by the stored-bytes cap stored before the refusal, which until then counts against the account.
- **Backup** (M4): `craft-cloud-server backup <dir>` pauses GC, snapshots the metadata (SQLite online backup, or
  `pg_dump` in one snapshot transaction for Postgres), then copies chunks (immutable, so anything copied
  after the snapshot is a superset), then resumes GC. With S3, `--metadata-only` skips the chunk copy for
  admins who back up the bucket themselves (bucket versioning or replication); GC must stay paused until
  that bucket backup is done, or the admin keeps the bucket's own deleted-object retention longer than the
  GC grace. **Restore** sets a new epoch for every space.
- **Admin CLI**: `init`, `serve`, `user add|reset-password|invite [--revoke]`, `device list|revoke` and `reset-epoch`
  exist; `user remove|quota`, `gc`, `verify` (re-hash every chunk), `backup <dir>` and `restore <dir>` come with M4.
  `user invite` prints the join string and link when `public_url` is in the config, else the code and how to finish
  it; it logs that an invite was made (the username at debug only, never the code).
- No web UI in v1. No telemetry, no update check.

API (`/api/v1`, JSON except chunk and manifest bodies; every signed-in response carries the space's epoch in
the `craft-epoch` header, and commits/tree/changes carry it in the body too). Errors are
`{error, message, conflicts?, missing?, quota?}` with stable codes (`conflict` 409, `missing_chunks` 409,
`resync_required` 410, `unauthorized` 401, `rate_limited` 429, `invalid` 400, `not_found` 404, `too_large` 413,
`timeout` 408, `quota_exceeded` 507,
`username_taken` 409, `invalid_code` 404, `password_required` 400, `weak_password` 400, `internal` 500; the codes are constants in `craft-cloud-proto::api::codes`). Unknown routes and
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
| `POST /pairings` | a new add-a-computer code from this device → `{id, code, expires_ms}` (the code shown once, normalised) |
| `GET /pairings/{id}`, `DELETE /pairings/{id}` | one of your pairings: `{state: waiting\|used\|expired, device_name?}` / cancel it (204) |
| `POST /join` | code + device name (+ password for an invite to a new account) → device ID + token, as a sign-in (no auth); `invalid_code`, `password_required`, `weak_password`, `rate_limited` |

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
  - `sync [--approve-deletes[=<set>]]`: one pass, printed as a short report. Exit codes tell apart done, failed,
    needs approval, and needs you.
  - `status`: account, folder, last pass, pending deletes, skipped files (since M3b2b1 also whether `run` runs and
    what it does).
  - `run` (M3b2b1): the agent on its own; see M3b2b.
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

M3b2 is split in two. **M3b2a** hardens the engine and the real disk; **M3b2b** makes the agent run by itself. M3b2b is split again: **M3b2b1** is everything below except the simulated network, which is **M3b2b2**.

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
    finish or undo the steps after a crash: both files stay on disk, never neither. If the target's name was
    taken meanwhile, the moved file comes back under a conflict name next to it.
  - A read-only file (Windows) is never replaced: "read-only here, not replaced", and the download is kept.
    It is never removed by a server delete either ("read-only here, not removed"); the delete waits until
    the attribute is cleared.
  - The file ID is compared before the first step; after the move it counts only when the move kept it (a
    hard link), since FAT/exFAT give a renamed file a new ID.
  - The exchange runs from a `.swap` temp file with its own record, which cleanup never deletes: after a
    crash, recovery removes it if it is the download or the checked file. If it is a save the exchange took
    out while the download is still at the target, the two are exchanged back (the save keeps the user's
    name, the download goes and is fetched again); anything else moves next to the target as `.cc-aside`. If an app saved twice during the swap back, the newest save stays at the name.
  - While a landing record is pending, its target counts as unknown to the scan (never deleted on the server)
    and its `.cc-aside` file isn't listed; a record recovery can't act on yet fails the pass
    (tried again next pass); a record it can't read is set aside as `<record>.bad` (a swap record's file as
    `.rescued`), and every pass's report and `status` say "check the files next to <target>, then remove
    <record>.bad" until the user does.
  - The engine's `Fs::finish_write` takes the expected occupant (`None`: the path must be empty).
- **Windows "rename blocked"** (an app holds the target open): the downloaded temp file is kept, the node is
  skipped with "in use by another program; the server's version is downloaded and kept, and lands once it can;
  tried again next pass" (or "read-only here, not replaced; …"), and a later pass lands the kept file if its
  server version is still current, without downloading again. While the file stays blocked the kept file isn't
  touched; it is hashed again before it lands.
- **Cleanup** also removes the temporary names an interrupted atomic write of the agent's own files leaves
  (`.<name>.<random>.tmp`: `root.json` and records in `.craft-cloud/`, `account.json` and `state.json` in the
  data folder; nothing else). A `.rescued` file is listed by `status` until it is gone.
- **State checkpoints**: the state is saved at safe points during a pass (after each commit), and the deletes a
  pass sent are remembered with the version they expected, so a crash right after a commit can't make the next
  pass undo or repeat it wrongly.
- **Simulation**: MemFs models the swap, a blocked rename, and a crash between the steps of the Windows path;
  seeds 0–63999 pass with and without crashes.
- Not in M3b2a: closing the race between the link check and an operation with `openat`-style walking. It needs
  an adversary swapping folders for links mid-operation, and Windows has no safe equivalent; documented.

**M3b2b: the agent on its own**
- **Seed 112337 first**: folder IDs are learnt at the start of every pass, before anything can fail (see the engine
  rules).
- `craft-cloud-agent run`: a foreground loop (the OS autostart entry comes with packaging). It holds the data
  folder's lock for its whole life and the folder's lock while the folder is there. Sync passes run on a
  dedicated thread with its own current-thread runtime, so the blocking disk work never stalls the network; the
  loop's control, the watcher's events and the long-poll run on the main runtime. Ctrl-C (and SIGTERM on Unix):
  no new pass starts, a running one finishes, the status is written, exit 0; a second Ctrl-C ends at once with
  exit 1: the status is written and the process ends without waiting for the running pass or a pending request
  (the pass is cut short; its checkpoints make that safe, as after a crash).
- **Triggers**: a file watcher (`notify`, CC0) on the folder, the server's long-poll `GET /changes?wait=30`,
  and a full pass every 10 minutes as a safety net. Every pass scans the whole folder; the triggers only decide
  when. Events under `.craft-cloud/` and reads are ignored. A watcher error, dropped events, or a watcher that
  stops by itself run a pass at once, and the watcher is made afresh (at most once a minute). Every start of a
  watcher but the loop's first (after it failed or stopped, when a pause ends) is followed by a full pass after
  the usual 1.5 s debounce, since events between the last pass and the start were lost (at once after a time
  unwatched); the Windows re-creation below comes right before a pass, which is its pass. A watcher that can't start (on Linux, more folders than
  `fs.inotify.max_user_watches` allows) is said once, in the output and in `status` ("not watching the
  folder"); a full pass then runs every minute, and the watcher is tried again after a minute, doubling up to 10
  minutes. Known gap: on Windows, `notify` 8 neither reports nor survives an overflow of
  `ReadDirectoryChangesW`'s buffer (a bulk copy of thousands of files): the events are dropped silently, or the
  folder stops being watched. Once a watcher has run for 10 minutes, the loop makes it afresh just before the
  next pass (when idle, the safety-net pass), and that pass, which also finds what was dropped, stands in for the
  restart's own: an idle loop still scans once per 10 minutes. Such changes can wait up to 10 minutes. `notify` 9 reports the overflow (a
  rescan); moving to it once it is stable is the follow-up.
  Folder events are collected and a pass runs once the folder has been quiet for 1.5 s, but no later than 10 s
  after the first event, so a folder that never goes quiet still syncs. A change on the server runs a pass at
  once. The long-poll asks for one node only (it only needs to know whether to run a pass), polls on from the
  server's new position, and backs off on errors (2 s doubling to 5 minutes, with jitter) and on `429` (the
  server's `Retry-After`, at most 5 minutes, plus up to half again at random), and two polls start at least 1 s
  apart whatever the server answers: never a hot loop. The agent's own writes wake it once more at most;
  that pass finds nothing to do, and an idle loop settles.
- **Stability**: a changed file syncs only when its size and mtime have been unchanged for 2 s: across two looks
  at least 2 s apart, or already at the first look when its mtime is at least 2 s old (a file written a while
  ago syncs on the first pass that sees it). A time in the future by the local clock never counts as old, and the
  shortcut is off where file times can't be trusted, which is told once per session from the folder's
  filesystem: on Linux and macOS a network share (NFS, SMB, AFP, WebDAV and the like, whose times come from the
  server's clock and may lag ours), FAT and exFAT (coarse times), and FUSE and 9P mounts (on macOS by type name,
  so a macFUSE mount made with `-o local` counts too); there a file always needs two looks. Elsewhere, and on Windows (where the probe below stands in), a writer that paused for longer
  than the window looks settled too; the re-stat after hashing still keeps a file that changed while it was read
  from going up. On Windows it must also open while denying other writers (a sharing violation means a writer
  still has it open; readers don't stand in the way). The probe opens the file itself, never a link put in its
  place (`FILE_FLAG_OPEN_REPARSE_POINT`), and for that moment an app opening the file for writing gets a
  sharing violation. The engine asks the `Fs` (`still_changing`) only about files that differ from the base,
  before an upload, before keeping one as a conflict copy and before a server change moves one aside (see the
  engine rule "Files still being written wait" for what that covers); an unstable file is skipped with "still
  changing" and keeps its base (never taken for deleted, never overwritten). The loop runs a pass again when the
  earliest look is due, not only on the next event: 2 s after the first look, then doubling for a file found
  changed again (or still open for writing), up to 5 minutes, and starting over once it settles. The watcher's
  events for a file that is waiting don't run passes, so a file that never settles (a log, a database held
  open) costs a pass every few minutes, not one every few seconds. Instead each event sets a cheap look (one
  stat, no pass) for when the file has been quiet for a quarter of its current delay (at most 30 s) since its
  last event; on Linux a writer closing the file after writing sets it for at once. A file written all the time
  keeps pushing that look back, so it costs nothing. A look that finds the file unchanged for 2 s (by the same
  rule as above), or gone, runs a pass, which syncs it; one that finds it changed looks again 2 s later. So a long
  export that ends syncs within about 30 s of its last write (a few seconds on Linux), not only when its
  backed-off look is due (up to 5 minutes). Only files still waiting are remembered, however many changed at
  once; a finished pass forgets those it didn't ask about. A one-off `sync` trusts every file at once (the
  re-stat after hashing still guards it). Folder events still run a pass after a 1.5 s debounce.
- **Backoff**: a pass that failed for a reason that may pass by itself (exit code 1: offline, a server error)
  is retried after 2 s doubling up to 5 minutes, the second half of each delay random; a pass that finishes
  resets it. The long-poll getting an answer again (the server is back) runs the pending retry at once (if it
  fails again, the next delay goes on growing); a change on the server, a watcher error or an approval waits for
  the retry like everything else. Account-level problems and safety stops (exit code 3) pause the loop and say
  why (`status` shows it). A folder or marker that is missing (a drive not mounted yet), data and synced folders
  inside one another, or another agent on the folder are looked at again every minute, and the loop goes on by
  itself once they are put right; a server that speaks another protocol or refuses requests every 10 minutes
  (an update fixes it); a revoked device, damaged or newer saved files, another account's state and damaged
  data from the server never fix themselves, so the loop stays paused until it is stopped.
- **Approval while the loop runs**: the mass-delete guard holds the deletions back and the rest goes on syncing.
  The waiting deletions form a set with a short identifier (a hash of its nodes and paths), printed with the
  list (in `run`'s output too). `status` shows the list and its set, and notes the set as shown (`shown.json` in
  the data folder). `craft-cloud-agent sync --approve-deletes` from another terminal finds the loop holding the
  locks and approves the set the loop's last pass listed only if it is the set `status` showed: it writes
  `approval.json`, naming the set and its deletions. If the set changed since (a pass found more deletions) or
  was never shown, nothing is approved: the command lists the new set, notes it as shown, and exits with code
  2, so the user checks it and runs the command again. `sync --approve-deletes=<set>` names the set instead (as
  the loop's output or `status` printed it; every list says "check the list, then run `craft-cloud-agent sync
  --approve-deletes=<set>`"): it approves that set if it is the one waiting, whatever `status` showed, and
  otherwise lists the waiting set, notes it as shown, approves nothing and exits with code 2. A one-off `sync`
  with no loop does the same against its last pass's list (a wrong set runs no pass). An empty set
  (`--approve-deletes=`, a script's unset variable) is a usage error, never the bare flag. The loop looks for an
  approval every 2 s while deletions wait (every 30 s while only deletes held behind a link wait: a hold pauses
  nothing and may stand for days) and takes it (renaming it away first, so a newer approval lands as a
  new file). It releases the deletions only if the approval names the set that waits now, and then exactly
  those: any deletion that came since still waits and is listed again. An approval naming another set, or one
  found when nothing waits (after any finished pass), is discarded with a message, never kept for later.
  Approvals cover what was shown. Deletes held behind a link form a set of their own, released only by an
  approval naming it (see the hold). A plain `sync` while the loop runs stops with exit code 3 ("another
  craft-cloud-agent is running").
- **Status file**: `status.json` next to `state.json`, written atomically when what it says changes and every
  15 s as a heartbeat. Its times don't count as a change (they are converted afresh at every wake-up and jitter
  by a millisecond; a new next try or next check only comes with a pass, and "syncing" is written in between),
  so a loop that is retrying while the watcher wakes it hundreds of times a second still writes it every 15 s.
  It holds the state (idle, syncing, offline retrying with the next try, paused with the reason and the next
  check, stopping, stopped), whether deletions wait for approval (the mass-delete guard), apart from it whether
  deletes are held behind a link (`held`: a hold pauses nothing and never counts as waiting for approval), the
  pid, the start, the last pass and its summary (with the deletes held behind a link, each with the paths of all
  its links, kept after a failed pass). Every
  time in it (the heartbeat, the next try, the next check) is the wall clock's at the moment it is
  written; the loop schedules on a monotonic clock, which stands still while the machine sleeps, so only
  durations from now are carried over. `status` reads it: the loop runs if it hasn't written "stopped" and its
  heartbeat is at most three beats (and 5 s) old (it doesn't take the lock to find out: that could make a loop
  starting at that moment stop). It works the same with no loop. Cleanup removes the interrupted atomic writes
  of `status.json`, `approval.json` and `shown.json` like the others', but only those at least a minute old
  (`status` and `sync --approve-deletes` write without the lock while a loop holds it), and the loop cleans
  once, at its start, never while it may be writing its own. If the watcher can't start, the status says so
  (see Triggers). The local control API stays M7.
- **Simulated network** (M3b2b2): `turmoil` (MIT, a dev-dependency: the shipped binaries don't link it) runs
  three agents and the real server (its router; SQLite and blobs in memory) on hosts of their own, over a
  simulated network, on top of the in-process simulation's actions and checks (`apps/craft-cloud-agent/tests/
  network/`). The agents' requests go through the agent's own HTTP code: `HttpTransport` builds the requests
  and reads the answers (error mapping, body caps, `429`/`Retry-After`, the long-poll; `info` and the sign-in
  too) over a `Connection` that only carries them, reqwest in the agent and a hyper client on turmoil's TCP in
  the simulation; the server side is the same router served by hyper, with the client's address as `serve`
  gives it. Passes of different machines run at the same time, while links are cut (both ways or one way, so
  requests or answers are lost), held and released, messages take up to 400 ms, the server loses answers after
  applying the request (commits, uploads) or cuts them halfway (feed and tree pages, chunks), and pages are
  small. A commit whose answer was lost relaxes no check (except the being-written check's "downloaded"
  exemption, see Check failed passes in the follow-ups). A seed replays exactly to the simulated millisecond,
  on any thread: SQLite answers from a thread of its own, so the server's database work takes no simulated
  time, and the server's clock (what requests record, when a device's last use is written) is the simulated
  one. Scenario tests pin each fault alone: a commit whose answer was lost is found on the server next pass
  (client IDs: no second node, no conflict copy, nothing sent again), an upload of several chunks whose first
  chunk's answer was lost sends each other chunk once and that one not again, a cut feed page fails the pass as
  offline, a long-poll cut off by a partition ends at its timeout (the wait plus the cap for a small answer:
  120 s), and a server whose long-poll slots are all taken answers `429` with `Retry-After`, which the loop's
  long-poll waits out. The `run` loop's scheduler and long-poll also run on simulated time (passes inline, the
  user's writes handed in for the watcher): a server change reaches it within seconds, a ten-minute partition
  backs passes and polls off, the heal runs the pending retry on the long-poll's first answer, and an idle loop
  polls every 30 s. That test copies the loop's event handling rather than running `run_loop`, so it can drift
  from it. Not simulated: reqwest's own connection handling (its pool, TLS, the connect and read timeouts; the
  simulation keeps the whole-request timeout and opens a connection per request), the server's `serve`
  (graceful shutdown), the pass thread, the watcher and the status file, server restarts, and machine crashes
  (the in-process simulation crashes machines). turmoil refuses a new connection across a partition at once;
  only connections already open stall until their timeout.

## Errors

- **Transient** (network down, timeouts, `5xx`, server restarting): retry with exponential backoff and
  jitter, capped at 5 minutes. Status shows "Offline, retrying"; local work carries on.
- **Per-file permanent** (name rejected, case collision, over the protocol's limits of 256 GiB or 2^20 chunks,
  too big for quota, permission denied, file locked): that file is skipped and listed with the reason, every pass
  until it changes; everything else keeps syncing. A file over the size limit is skipped before it is read. A
  file whose conflict copy finds every one of its 1,000 names for that minute taken is skipped the same way and
  tried again next pass. Only damaged data from the server (a chunk or manifest that fails its hash, a state
  that doesn't hold together) and a server that answers outside the protocol stop the pass and ask for a person
  (`sync` exits 3).
- **Account-level** (token revoked, server gone, stored-bytes cap full or less than one chunk of it left,
  protocol mismatch): pause syncing, say so clearly, never delete local files because of it.
- **Safety stops**: a missing root marker pauses and asks. The mass-delete guard and a delete held behind a link
  hold back only those deletes and ask; everything else keeps syncing. The mass-delete guard is only ever
  resolved by the user's approval; a hold ends when its links go, the node is found here again, or the user
  approves its set. A missing folder or marker (a drive not mounted yet) pauses the `run` loop,
  which looks again every minute and resumes by itself once both are back, as they were: the marker must still
  name this account's space and root, so a different folder mounted in its place is never synced. While paused,
  the loop releases the folder's lock (the data folder's lock stays held): the folder may be gone, and a lock
  file can't be held in it. That is safe because every pass first takes the folder's lock again, checks the
  marker and the folders, and another agent that took the lock meanwhile makes the loop pause again ("another
  agent on the folder"). A one-off `sync` stops with exit code 3 and never resolves anything by itself.
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
  (`SIM_JOBS`) against an in-memory test server; seeds 0–3999 take under a minute. The same actions and
  checks also run over a simulated network (`turmoil`, MIT; `--test network` in the agent, the same `SIM_*`
  knobs): the real server and agents' HTTP code, with partitions, held links, latency, lost and cut answers and
  machines syncing at the same time (see M3b2b2). Seeds 0–999 take about 35 s in `--release`; a seed replays
  exactly, on any thread, which a test checks on 20 seeds.
- **Scripted scenarios** on top, as readable regression tests: concurrent edit → conflict copy; offline edits
  on both sides; upload cut halfway then resumed; delete versus edit; folder move during an upload; epoch
  change with a newer local file and with another device already reconciled; mass delete paused; Windows
  rename blocked; atomic save (temp → rename over) keeps one node and its history; offline mkdir + files in
  one batch with client IDs; two devices create the same name offline; move cycle; edit inside a remotely
  deleted folder restores the parent chain; a batch over the cap; an app saving while the agent hashes
  (Windows) never fails.
- **HTTPS**: an agent test serves a self-signed test certificate over TLS (`tokio-rustls`, a dev-dependency):
  the agent's own connection refuses it, and reaches the same listener when told to trust only that
  certificate (`Reqwest::trusting_only`, a seam the agent never uses itself).
- **Licences**: `cargo xtask licenses` (in `cargo xtask ci` and CI) reads `cargo tree` for every target and
  every kind of dependency and refuses a crate whose licence expression doesn't allow it under the permissive
  licences `AGENTS.md` lists (CDLA-Permissive-2.0 only for the root-certificate crates). No tool beyond cargo.
- CI on Ubuntu and Windows (repo `ci.yml`); macOS in release builds, as in the other Craft repos.

## Milestones

| # | Milestone | Status |
|---|---|---|
| M0 | Repo, conventions, CI | DONE (2026-10-11) |
| M1 | `proto` + `chunker` (names, IDs, `fastcdc-v1` with golden vectors) | DONE (2026-10-11, PR #1) |
| M2 | Server core: info, auth, devices, chunks, batch commit, changes, tree, epoch; SQLite + Postgres, disk + S3 | DONE (2026-10-11; M2a storage, M2b server) |
| M3a | Sync engine on `Fs`/`Transport` + simulation harness | DONE (2026-10-12) |
| M3b1 | Agent, first cut: real `Fs`, HTTP transport, saved state, root marker, CLI; two machines sync a folder by hand | DONE (2026-10-12) |
| M3b2a | Hardening: the known gaps, same content = no change, atomic download landing, Windows rename-blocked retry | DONE (2026-10-13) |
| M3b2b1 | Agent on its own: seed 112337, `run` loop, watcher, long-poll, stability window, backoff, status file | DONE (2026-10-14) |
| M3b2b2 | Simulated network (`turmoil`): partitions, delays, dropped connections | DONE (2026-10-15) |
| M3c | Fixes from the full repo review | DONE (2026-10-15) |
| M4 | History, restore, trash, thinning, GC (including the chunks of uploads the stored-bytes cap refused), logical quotas, admin CLI (`user remove\|quota`, `gc`, `verify`), backup/restore | TODO |
| M5 | Selective sync, large-media polish (a few chunks in flight at once), ignore rules | TODO |
| M6 | Packaging: binaries, Docker, systemd, built-in TLS, self-hosting guide | TODO |
| M7 | Local control API + launcher integration PR | TODO |

### M3b2b follow-ups

From the final reviews of the M3b2b1 and M3b2b2 prototypes, less what their fix rounds did:

- **Delete vs. server move onto a file of this machine's own**: one rule for the occupied and the free target
  (the review recommends "the delete wins"). It then decides the folder case in Known gaps
  (`known_gap_a_server_folder_rename_onto_an_own_folder_…`).
- **Hidden-node guard** (Known gaps' third miss, `known_gap_a_link_seen_while_the_node_was_behind_another_link_…`):
  don't make links known while a tracked node is hidden behind a link. Weigh it against longer wrong holds.
- **Ancestor-name false holds**: count an ancestor-name match only while that folder isn't at its place, or
  accept them now that an approval releases a hold. Since seed 9807 a folder the pull moved keeps its old path
  until a push finishes, which adds a false hold of its own: another machine renamed `Old` to `New`, and this
  machine deleted `Old/f.txt` and made an unrelated new link `Links/Old` before its pull; the delete is held
  behind `Links/Old` until the link goes or the user approves (safe side). The candidate narrower rule: keep the
  old path only for folders the pull moved whose ID here was taken afresh by path (a new folder that took the
  name); `Old` kept its ID. It needs that set kept across failed pushes.
- **Lost commit answers by ID** (from M3b2b2, Known gaps): save the commit's new nodes with the local IDs of their
  files in the checkpoint before sending it, and on the next pass match a server node the base lacks to its file
  by local ID, so a rename after a lost answer stays a rename.
- **Check failed passes** (from M3b2b2): what a failed pass got onto the server (lost answer, crash) is checked
  against the files being written when it started: in process, the in-memory server's versions after the pass; over
  the network, the harness's per-host commit log. Then the simulation's "downloaded" exemption in the
  being-written check goes.
- **Approval pace for holds**: the loop looks every 30 s while only holds wait, so a release can take that
  long. Looking every 2 s for a minute after `status` or a pass showed a held set would make it prompt.
- **Errors**: the section hasn't had its own review yet.
- **Carried**:
  - folder-ID reuse (now only relevant to moves);
  - filesystems without folder IDs;
  - macOS and Windows CI, including the Windows watcher renewal driven past 10 minutes and the reparse-point
    probe;
  - `notify` 9 for the Windows overflow;
  - event-scoped scans for huge folders;
  - the simulation's one-shot list failure ("vanished during the scan");
  - recording the commit's `seq` to save the long-poll's extra wake;
  - from M3b2b2: a server restart in the network simulation (it needs the stores on disk: dropping the
    in-memory database's only connection loses it), crashes over the network, the `run` loop's own code (its
    event loop split from the pass thread, the watcher and the status file, so the simulation drives it rather
    than a copy), and a shared dev-only simulation crate instead of the agent's tests including sync's harness
    by path.
- **Simplification on main**: no state older than M3b2a exists there, yet the replay brought in the
  prototype's older hold records (fix rounds 3–6) and the one-link form of a saved summary's held deletes:
  they can be removed. Upgrading from M3b2a only needs "no `known_links`: every link new once" and "no
  `held_nodes` in a saved summary: no held set". In the same change, drop the code comments that cite the
  prototype's review and fix rounds.
- **Network simulation tidying** (from M3b2b2's final review): `hold_time` spins (`std::hint::spin_loop` or
  `yield_now`) instead of a 20 µs sleep if Windows CI turns out slow (a sleep there lasts at least about a
  millisecond); the harness's server clock fails loudly outside a simulation instead of reading the start time;
  AGENTS.md names the `test-support` seams (`Meta::busy`, `AppState::with_clock`) next to the turmoil note.

### M3c follow-ups

From the reviews of M3c, less what their fix rounds did:

- **Room in `GET /space`**: the space answer carries `quota: {used, limit}`, so the agent knows the room at the
  start of every pass and never reads a file that can't fit (today it learns the room from the first refusal of
  a pass). Additive to the protocol. It bounds reads of new files, not of edits or copies that fit by dedup;
  only chunking tells those.
- **Capped simulation mode** (before M4, when GC meets the cap): a seed-chosen stored-bytes cap (every 8th seed,
  a few files' worth) on a cap the harness can lift (`Meta`'s cap is per handle and its clones, so it needs a
  shared cell or a `set_stored_bytes_cap` the transports see). During the random phase `Err(QuotaExceeded)` is a
  legitimate end of a pass (today `settle_and_check` panics on any error) and quota skips are not "stuck"; after
  each pass `stored_bytes(space) <= cap`; then the cap is lifted and the existing oracle runs unchanged
  (convergence, nothing lost, nothing live deleted, nothing stuck). Lifting the cap keeps the oracle exact
  instead of teaching it which files were allowed to stay behind.
- **Round-robin over refused files**, if the copy-of-a-synced-file gap (Known gaps) turns out to matter: files
  refused since the room last grew are processed last, one read per pass, so every file gets a turn.
- **Certificate errors ask for a person**: a refused server certificate is `Unavailable` today (retried
  forever, "Offline"); with the built-in TLS work (M6) it becomes needs-you with the reason.
- **A case-insensitive `MemFs`**: a mode that folds names like Windows and macOS, so the case-twin checks and the
  listing cache run here, not only on Windows CI. The twin-listing cache is keyed by the engine's spelling of a
  folder; key or test it by folded path there.
- **Parallel commits of two users to different spaces** (the original review's I2): Postgres runs them at once,
  SQLite one at a time; a test of both, CI-only.
- **Tests that pin rather than discriminate**: `a_write_waits_for_a_lock_another_program_holds` passes without the
  explicit `busy_timeout` too (sqlx's default is the same); a test that sets a short timeout and sees it fail would
  tell the two apart. (The password-check test now uses one slot and shows it is given back.)
- **Parallel chunk transfers** (M5): a few chunks in flight at once behind a knob that defaults to one, keeping
  both simulations deterministic.
- **Limits in the config**: the body deadlines and per-user slots are constants in `Limits`; the config file
  gets them with M6, with a server-side header timeout once built-in TLS lands, and a deadline around the
  blob-store write while a body slot is held (today a stalled store holds that user's slots for as long as the
  store's own timeout; other users are not affected).
- **Per-pass O(N) work**: `find_local`'s whole-tree walk and `scan()`'s sort comparator (`sort_by_cached_key`),
  after a measurement on a real 50,000-file tree; `Base::update` clones the entry and relinks the indexes on every
  change.
- **`add_chunks` charges once per batch**, after the last part (today once per 500-chunk part, inside one
  transaction: on Postgres two multi-part batches of one space could deadlock; no caller sends one).
- `init` prints the `stored_bytes_per_user` hint for shared servers.

## Out of scope for Phase 1

Hosted service; collaboration and sharing; locking; web UI; mobile; virtual files and on-demand download;
end-to-end encryption (not built, but chunk IDs are opaque and stored chunks have a format byte so it can
come later); per-chunk compression (same format byte); a packed small-chunk upload endpoint (one request per
chunk until measurements say otherwise); compatibility with any other sync service.

## Open questions

- Would the `storytold` maintainers take a launcher integration, and the launcher installing the agent?
