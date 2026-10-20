# Craft Cloud — Live documents: protocols

> The protocol section of the live documents design, kept word for word in step with it (as `sync.md` is with the
> original design). The wire types are in `craft-cloud-proto` (`live` for the server endpoint, `local` for the local
> API).

## Protocols

**Local API** (agent ↔ apps): the loopback design from the original design ("Agent and launcher API": `127.0.0.1`,
random port, per-start token in an owner-only file `local-api.json {schema, port, token, pid}` in the agent's data
dir, exact `Host: 127.0.0.1:<port>`, any `Origin` refused, token only in `Authorization: Bearer`), plus one WebSocket
per app connection for live sessions. `run` serves it while signed out too. Calls:
- `POST /v1/login {server, user, password, folder?, device_name?, adopt?}` / `POST /v1/logout {local}` (a sign-in
  with a password, as `craft-cloud-agent login` makes; the password goes to the agent, never to an app or a file;
  the Craft Cloud app joins with a code instead, and signs out with `logout`);
- `POST /v1/join {join, folder?, device_name?, adopt, password?}` → status: parses the join string
  (`cloud.example.com/K7M4-Q9TX` or the link `https://cloud.example.com/join/K7M4-Q9TX`), checks what it can before
  the code is spent (the folder, the device name, the server's `/info`), redeems it and signs in as `login` does.
  Refused while signed in. A setup that fails after the code is spent (the folder is set up for another space)
  revokes the new device and says to make a new code. `password` only for an invite to a new account, which is
  answered `password_required` without it. A folder with files in it, without `adopt`, is answered
  `folder_has_files` before anything reaches the server (as `login` is), so the app can ask and send it again;
- `POST /v1/pairings` → `{id, join, link, expires_ms}` (a new add-a-computer code, replacing this device's open
  one), `GET /v1/pairings/{id}` → `{state: waiting|used|expired, device_name?}`, `DELETE /v1/pairings/{id}` → 204;
- `GET /v1/devices` → the account's devices (`current` marks this one); `DELETE /v1/devices/{id}` revokes another one
  and answers the list after (this one is refused with `cannot_remove_self`: that is `logout`);
- `POST /v1/pause` / `POST /v1/resume` → status: while paused no sync pass starts (one under way finishes), and the
  long-poll, the folder watcher and the live connection hold; the local API and the apps' recents keep working (adds
  reach the server after resume). Kept in `state.json`, so a paused agent starts paused; `run_state` is
  `paused_by_user`, apart from `paused`, which needs someone;
- `POST /v1/quit` → status: the agent finishes the pass under way and exits (the answer goes out first);
- `POST /v1/autostart {on}` → status: the agent registers or removes its own start-at-login entry (the Windows `Run`
  value for the background build, a macOS launch agent, a systemd user unit or an XDG autostart entry), and
  `status.autostart` says whether it is on (absent where the agent doesn't handle it). Signing out turns it off. The
  entry belongs to the agent with the default data folder only; an agent with a data folder of its own is refused
  (`autostart_failed`), reports nothing and leaves the entry alone when it signs out;
- `GET /v1/status` → `{api, signed_in, account, server, folder, machine, online, run_state, autostart?,
  last_sync_ms?, removed}` (`last_sync_ms`: when the last pass that didn't fail finished; `removed`: the server
  refused this device at the last pass, so it was removed from the account and must sign out and join again). Something only a person can put
  right found as `run` starts (a damaged or newer `account.json`, a damaged token) keeps the agent up, signed out with
  `run_state` `paused` and the reason; every call but `logout` (which forgets the account here) and `quit` is answered
  `needs_you` until then;
- `GET /v1/recents` (WebSocket: subscribe per app, add an entry `{path, title?, saved}`, receive the list on change);
- `GET /v1/docs` (WebSocket: live documents). Control messages are JSON text frames, document bytes binary frames of
  `[u8 kind][u64 id, big-endian][bytes]` (kinds: 1 state, 2 update, 3 presence, 4 import), so nothing is base64; `id`
  is the number the app gave the document in its `open`, and one connection may open many documents:
  - `open {id, path, app}` (`path` relative to the synced folder, or absolute inside it; `app` a lowercase app name)
    → `opened {id, doc, state: {online, waiting, bytes}}` followed by the binary state (a Loro snapshot) and then, as
    binary updates, the updates still waiting for their history and this machine's own not yet acked; or
    `needs_import {id}` (the file has never been live: the app sends a binary import, a Loro snapshot, and then gets
    `opened`; the state is the import the server kept, which may be another machine's); or `not_live {id, reason,
    message?}`, with `reason` one of `offline` (never live here and no server with live documents to ask),
    `not_synced` (the file isn't uploaded yet: the agent starts a sync pass for it and waits up to 5 s for its
    node), `not_signed_in`, `too_large` (the file, or its import, is over the snapshot limit), `not_in_folder` (no
    such file in the synced folder) and `resetting` (the admin reset the server's document and no machine has sent it
    back yet: an app here would get an empty document). The app then opens and saves the file as always. A document
    that has been live on this machine opens offline from the agent's copy;
  - a binary update from the app (its own change, at most 1 MiB) → `ack {id, seq}` once it is on the agent's disk
    (`seq` counts the app's updates of that document on this connection, from 1), or `error {id, seq, code,
    message}` when the agent refused it (`invalid`: Loro doesn't take it; `too_large`); the agent's copy is as it was;
  - from the agent: remote changes as binary updates, other machines' and apps' presence as binary presence (as
    sent), and `status {doc, online, waiting, deleted, stopped}` when that changes (`deleted`: the file was deleted,
    here or on another machine; the document still works, and a later open of the path is a new node and a new
    document; a rename or move keeps the document, which belongs to the node; `stopped`: Loro failed on the
    document twice in this run, and the agent follows it again only once it restarts; changes made meanwhile are
    kept);
  - a binary presence from the app (at most 4 KiB, at most 20 a second per document) goes to the document's other
    machines and apps; `close {doc}`: the app is done with it (the agent keeps it);
  - `saved {doc, hash, frontiers, version}` (appended): the app saved the document's file; `hash` is the BLAKE3 hash
    of the bytes it wrote (hex), `frontiers` Loro's (hex, kept for the app's own use) and `version` the document's
    version vector as `[peer, end counter]` pairs, at most 4,096. The agent keeps the newest 512 in
    `live/<space>/saves.json` (atomically) and answers nothing; an `error` only when it doesn't look like a save. An
    agent from before save records answers `error` (not a request it knows) and nothing else changes: the file syncs
    as any file;
  - a binary frame without a header, or of a kind an app doesn't send, ends the connection; a text message that
    isn't a request is answered `error`. An app that doesn't take a message within 10 s is dropped (so is one whose
    messages queue up past 1,024 in the agent); its documents stay with the agent. An app that goes away mid-import
    leaves nothing on the server (the import goes up only once whole); the next app to open the file is asked.

- `PUT /v1/blobs` (the body: 1 byte to 64 MiB of binary content an app keeps out of a live document) → `{hash, len}`
  once the bytes are on the agent's disk (`live/<space>/blobs/<hash>`, with a `<hash>.pending` mark written first);
  `hash` is the BLAKE3 hash of the manifest the bytes cut into with `fastcdc-v1`, so the same bytes have the same
  name everywhere. The bytes go up through the chunk API (the chunks the server lacks, then the manifest, the
  account's storage cap applying) at once when the server can be reached, and otherwise with the next put or every
  30 s; the mark goes once they are up. `GET /v1/blobs/{hash}` → the bytes: from the agent's disk (checked against
  the name on every read; a damaged copy is dropped and fetched again), or downloaded (every chunk checked, the whole
  against the manifest) and kept. `blob_not_found` when the account has no such blob, `offline` when it isn't here
  and the server can't be reached. An agent from before blobs answers both with the local API's `not_found` for an
  unknown call, which `craft-cloud-app` reports as `agent_too_old` (blob calls only: everything else works as
  before).

Errors use stable codes (`forbidden`, `wrong_password`, `already_signed_in`, `not_signed_in`, `offline`,
`needs_you`, and for joining `invalid_code`, `password_required`, `weak_password`, `untrusted_certificate`,
`cannot_remove_self`, `autostart_failed`, `folder_has_files`, and for blobs `blob_not_found`). Every call has the same `Host`, `Origin` and token checks. Each request is answered before the next is read; a client that doesn't take a message within 10 s is
disconnected, and the crate reconnects.

**Server live endpoint** (agent ↔ server): `GET /api/v1/live` upgraded to a WebSocket, signed in with the device
token like every call (at most 4 connections per device). Binary frames encoded with postcard (compact, no base64
for opaque updates; variants are only ever appended, anything new is negotiated at `hello`), types in
`craft-cloud-proto`:
- `hello {min, max}` → `welcome {protocol, epoch}` (the space's epoch), or `error protocol_mismatch`. Protocol 2 adds
  documents of files, snapshots, compaction and presence; a connection on protocol 1 (an agent from before them, or
  a new agent and an older server) carries recents only, and is sent a compacted log whole. A new agent with an older
  server keeps recents and says live documents need a newer server; a document live on it already keeps its changes
  on the machine until then.
- `create {doc, kind}` → `created {doc}`; idempotent. Kinds: `recents:<app>`, whose ID is derived from the account and
  the app, so two machines that create it offline make the same one; and `document {node, app}`, a file of the synced
  folder, whose ID is `node_doc(space, node)` (BLAKE3 of both with a domain of its own, as a UUIDv8), so two machines
  that import the same file make one document. A document of a file is only made by import: `create {doc, node, app,
  snapshot}` (the snapshot's manifest appended to the frame) → `created {doc, kept}` (`kept` appended): the server
  checks the ID is the node's, the node a file of the space and the manifest one the account uploaded, and keeps the
  first one it sees; an agent whose import wasn't kept drops it and catches up from 0, and its app is told the
  document is live only once that is settled, so an app never edits a losing import. A recents `create` and `created`
  are byte for byte what they were. A joined document the server lost (a restore from before it was made) is made
  again from the agent's copy.
- **Snapshots never travel in frames.** A snapshot (a Loro snapshot of a whole document, at most 36 MiB) goes up as a
  file's content does: cut into chunks named by a manifest, uploaded through the chunk API (with its body limits,
  hash checks and the account's storage cap, `stored_bytes_per_user`) before the frame that names its manifest; the
  agent's other frames for that document wait behind the upload. Down, a frame names the manifest and the agent
  downloads it the same way, every chunk checked against its ID and the whole against the manifest; the catch-up's
  updates wait for it. One the agent can't upload isn't named (an import's open then answers `offline`); one it can't
  download, it follows the whole log instead. So no message on the live connection is over an update and its envelope
  (about 1 MiB), on any protocol.
- `subscribe {doc, after, at}` → the log above `after`, then `subscribed {doc, head}`, then live updates. **Catch-up is
  by sequence number**, not by Loro version, so the server never parses Loro: per-document numbers have no gaps,
  `after` is the highest number up to which the agent holds every update (an own ack that arrives before the updates
  below it isn't counted until they do, and its update is sent again on the next connection; the server's retry rule
  makes that free), and `at` is the BLAKE3 hash of the update at `after`. Whether one of the agent's own updates is
  stored is known by version, not by acks: an unacked update all of whose operations are in the rows (and the
  snapshot) the agent has taken is stored, whatever its ack does; an ack that came for it is kept until the rows
  below it are there.
- A restored or reset server is recognised by the epoch `welcome` names (checked again at every ping; the space's
  epoch hashed with its live generation), by an `at` that doesn't match its log (`error {resync_required, doc}`), or by
  a head behind the agent's, even when the log has grown back to the same length. The agent then catches up from 0
  and sends only the operations the log lacks: parts of at most 64 KiB, built from the missing changes in Lamport
  order (a change too big for one part goes in pieces, and a part never spans a gap), so each part applies once the
  earlier ones are in; each part is worked out when the last is acked. Its own operations (its apps' among them) go
  without a wait, and a change an app makes meanwhile goes at once as a plain update; others' go only
  after a random wait of 1–8 ticks, after which what the log lacks is worked out again; any update that comes in
  meanwhile restarts the wait (9–16 ticks), so pushers queue behind whoever is pushing. Duplicates arise only when two
  waits run out before either's part arrives (at the start, or when a pusher goes quiet): expected at most
  (k−1)(d+1)/8 extra parts each time, for k machines and d ticks of delay. Unacked adds travel inside the parts. Restore steps run `reset-epoch` before
  `serve`.
- A second `subscribe` to a document whose catch-up is still running is refused (`invalid`); catch-up is paged by
  bytes.
- **Snapshots and compaction**: when a document's log above its snapshot passes 1,000 updates or 1 MiB, the server
  sends `compact {doc, upto}` to one subscribed device that holds every update up to `upto` (the one that sent the
  update that crossed, or one that just caught up); one request per document at a time. That agent answers `snapshot
  {doc, upto, manifest}`, naming a Loro snapshot of exactly the updates 1..=`upto` (exported at the version those
  updates make as the agent took them, never including later ones: the agent tracks what the log holds, as a version
  per update), once it holds them and nothing waits for its history; otherwise it doesn't answer. An agent that
  dropped a row at or below `upto` (Loro failed on it, or it never applies) never answers, and says so. The server
  takes only the snapshot it asked that session for, and only a manifest the account uploaded: it records `base =
  upto` and the manifest in one metadata transaction, only past the current base (a second snapshot for an older or
  equal `upto` is ignored). The rows at or below the base are kept (compacted: no longer counted against the 16 MiB
  cap, and not sent to a protocol 2 agent that reads the snapshot) until the retention rules come; the retention
  rules must count `snapshot` and `import_snapshot` as references to their manifests. A device that doesn't answer
  within 30 s is skipped for 10 minutes and another is asked when it next appends or catches up. Recents documents
  compact the same way.
- **Catch-up with a snapshot**: a `subscribe {after}` with `after < base` (or `after` 0, for a document made by
  import) is sent `snapshot {doc, base, manifest}`, then the log from `base` on (the row at `base` too, so the agent
  learns its hash for the next `at`). The `at` check for `after < base` is against the stored hash of row `after`;
  an `after` the server can't check means `resync_required`. An agent that can't read the snapshot, or finds updates
  waiting after it for history the snapshot should have held, follows the whole log instead: `subscribe_log {doc,
  after, at}` is a `subscribe` that never gets a snapshot, but from 0 gets the document's import first (the log
  doesn't hold it; the server keeps it beside the snapshot).
- `update {doc, client_seq, bytes}` → `ack {doc, client_seq, seq}` once committed; a retry with the same
  `(device, client_seq, bytes)` is acked with its first `seq` and stored once. The server sends
  `update {doc, seq, bytes, device}` to every other subscriber.
- `presence {doc, bytes}` both ways, at most 4 KiB, at most 20 a second per document per device (more are dropped),
  relayed to the document's other subscribers and never stored. The agent forwards it to the apps as it is.
- `unsubscribe {doc}`; `error {code, message, doc?, client_seq?}`. A refused `create` or `subscribe` is retried with
  backoff.
- Another account's document gets the same `not_found` as a missing one. A revoked device's sockets are closed.
- Limits: one update 1 B to 1 MiB, one message at most an update and its envelope, one snapshot at most 36 MiB (an
  upload), at most 256 subscribed documents per
  device, 400 updates, creates and subscribes per connection refilled at 200/s; over them, `too_large` /
  `rate_limited` like the HTTP API.
- Quota: once a document's log holds 16 MiB (about 30,000 recents adds), every later append is refused with
  `quota_exceeded` (one-way: the update that crosses the cap is the last one stored). The agent keeps every unacked
  update, retries with a pause growing to about 64 s, and says the document is full.
  `craft-cloud-server live reset-doc <user> <app>` (or, for a document of a file, `<user> <document ID>`) empties the
  log, drops the snapshot and the import, and raises the space's live generation in the same transaction, so every
  live connection ends at its next ping and every agent starts over and pushes what the log lacks. The file sync's
  epoch is untouched, so it never sees a reset. Nothing a machine holds is lost. Only the log above the snapshot
  counts against the cap, so compaction keeps a document from filling up.
- Account caps: at most 64 of an account's recents documents may hold updates; the first update to one more is
  refused with `quota_exceeded`, and the agent handles it like a full document. Recents documents with no updates
  (made by following a list) are at most 1,024 per account; a create over that is refused with `quota_exceeded`, and
  that list stays on the machine and is tried again after a growing pause. Documents of files are counted apart (each
  needs a file of the space): at most 65,536 per account, at most 10,000 of them holding updates (the first update to
  one more is refused with `quota_exceeded`, handled like a full document), and their snapshots count against the
  account's storage (they are uploads). Every row of every live document of an account, compacted ones too, counts
  towards its live bytes, at most `live_bytes_per_user` (1 GiB by default); over it, every append is refused with
  `quota_exceeded`. A live document can't be deleted yet: `reset-doc` empties one (and gives its bytes back), but
  machines that hold its updates send them up again.
- The agent follows at most 256 documents (the server's cap) and keeps at most 256 documents of files in memory:
  past that, the least recently used document no app has open and with nothing to send or waiting is unfollowed, and
  written and let go of (it stays on disk, and is loaded and followed again when opened). At start only the documents
  with something to send or waiting are loaded. Its copy of each document is a file of its own beside a journal of
  the apps' changes since it was written (on disk before an app hears its change is kept; every record with its
  length and a checksum, read up to the first that doesn't read and cut there; an append that fails is cut back and
  refused, and the document written whole before its journal takes another); an older agent leaves those files
  alone.
- Loro failing on a row of a catch-up taken as a batch: the catch-up is taken again one row at a time, so only the
  row at fault is dropped (and said). A second failure in a run stops the document (`status.stopped`) until the agent
  restarts. A batch goes to Loro in groups of at most 16 KiB of updates (Loro's batch import of much text at once
  takes time growing with its square).

**Save records** (the file sync and live documents): each file version a Craft app saves records the live
document's version it was made from.
- The app tells its agent (`saved`, above); the sync engine's new version of a file whose content an app saved
  carries the record, `NewVersion.save = {doc, version}` (appended: an older agent sends none, an older server ignores
  it). The server keeps it with the version as the JSON it came as (`versions.save`, migration 0006; a record of more
  than 4,096 entries is kept as none) and lists it as `FileState.save` (an older agent ignores it). The server never
  reads what it means, and never parses Loro.
- When a file changed here and on the server, and this content and the server's version both carry a record of the
  same live document, the engine makes no conflict copy: the server's version wins if its record's version includes
  this one's (it was saved later from a document that had everything this one had: it is downloaded over the file);
  otherwise this one goes up over it (the newer upload; the live document holds both sides' edits, so the next save
  writes them all). A version without a record (edited outside Craft), or records of two documents, is a conflict as
  always, with its copy.
- A file whose bytes are no save (or the import) of its document is reported by the app kit on open
  (`changed_outside`): the app may bring the file in as an ordinary local change, or keep the live content. The kit
  keeps the newest 32 saves' hashes in the document's `$saves` map for that.

**The app kit** (`craft-cloud-kit`, what apps link; see `docs/kit.md`): the content's layout in Loro.
- The root map `root` holds `schema: {app, version}`, written at import. A document whose app differs, whose version
  is newer than the app's, or that has none opens read-only.
- Each part of an app's model has a key: a tree `k` (`LoroTree`, fractional index on, each node's meta map holding
  the element's ID under `$id` and its properties), a rich text `k` (`LoroText`: character attributes as marks
  expanding after, paragraph attributes as marks `¶<name>` on each paragraph's `\n` never expanding, the last
  paragraph's in the root map `k.last`), a table (`k.rows`, `k.cols`: movable lists of IDs; `k.cells`: a map from
  `row:column` to a value), a timeline (`k.tracks`: a movable list; `k.track`: track properties; `k.items`: item ID
  to `{$track, $start, $len, …}`; `k.keys`: keyframe ID to `{$item, $at, $value}`), an opaque value (the root map
  `$opaque`, key `k`, bytes). IDs are 16 bytes (little-endian `u128`), or 32 hex digits as map keys. Binary content
  is never in Loro: it is a `{$chunk, $len}` value naming a blob.
- Inside a tree node's meta map, a timeline item's or track's map, or a table's map cell, an app's codec can put a
  rich text and a table under a key of its own, as Loro's mergeable child containers (the same container on every
  machine that makes it, so two first edits at once merge): a story `s` is a `LoroText` at `s` laid out as a root
  one, its last paragraph's attributes in a mergeable map `$last.s` beside it (made only when there are some); a
  table `t` is a mergeable map at `t` holding mergeable `rows` and `cols` (movable lists of IDs) and `cells`. A cell
  is a value, or, when the table's source says its cells are maps, a mergeable map at `row:column` holding the
  cell's properties and stories. Deleting the key hides the child (a node's story that the app stops writing goes);
  a deleted node takes everything nested in it.
- A local change is written as commits of at most 512 KiB each (estimated: payload and 32 bytes per operation), each
  exported at once; an export still over 512 KiB is cut by counter range. One change is one undo step.
- Presence an app sets is sent as `[u64 peer, big-endian][the app's bytes]`, again every 3 s, and dropped 10 s after
  its peer goes quiet.
