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
    node), `not_signed_in`, `too_large` (the file, or its import, is over the snapshot limit) and `not_in_folder` (no
    such file in the synced folder). The app then opens and saves the file as always. A document that has been live
    on this machine opens offline from the agent's copy;
  - a binary update from the app (its own change, at most 1 MiB) → `ack {id, seq}` once it is on the agent's disk
    (`seq` counts the app's updates of that document on this connection, from 1), or `error {id, seq, code,
    message}` when the agent refused it (`invalid`: Loro doesn't take it; `too_large`); the agent's copy is as it was;
  - from the agent: remote changes as binary updates, other machines' and apps' presence as binary presence (as
    sent), and `status {doc, online, waiting, deleted}` when that changes (`deleted`: the file was deleted, here or on
    another machine; the document still works, and a later open of the path is a new node and a new document; a
    rename or move keeps the document, which belongs to the node);
  - a binary presence from the app (at most 4 KiB, at most 20 a second per document) goes to the document's other
    machines and apps; `close {doc}`: the app is done with it (the agent keeps it);
  - a binary frame without a header, or of a kind an app doesn't send, ends the connection; a text message that
    isn't a request is answered `error`. An app that doesn't take a message within 10 s is dropped (so is one whose
    messages queue up past 1,024 in the agent); its documents stay with the agent. An app that goes away mid-import
    leaves nothing on the server (the import goes up only once whole); the next app to open the file is asked.

Errors use stable codes (`forbidden`, `wrong_password`, `already_signed_in`, `not_signed_in`, `offline`,
`needs_you`, and for joining `invalid_code`, `password_required`, `weak_password`, `untrusted_certificate`,
`cannot_remove_self`, `autostart_failed`, `folder_has_files`). Every call has the same `Host`, `Origin` and token checks. Each request is answered before the next is read; a client that doesn't take a message within 10 s is
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
  snapshot}` (the snapshot appended to the frame) → `created {doc, kept}` (`kept` appended): the server checks the ID
  is the node's and the node a file of the space, stores the snapshot in the chunk store and keeps the first one it
  sees; an agent whose import wasn't kept drops it and catches up from 0, and its app is told the document is live
  only once that is settled, so an app never edits a losing import. A recents `create` and `created` are byte for
  byte what they were. A joined document the server lost (a restore from before it was made) is made again from the
  agent's copy.
- `subscribe {doc, after, at}` → the log above `after`, then `subscribed {doc, head}`, then live updates. **Catch-up is
  by sequence number**, not by Loro version, so the server never parses Loro: per-document numbers have no gaps,
  `after` is the highest number up to which the agent holds every update (an own ack that arrives before the updates
  below it isn't counted until they do, and its update is sent again on the next connection; the server's retry rule
  makes that free), and `at` is the BLAKE3 hash of the update at `after`.
- A restored or reset server is recognised by the epoch `welcome` names (checked again at every ping; the space's
  epoch hashed with its live generation), by an `at` that doesn't match its log (`error {resync_required, doc}`), or by
  a head behind the agent's, even when the log has grown back to the same length. The agent then catches up from 0
  and sends only the operations the log lacks: parts of at most 64 KiB, built from the missing changes in Lamport
  order (a change too big for one part goes in pieces, and a part never spans a gap), so each part applies once the
  earlier ones are in; each part is worked out when the last is acked. Its own operations go first; others' go only
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
  {doc, upto, bytes}`, a Loro snapshot of exactly the updates 1..=`upto` (exported at the version those updates make,
  never including later ones: the agent tracks what the log holds, as a version per update), once it holds them and
  nothing waits for its history; otherwise it doesn't answer. The server takes only the snapshot it asked that
  session for: it stores the blob in the chunk store (pieces of at most 4 MiB, charged to the account like chunks),
  fsyncs, then records `base = upto` and the blob in one metadata transaction, only past the current base (a second
  snapshot for an older or equal `upto` is ignored). The rows at or below the base are kept (compacted: no longer
  counted against the 16 MiB cap, and not sent to a protocol 2 agent that reads the snapshot) until the retention
  rules come. A device that doesn't answer within 30 s is skipped for 10 minutes and another is asked when it next
  appends or catches up. Recents documents compact the same way.
- **Catch-up with a snapshot**: a `subscribe {after}` with `after < base` (or `after` 0, for a document made by
  import) is sent `snapshot {doc, base, bytes}`, then the log from `base` on (the row at `base` too, so the agent
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
- Limits: one update 1 B to 1 MiB, one snapshot at most the manifest limit, at most 256 subscribed documents per
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
  needs a file of the space): at most 65,536 per account, and their snapshots count against the account's storage. A
  live document can't be deleted yet: `reset-doc` empties one, but machines that hold its updates send them up again.
- The agent follows at most 256 documents (the server's cap): past that, the least recently used document no app has
  open and with nothing to send is unfollowed (it stays on disk, and is followed again when opened). Documents loaded
  at start are followed only if they have something to send. Its copy of each document is a file of its own beside a
  journal of the apps' changes since it was written (on disk before an app hears its change is kept); an older
  agent leaves those files alone.
