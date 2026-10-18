# Craft Cloud — Live documents: protocols

> The protocol section of the live documents design, kept word for word in step with it (as `sync.md` is with the
> original design). The wire types are in `craft-cloud-proto` (`live` for the server endpoint, `local` for the local
> API).

## Protocols

**Local API** (agent ↔ apps): the loopback design from the original design ("Agent and launcher API": `127.0.0.1`,
random port, per-start token in an owner-only file `local-api.json {schema, port, token, pid}` in the agent's data
dir, exact `Host: 127.0.0.1:<port>`, any `Origin` refused, token only in `Authorization: Bearer`), plus one WebSocket
per app connection for live sessions. `run` serves it while signed out too. Calls:
- `POST /v1/login {server, user, password, folder?, device_name?, adopt?}` / `POST /v1/logout {local}` (the launcher's;
  the password goes from the launcher to the agent, never to an app or a file);
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
- later: `open` (a path → its live document, created by import when absent; returns the state and then a stream
  of remote updates and presence), `update` (the app's local changes), `presence`, `close`.

Errors use stable codes (`forbidden`, `wrong_password`, `already_signed_in`, `not_signed_in`, `offline`,
`needs_you`, and for joining `invalid_code`, `password_required`, `weak_password`, `untrusted_certificate`,
`cannot_remove_self`, `autostart_failed`, `folder_has_files`). Every call has the same `Host`, `Origin` and token checks. Each request is answered before the next is read; a client that doesn't take a message within 10 s is
disconnected, and the crate reconnects.

**Server live endpoint** (agent ↔ server): `GET /api/v1/live` upgraded to a WebSocket, signed in with the device
token like every call (at most 4 connections per device). Binary frames encoded with postcard (compact, no base64
for opaque updates; variants are only ever appended, anything new is negotiated at `hello`), types in
`craft-cloud-proto`:
- `hello {min, max}` → `welcome {protocol, epoch}` (the space's epoch), or `error protocol_mismatch`.
- `create {doc, kind}` → `created {doc}`; idempotent. For now the only kind is `recents:<app>`, whose ID is derived from
  the account and the app, so two machines that create it offline make the same one. Later: `create {doc, node,
  app, snapshot}` for a document made by import.
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
- `update {doc, client_seq, bytes}` → `ack {doc, client_seq, seq}` once committed; a retry with the same
  `(device, client_seq, bytes)` is acked with its first `seq` and stored once. The server sends
  `update {doc, seq, bytes, device}` to every other subscriber.
- `presence {doc, bytes}` (later) relayed to the other subscribers, never stored.
- `unsubscribe {doc}`; `error {code, message, doc?, client_seq?}`. A refused `create` or `subscribe` is retried with
  backoff.
- Another account's document gets the same `not_found` as a missing one. A revoked device's sockets are closed.
- Limits: one update 1 B to 1 MiB, one snapshot at most the manifest limit, at most 256 subscribed documents per
  device, 400 updates, creates and subscribes per connection refilled at 200/s; over them, `too_large` /
  `rate_limited` like the HTTP API.
- Quota: once a document's log holds 16 MiB (about 30,000 recents adds), every later append is refused with
  `quota_exceeded` (one-way: the update that crosses the cap is the last one stored). The agent keeps every unacked
  update, retries with a pause growing to about 64 s, and says the document is full.
  `craft-cloud-server live reset-doc <user> <app>` empties the log and raises the space's live generation in the same
  transaction, so every live connection ends at its next ping and every agent starts over and pushes what the log
  lacks. The file sync's epoch is untouched, so it never sees a reset. Nothing a machine holds is lost. A document
  whose own history is over the cap stays full until the log can be compacted, which is the real fix.
- Account caps: at most 64 of an account's live documents may hold updates; the first update to one more is refused
  with `quota_exceeded`, and the agent handles it like a full document. Documents with no updates (made by following
  a list) are at most 1,024 per account; a create over that is refused with `quota_exceeded`, and that list stays on
  the machine and is tried again after a growing pause. A live document can't be deleted yet: `reset-doc` empties
  one, but machines that hold its updates send them up again.
