# The agent on the command line

With a server running, sign in on each machine and sync:

```sh
cargo build --release -p craft-cloud-agent
# The password is read from stdin. The folder is created if it doesn't exist.
printf '%s\n' 'your password' | ./target/release/craft-cloud-agent login https://cloud.example.org --user you --folder ~/'Craft Cloud'
./target/release/craft-cloud-agent run       # keep it in step until Ctrl-C
./target/release/craft-cloud-agent status    # account, folder, whether `run` runs and what it does, last pass
./target/release/craft-cloud-agent sync      # or: one pass by hand
```

- `run` syncs whenever something changes in the folder (it watches it) or on the server (it waits on the
  server's change feed), and at least every 10 minutes. A file that is still being written (its size or
  time changed within the last 2 seconds, or on Windows a program still has it open for writing) waits
  until it settles; a file that keeps changing (a log, a database held open) is looked at less and less
  often, at least every 5 minutes. When the server can't be reached it tries again with growing pauses, up to 5 minutes.
  If something needs you (the folder or its marker is missing, the device was signed out) it pauses and
  `status` says why; a missing folder or marker is looked at again every minute, and syncing goes on by
  itself once it is back. When the server itself is full (its admin turned on `refuse_when_full` and the server's disk
  or capacity is used up) the agent pauses with "The server is full; the admin needs to free space or raise its
  capacity" (never the account's storage message, which means the account's own quota) and looks again every 10
  minutes; nothing is lost, and syncing goes on once the admin has freed space. Ctrl-C lets a running pass finish and stops; a second Ctrl-C stops at once.
- While `run` runs, `sync --approve-deletes` from another terminal hands the approval to it: exactly the
  deletions `status` listed. If more came since, it lists them instead, approves nothing and exits 2; check
  the list and run it again. Every list of waiting deletions comes with its set (`set 3f9a…`), also in
  `run`'s output: `sync --approve-deletes=<set>` approves exactly that set if it is the one waiting, without
  `status` first, and otherwise shows the waiting set, approves nothing and exits 2. A plain `sync` says the
  agent is already running.
- A team space's held deletions are approved with `sync --approve-deletes[=<set>] --space <id or name>` (the same rules,
  with the space's own list and `spaces/<id>/approval.json`; it needs the running agent), or by the local API's
  `POST /v1/spaces/{id}/approve-deletes`. Each computer approves its own side: deletions made on one computer wait for
  its approval, and again on the others when they arrive.
- A delete held back because a link appeared that may be the file, moved with its folder ("held while … is a
  link", or "held while any of … is a link" when several links appeared with that name), comes with a set of
  its own. If you really deleted the file and none of the links listed is it, `sync --approve-deletes=<set>`
  with that set lets the delete go; nothing else (the bare flag included) releases it.
- `craft-cloud-agent autostart on` makes the agent start by itself at every login (`join --autostart` does it
  when joining; `autostart off` and signing out undo it): on Windows a `Craft Cloud` value under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` that runs `craft-cloud-agent-bg.exe run --log`, the build
  without a console window; on macOS `~/Library/LaunchAgents/ch.craftcloud.agent.plist`; on Linux the systemd user
  unit `craft-cloud-agent.service` (a unit of that name made by hand is taken over), or
  `~/.config/autostart/craft-cloud-agent.desktop` without systemd. Each runs `run --log`, which writes what the agent
  says to `run.log` in its data folder (rotated at 10 MiB). After a crash it is started again (after 30 s), but not
  after a clean quit, and on Linux not after exit code 3. `status` through the agent's local API says whether it is on. The entry belongs to the agent with the
  default data folder: one run with `--data-dir` or `CRAFT_CLOUD_AGENT_DIR` elsewhere (a spare folder) never sets,
  replaces or removes it.
- To sync a folder that already has files, add `--adopt` to `login`.
- `sync` exits 0 when done; 1 when it failed and can simply run again (offline, server busy); 2 when
  deletions wait for approval (`sync --approve-deletes` releases exactly the ones it listed); 3 when it
  needs you (the folder or its `.craft-cloud` marker is missing, another agent is running, the device was
  signed out, and so on). The message says what to do.
- HTTPS is required, except to a server on the same machine. Proxy settings are ignored for now.
- `logout` signs the device out on the server and forgets the account here; your files stay. Use
  `logout --local` if the server is gone.
- `run` also starts without an account: it waits for a sign-in from the Craft Cloud app (or `join`, `login`),
  then syncs. A second `run` for the same data folder says another agent runs and exits 0. A damaged account file
  found as `run` starts keeps it waiting (its status says what is wrong) until it is put right or a sign-out
  forgets it.
  While `run` runs, `login` and `logout` go through it (it holds the data folder), and a sign-out stops
  syncing and keeps the files.
- Instead of a password, a machine can join with a code. On a machine that is signed in and running,
  `craft-cloud-agent add-computer` prints a join string (`cloud.example.org/K7M4-Q9TX`, good once for 10 minutes)
  and waits; on the new machine, `craft-cloud-agent join cloud.example.org/K7M4-Q9TX` (with `--folder`, `--name`
  and `--adopt` as for `login`) signs it in. An admin's invite (`user invite`, in [`server.md`](server.md)) works the same way; for a new
  account, `join` asks for its password twice on standard input. A code that doesn't work says so, without saying
  why: make a new one.
- An account with two-factor sign-in (turned on in the app's Security page; see "Two-factor sign-in" in
  [`server.md`](server.md)) asks for a second step at `login` and at `join`: after the password, the command prints
  `This account has two-factor sign-in. Type the code from your authenticator app (or a recovery code):` and reads one line
  from standard input, so a script gives the password line and then the code line. The code is never an argument and
  is never stored; a wrong one may be typed again twice (the server itself allows five wrong codes per sign-in). Six digits
  are a code from the app; anything else is taken as a recovery code, which works once. Too many wrong codes lock the
  account's second step for a while: the command says how long, and the lock does not sign anything out. For a `join` that
  needs a code the invite isn't used up until the code is right. A running agent (`run`) does the same through its local
  API; the Craft Cloud app asks for the code in its own window.
- Rolling this out: update the server first, then the agents and the app. An agent from before two-factor sign-in can't
  complete a sign-in for an account that has it, and it reports that as a wrong password; update the agent, nothing is
  broken. The app on a server that doesn't know two-factor sign-in says so on its Security page instead of failing.
- Through the running agent: `pause` and `resume` syncing (a pause holds across restarts; recents keep working),
  `quit` the agent, and `devices` to list the account's machines (`devices remove <id>` takes one out; to take this
  machine out, use `logout`).
- The desktop reads storage through the authenticated local API, `GET /v1/usage`. It asks the server for every
  request and returns `AccountUsage` unchanged. Signed out, it returns `409 not_signed_in`; when the server cannot be
  reached, it returns `503 offline`. The desktop shows trash and old-version values as overlapping estimates and
  never adds them to stored bytes.
- Recent files, per app, shared by every machine of the account (through the running agent, as an app
  sees them):

  ```sh
  ./target/release/craft-cloud-agent recents wordcraft                          # the list, newest first
  ./target/release/craft-cloud-agent recents wordcraft add Letters/doc1.docx    # a path in the synced folder
  ./target/release/craft-cloud-agent recents wordcraft --watch                  # again after every change
  ```

  An entry added offline is kept and reaches the other machines once the server is back. Each list keeps
  the 100 files opened last.
- A file of the synced folder as a live plain-text document (through the running agent, as an app opens one; a
  test tool for live documents):

  ```sh
  ./target/release/craft-cloud-agent doc Notes/todo.txt                    # its text
  ./target/release/craft-cloud-agent doc Notes/todo.txt --append "eggs"    # add to it, on every machine
  ./target/release/craft-cloud-agent doc Notes/todo.txt --watch            # again after every change
  ```

  The first machine to open a file makes it a live document from the file's content (UTF-8 text only); the others
  join it. Changes go to every machine that has it open as they are made, and edits made offline merge when the
  machine is back. `doc` never writes the file: what it shows is the live document.

## The local API's admin calls

For the Craft Cloud app's admin pages the running agent passes the server's admin API on (`/api/v1/admin/…`, see
[`server.md`](server.md), "Managing accounts over HTTP") under `/v1/admin/…`, with the signed-in device's token. They
have the same checks as every local call (the `Host`, no `Origin`, the token in `Authorization: Bearer`), bodies of at
most 8 KiB, and need the agent to be signed in (`not_signed_in` otherwise).

| Local call | Server call | Body, answer |
| --- | --- | --- |
| `GET /v1/admin/server` | `GET /admin/server` | the server's counters and kinds, and its health (`started_ms`, `database_ok`, `store_ok`, `store_check_ms`, `refusing_uploads`, all optional); a member is refused `forbidden` |
| `GET /v1/admin/storage` | `GET /admin/storage` | bounded storage overview: totals, quota counts, top ten accounts, manifest-index counts, and the room the server has (optional fields) |
| `PUT /v1/admin/storage/compression` | `PUT /admin/storage/compression` | `{level, password, totp_code?, recovery_code?}` → the compression part of the overview; the zstd level (1 to 19) new blobs are written at |
| `GET /v1/admin/users?cursor&limit&query` | `GET /admin/users` | a page of accounts by username; `next` is the cursor of the following page; `query` finds a username containing it |
| `POST /v1/admin/users` | `POST /admin/users` | `{username, role, password, totp_code?, recovery_code?}` → the account and its invite code |
| `PATCH /v1/admin/users/{id}` | `PATCH /admin/users/{id}` | `{role?, quota_bytes?, password, totp_code?, recovery_code?}` → the account (`null` removes the quota) |
| `POST /v1/admin/users/{id}/disable`, `…/enable` | the same | `{password}` → 204 |
| `DELETE /v1/admin/users/{id}` | the same | `{password}` → 202 |
| `POST /v1/admin/users/{id}/invite`, `…/password-reset` | the same | `{password}` (a reset code: also `totp_code?`, `recovery_code?`, `reset_two_factor?`) → `{code, join_url?, expires_ms}` |
| `DELETE /v1/admin/users/{id}/invite` | the same | `{password}` → 204 |
| `GET /v1/admin/users/{id}/devices` | the same | every device of the account |
| `GET /v1/admin/users/{id}/usage` | `GET /admin/users/{id}/usage` | the account's stored bytes, effective quota, and labeled trash/history estimates; unknown or removed account is `not_found` |
| `DELETE /v1/admin/users/{id}/devices/{device_id}` | the same | `{password}` → 204 |
| `POST /v1/admin/users/{id}/reset-two-factor` | the same | `{password, totp_code?, recovery_code?}` → 204: takes the account's two-factor sign-in away and signs its computers out |
| `GET /v1/account` | `GET /account` | the signed-in account: `role`, `two_factor_enabled` (how the app finds out whether to show its admin pages) |
| `GET /v1/account/activity?cursor&before_ms&limit` | the same | the account's own security events, newest first, any account (`user` and `action` are not read) |
| `GET /v1/admin/audit?cursor&before_ms&limit&user&action` | `GET /admin/audit` | the whole log, newest first; `user` an account ID, `action` an event name or a prefix like `auth.` of the closed list; a cursor is passed back as it came |
| `GET /v1/admin/audit/export?before_ms&user&action` | `GET /admin/audit/export` | the log as NDJSON, **streamed** through (chunked): event lines, then a last line `{"complete":true,"exported":n}` or `{"truncated":true,"exported":n}` |

- **The server decides.** The agent checks no role and makes no decision of its own; it reads each body as the shared
  type, writes it to the server again and answers what the server answered, with the server's HTTP status, error code and
  message. A wrong password stays `403 wrong_password`, a member's call `403 forbidden`, a disabled admin's
  `403 account_disabled`, the last enabled admin's demotion `409 last_admin`, too many tries `429 rate_limited` with the
  server's `Retry-After` header. The one status the agent does not pass on is the server's `401`: on a signed-in route it
  says this device was revoked, and the local API answers it as every other call does (`409 needs_you`); a local `401` always means a wrong token.
- **The confirmation is not kept.** Every change carries the acting admin's current password in its body, from the app
  through the agent to the server. The agent holds it only for the length of the request, never writes or logs it, and
  the `Debug` output of every body hides it. The codes the server makes (an invite, a password reset code) are shown by
  the app once and kept by neither side.
- **A second factor** joins the confirmation when the admin's account has two-factor sign-in: `totp_code` (a code from the
  authenticator app) or `recovery_code`, in the same body as the password, passed on as typed fields and never kept. The server
  answers a missing or wrong one `403 two_factor_invalid` and too many `429 two_factor_locked` with the wait (the body's
  `retry_after_secs` and the `Retry-After` header); neither is a revoked device, so the agent stays signed in. A password reset
  code takes the whole `AdminPasswordReset` (the same fields and `reset_two_factor`, which makes redeeming the code turn the
  account's two-factor sign-in off).
- **The audit export is never held whole.** The agent passes the server's stream through chunk by chunk; a client that
  reads it (`craft-cloud-app`'s `admin_audit_export`) counts it as whole only with the server's `complete` line and the number
  of events it says, and takes a stream that ends without it (a broken connection, a server that failed in the middle) for a
  failed export. `truncated` is a good export of the newest events that is not the whole log. Filters and limits are checked
  before anything is sent: an `action` outside the closed list is `400 invalid`, as is a `limit` of 0 or over 200 (100 for an
  account's own activity).
## Local API: file history

The local API exposes server history to desktop clients. Paths are relative to the synced folder and resolve through
the agent's saved sync state; unknown or unsynced paths return `404 not_found`, paths outside the root return
`400 invalid`, and folders return `400 invalid`.

| Method and path | Request | Success |
|---|---|---|
| `GET /v1/files/history?path=<relative-path>&cursor=<opaque>&limit=<count>` | Query parameters; `cursor` and `limit` optional | `200` with `{versions, next}`; newest first |
| `POST /v1/files/restore` | `{"path":"Notes/todo.txt","version":"…","expected_version":"…"}` | `200` with the new version, its parent, sequence and epoch |

Restore requires the file's current version to match `expected_version`; otherwise the API answers `409 conflict`.
The agent does not write restored bytes directly. Its normal change feed picks up the new server version and syncs it
to disk.

## Local API: trash

Trash items are addressed by their server node ID because deleted nodes are absent from the agent's sync tree. The
agent validates that each ID has the expected UUID form before forwarding it. List responses may include `path`, a
display hint formed from a currently synced parent folder and the item's name; it is `null` when that parent is not
live on this machine. A missing hint does not fail the request.

| Method and path | Request | Success |
|---|---|---|
| `GET /v1/trash?cursor=<opaque>&limit=<count>` | Optional paging query parameters | `200` with `{items, next}`, newest deletion first |
| `POST /v1/trash/restore` | `{"id":"<node-id>"}` | `200` with restored node, parent, name, rename flag, version (for files), sequence and epoch |
| `POST /v1/trash/remove` | `{"id":"<node-id>"}` | `204` |

A successful restore asks the agent for a normal sync pass; it never writes files directly. Removing an item only
marks it out of the trash. It cannot be restored from the trash afterward, but stored space is not freed yet. Server
HTTP status and error code are preserved; a server `401 unauthorized` becomes local `409 needs_you`.

## Local API: conflict copies and folder sizes

Both calls answer from the agent's saved sync state (`state.json`) alone. They do not ask the server, do not wait for a
pass (the state file is replaced atomically and read without a lock), and show what the last finished pass recorded.
Signed out they answer `409 not_signed_in`, like `GET /v1/usage`. Paths are relative to the synced folder and `/`-separated.

| Method and path | Success |
|---|---|
| `GET /v1/conflicts` | `200` with `{copies, truncated}` |
| `GET /v1/usage/folders` | `200` with `{total_bytes, total_files, folders, other?, kinds}` |

**Conflict copies.** `copies` holds `{path, original_path?, device?, at?, at_utc, at_ms?, size, mtime_ms}`, newest first (by
the minute in the name, then its counter), at most 1000; `truncated` is true when older ones were left out. A synced
file is listed when its name is one the engine makes (`name (conflict, <device>, YYYY-MM-DD HH-MM[Z][, n]).ext`, see
`docs/design/sync.md`) **and** a file with the original's name sits beside it, or when the last finished pass reported
it as a copy it made. For a reported copy whose name does not follow the pattern, `original_path`, `device` and `at` are
left out. `at` is `YYYY-MM-DD HH:MM` in the local time of the computer that made the copy; `at_utc` says it is UTC (that
computer could not read its offset), and only then is `at_ms` given, since a local time has no offset in the name.
`size` and `mtime_ms` are the copy's as this computer last synced it. Notes:

- A user's own file can match the pattern and have a sibling of that name, so the list can show a file that is not a
  conflict copy. The name cannot tell; the desktop words it as "looks like a conflict copy".
- A copy whose original was deleted or renamed since is missed, unless the last pass made it. Copies of folders are not
  listed. A stem shortened to fit the 255-byte name limit has no sibling under its shortened name, so such a copy
  shows only while the last pass reported it.
- Copies are kept (a conflict keeps both versions); this call only lists them. Removing a copy is deleting the file.

**Folder sizes.** `total_bytes` and `folders[].bytes` are **local logical bytes**: the sizes of the files this computer
has synced, as the last pass recorded them. They are not the server's stored bytes (`GET /v1/usage`), which count each
chunk once and include history and trash. `folders` has one entry `{name, bytes, files}` per top-level folder (the first
path component; the files directly in the synced folder count under `""`), the largest first (by bytes, then name), at
most 50. When there are more, `other` is `{folders, bytes, files}` for the rest. `kinds` always lists all eight classes
in the order `image`, `video`, `audio`, `document`, `model_3d`, `archive`, `code`, `other` with `{kind, bytes, files}`;
the class comes from the file's extension (case-insensitive; no extension or an unlisted one is `other`; `code` includes
markup and plain text). Only files count, so empty folders add nothing.
## Team spaces

The running agent syncs the personal space as ever, and any team space the person turns on. Everything below is off for
a server that speaks HTTP protocol 1 (before team spaces): the agent asks that server about its version only, sends no
`craft-space` header, and lists no team space.

- **Which spaces.** `GET /spaces` is asked at the start, then every two long-poll waits, and at once after an invitation is
  accepted or declined here or a space answers `404`. A failed look changes nothing.
- **Folders.** A team space syncs to `Craft Cloud Shared/<name>/`, a folder beside the personal one (so synced folders never
  nest). The name is the space's name with what a file name can't hold made `_`; a name that is taken, by another space or
  by a folder that is already there, gets ` (2)`, ` (3)`. The folder is chosen when the space is first seen and kept when
  the space is renamed on the server. Each folder has its own root marker (`.craft-cloud/root.json`).
- **State.** `account.json` gains `spaces: [{space, folder, sync, name, root}]` (left out while there are none, so a data
  folder from before team spaces is not rewritten). Each synced team space keeps `spaces/<space id>/state.json` in the
  data folder; the personal space's files stay where they were. The state records the space and the canonical folder it
  was made for, and is used for that pair only: for any other folder (the space moved, or turned off and on elsewhere,
  and back) the space starts afresh as a first sync, merging the folder with the space and deleting nothing on either
  side. The same goes when the server's stamp of the account's membership (`joined_ms` in `GET /spaces`, new each time
  someone is removed and invited again) differs from the one the state was made under, which also covers an agent that was
  not running while access was lost and regained; a server that gives no stamp is not compared. Turning a space off
  forgets its state, so turning it on again, in the same folder or another, is a first sync too. Moving a space that
  syncs, or turning it off, ends its session and waits for its running pass before anything else changes; the agent goes
  on meanwhile (other spaces, the local API, stopping), and the call is answered when the session is closed (after 50
  seconds at most). A pass that was queued behind another pass when its space was closed (or the agent began to stop) is
  not run. Another change of the same space meanwhile is refused with `409` ("this space is being changed, try again in a
  moment"). Turning on a space that has no session or pass here does not wait for another space's pass; if the space's own
  last pass has not ended after 50 seconds the answer is "this space is still finishing its last pass, try again in a
  moment". If the agent stops while a space is being turned off, the space stays off. Signing out removes the team spaces' states too; their folders, markers and files stay.
- **A state is trusted only when nothing doubtful happened to it.** Each team space's setting in `account.json` has a
  `generation` (left out while 0), and `state.json` records the generation it was made under; a state under another
  generation is not used. The generation is raised and saved before a state is retired (turning the space off, losing
  access) and before a new folder's marker is written, so a state that survives its removal, or a crash between the marker
  and the removal, is never trusted again. If the state cannot be removed (something holds the file), turning the space off
  fails with that error instead of reporting success. Before every pass the session also compares the folder's canonical path
  and ID with those it opened on: a link or junction pointed at another folder meanwhile ends the session, drops the state and
  starts the space afresh there as a first sync. It does the same just before the pass sends deletions, and a saved state also
  records the folder's ID, so a folder restored from a backup at the same path is not the folder the state was made for. If a
  raised generation cannot be saved, it is saved again at the next look at the list and that space is not started meanwhile (its state says why); other spaces go on.
- **A first sync merges, and says what that means.** Without a saved state, nothing is known to have been deleted, so
  nothing is deleted on either side. A file that teammates deleted on the server meanwhile, and that is still in the
  folder, is uploaded again. A file that exists on both sides with different content is not overwritten: the local one is
  moved aside as a conflict copy and the server's version takes its path (the local file no longer sits at its path). A
  file only here goes up, a file only on the server comes down, and equal files are adopted.
- **Off by default.** An accepted space does not sync until `POST /v1/spaces/{id}/sync` with `{"on": true}` (optionally
  `folder`, an absolute path that lies outside every synced folder and the data folder, and `adopt` for a folder that already
  has files: what differs from the space's becomes conflict copies, nothing is deleted).
- **One engine per space.** Each synced space has its own session (state, root, marker, guards), scheduler, folder watcher
  and long-poll, over a transport bound to the space. Passes run one at a time across all spaces. A problem in one space
  never pauses another or the personal space. `pause`, `resume` and `quit` apply to every space.
- **The echo.** Every answer for a team space must carry `craft-space` with that space (success, `403`, `404`, `409`, `410`).
  A missing or different one stops that space with "the server is too old for team spaces": a server that ignored the header
  would otherwise answer about the personal space.
- **Read-only.** A role of viewer, or an archived space, puts the space's engine in read-only mode (see the sync design,
  "Read-only spaces"); `read_only` in the space's entry says so. The server lists an archived space with the member's
  stored role (an editor stays `editor`) although their access is capped at viewer; read `archived`, not `role` alone, to
  know whether the member may write.
- **Losing access.** A space the server no longer lists, or answers `404`, stops syncing and shows
  `run_state: {"kind": "no_access"}` with "You no longer have access to <name>. Your local copy is kept and no longer
  syncs." Nothing in its folder is touched or deleted. Its saved sync state is dropped, so if the account is added again the
  folder is merged with the space afresh (a first sync, above): no local file is deleted, a file deleted here meanwhile is
  not deleted on the server, one deleted on the server meanwhile is not deleted here (and goes up again), and a file that
  differs on both sides is kept twice (one as a conflict copy). A member of an archived space has not lost access: the space stays
  listed (`archived: true`) and syncs read-only.

- **Live documents in team spaces.** Each synced team space also has a live link of its own (the server's live protocol 3,
  `docs/design/live.md`), started and stopped with the space's sync; the agent speaks protocol 3 when the server offers
  it and protocol 2 otherwise, and a server whose live service cannot serve a team space (a `404` for the link) is asked
  again only after ten minutes, while the space's files sync on and its documents open as plain files (`not_live`,
  `offline`). A document is routed by the path the app opens: an absolute path inside a synced team folder is the space's,
  any other path the personal space's. For a viewer (or an archived space) `opened` says `read_only: true`; an update the
  app sends is refused with `read_only` and kept nowhere. When the account's access changes while connected (a role
  raised or lowered, the account removed) the server ends the link with `access_changed`; the agent connects again once
  and learns the new role, or that it is no longer a member, in which case the space's link stops with the space.
  Changes that were never acked stay in the document's journal under `live/<space>/` through all of this: they go up when
  the account may write again, and while it may not, they stay on this computer (the agent's log says how many, by space ID
  only; the document opens with them in `waiting`). An app that opens with `identity: true` is told who is there (see
  `docs/design/live.md`); one that does not gets what it always got. The agent's log and its debug output carry space and
  account IDs, never names. Ctrl-C with an idle personal space and a team pass that hangs (a server that does not answer)
  waits for that pass; a second Ctrl-C, read meanwhile, ends the wait. A space's live folder (`live/<space>/`) belongs to one hub
  at a time (a lock file, `.lock`): a hub that replaces one that was stopped (sync turned off and on, a folder moved) starts
  when the old one has ended, so no change an app was told was kept is overwritten. When a team space stops syncing, the
  documents an app has open in it are told `status {stopped}` and a later change to one is answered `error {not_found}`;
  the app's documents in other spaces go on. A viewer's recents `add` is refused with `read_only`. A role change is told to the apps: every open document of the space gets `role {doc, read_only}` when the account's right to change
  the space changes (both ways; `opened.read_only` is as of the open), so an app that kept a refused change can send it
  again. An app that ends while it still holds such changes hands them over as binary kind 6 (`held`, one update per frame, in
  order): the agent puts them into the document's journal like an update (the other apps with the document open get them)
  acks each once it is on disk (an app that ends waits for that before it closes) and never sends them while the account may not write; they go up with the first connection of an
  account that may write, with no app open.

| Local call | Answer |
| --- | --- |
| `GET /v1/spaces` | `{spaces: [{space, kind, name?, role, state, archived, folder?, sync, run_state?, last_sync_ms?, read_only}]}`: the personal space first, then the team spaces, invitations (`state: "invited"`, no folder) among them. `sync` is `on`, `off` or `unavailable` |
| `POST /v1/spaces/{id}/accept` | the server's answer: the space as it lists it |
| `POST /v1/spaces/{id}/decline` | 204 |
| `POST /v1/spaces/{id}/sync` | `{on, folder?, adopt?}` → the space as `GET /v1/spaces` lists it |
| `POST /v1/spaces/{id}/approve-deletes` | `{set?}` → `{approved, message, set?, total, truncated, entries[{path, side}], held_set?, held_total, held[{path, links}]}`: approves the deletions the mass-delete guard holds back in a synced team space (`spaces/<id>/approval.json`), as `craft-cloud-agent sync --approve-deletes[=<set>] --space <id or name>` does. With a `set` only that set, if it is the one waiting; a set that is not waiting (an app sends `look`) approves nothing and lists the waiting one: `set` is its name (a hash of exactly those deletions), `total` their number, `entries` up to 1000 of them (`side` is `server` or `here`; `truncated` says there are more, and the paths are cut at 512 KiB), `held` the deletes held behind a link (up to 100; only the command line releases them, by naming `held_set`). `message` is the command line's words, cut at 16 KiB. Without a `set`, the set the previous call listed |
| `GET /v1/spaces/{id}/members`, `POST` (`{username, role}`), `PATCH …/members/{user}` (`{role}`), `DELETE …/members/{user}` | passed on to the server as they are (`SpaceMembers`, `SpaceMember`, 204): who may do what is the server's decision, so an editor's invitation is `403 read_only` and the last owner's demotion `409 last_owner` |
| `GET /v1/spaces/{id}/usage` | `{stored_bytes, quota_bytes?, …}`, passed on |
| `PATCH /v1/spaces/{id}` | `{name?, quota_bytes?, password?, …}` passed on (a rename by an owner; a limit by an administrator with the confirmation) → the space as the administrator's list shows it |
| `POST /v1/spaces/{id}/leave` | 204: leaves the space on the server (the account's own `DELETE …/members/{its id}`); a refusal (the last owner: `last_owner`, a rate limit, no connection) changes nothing here and the space keeps syncing. The agent looks at its list again; the space is then lost to it: `no_access`, the sync ends, the folder and its files stay |
| `GET /v1/admin/spaces?cursor&limit&query`, `POST` (`AdminCreateSpace`) | the administrator's list of team spaces and the creation of one, passed on with the confirmation in the body |
| `POST /v1/admin/spaces/{id}/archive`, `…/unarchive` (`{password, …}`), `PUT …/owner` (`{user, password, …}`) | passed on, 204 |
| `GET /v1/status` | gains `spaces: [{space, run_state, last_sync_ms?, read_only}]`, one per synced space or one that lost access; the top-level fields describe the personal space |
| `POST /v1/spaces/from-folder` | `{folder, name, quota_bytes?, password?, totp_code?, recovery_code?}` → `FromFolderStatus`: "Make a team space from this folder" (below). An administrator's call: the confirmation is the administrator's own. The call checks the folder and makes the team space (a wrong password, a taken name, an account that is no administrator, a server without team spaces are the call's errors, and nothing exists then), then answers while the rest goes on in the agent |
| `GET /v1/status` | also has `from_folder`: the work above, under way or how the last one ended (`step`, `space?`, `team_folder?`, `done`/`total` files of the step, `failed_at?`, `message?`, `kept`) |
| `GET /v1/recents` (WebSocket) | `subscribe {app, space?}` follows a team space's list (it must sync here, else `not_found`); an `add` with an absolute path inside a team folder goes to that space's list; `entries` names the `space` |
| `GET /v1/docs` (WebSocket) | `open` gains `identity`; `opened` gains `read_only` and `space`; new binary kind 5 (presence with the sender), kind 6 (held changes from an app) and text messages `members` and `role` |
| `PUT /v1/blobs?for=<path>`, `GET /v1/blobs/{hash}?for=<path>` | the blob of the space of the file at `path` (a team space's, read by every member's agent); without `for`, the personal space |

**Who has a recent file open.** A recent entry may carry `open_now`, a list of `{name, colour}` (the username and the palette index 0 to 11 of `craft-cloud-api::colour`): the other accounts of the space that have the file open right now. It is filled in when the list is handed out, from the presence the agent hears for documents it already follows, that is, files an app on this computer has open (the server relays presence only to connections that follow the document, and the agent follows nothing for a list). The account the agent is signed in as is never listed, an account whose name the member list lacks is left out, and an account fades ten seconds after its last presence. The list is handed out again when somebody new appears, and to every new subscription, so a client that wants the current picture asks again; there is no timer and no extra traffic. The field is absent when nobody else is there and from an older agent, and a reader that does not know it ignores it. It does not say whether the person edits or only looks: the role is not in the data. Apps are told nothing new: the entries are the same as before, apart from this field, and the field is never written to the shared list. A debug print of an entry shows no name.

**Make a team space from this folder** (`POST /v1/spaces/from-folder`). A folder of the personal folder becomes a new team space, with the account as its first owner. The files are copied, never moved, and the old folder goes only when the team space is known to hold every file:

1. *Checking* (in the call): the folder is inside the personal folder and is not it, is no link and holds no link or special file, no team folder, no `Craft Cloud Shared` folder, no other synced folder (a `.craft-cloud` folder) and no name the sync cannot carry; it is not empty. Files the sync never carries (`*.tmp`, lock files) are neither copied nor deleted.
2. *Creating* (in the call): the team space is made on the server (`POST /admin/spaces`).
3. *SettingUp*: its folder `Craft Cloud Shared/<name>` is set up and its sync turned on, and its passes may claim chunks from the personal space (`POST /chunks/claim`), so the bytes are not sent again. A server that has no such call, or refuses it, makes the files go up as usual.
4. *Copying*: each file is read once, chunked, and written into the team folder through a temporary file, so the team pass never sees half a file; a file that changes under the copy is read again. The team folder and every folder written into must stay real folders (no link of any kind, the same folder) or the copy stops; a file that is not there yet lands by a hard link that fails if the name was taken (a volume without hard links falls back to a rename to a name that was absent a moment before). Everything is written through the team folder's resolved path. The copy records what it wrote for each path (saved about once a second, so a kill loses little): a team file with other content than that and the old file was edited in the team space, so it stays, the old version goes beside it as a conflict copy (once for the same two versions, however often it is asked), and that old file is not deleted (counted as kept with the reason). A team file the copy wrote itself is replaced by a newer old version like this: it is looked at where it stands (file ID, size, times and, on Unix, the inode change time); a file that changed is read in place, and other content is a team edit that stays while the new version goes beside it as a conflict copy. A file that plainly is the one that was read is moved aside (with a note that names its path), the new copy lands at once by a link that fails on a taken name (the place is empty for two renames only, because the team pass fails on a file that is absent while it is read), and then the moved file is read before it is deleted: if it is not the content the copy wrote, it is kept beside the new copy as a conflict copy and the old file is not deleted. Team files moved aside by a copy that was interrupted are put back when the agent starts, before the team space's sync looks at the folder; any that cannot be are reported (`Failed` at the copy) and stop the next copy.
5. *Syncing* (a stall is ten minutes in which the team space stores no new byte and commits no new file, so one big upload is none): the team space's tree on the server is read until it holds every file with the manifest the copy was chunked into, and every folder. That, not the pass's own report, is "the team commit succeeded".
6. *Verifying*: the old folder is read once more and must still be exactly what was copied; if not, the work goes back to copying (up to three rounds).
7. *Trashing*: before the first file, and again after every batch (1,000 files or 60 s), the agent requires the team space to be listed, active, not archived, the account still able to change it, and the server's tree to hold every file still to be deleted; anything else stops with "nothing more was deleted". The old folder's files are then deleted here one by one: each one's folders are checked for links of any kind, the file is renamed to `<slot>/<its path in the old folder>`, the slot being a folder made new for it (so nothing can be replaced there and the file keeps its own name), in a quarantine inside the personal folder's `.craft-cloud/from-folder/`; what arrived must be a regular file with the identity of the one that was looked at (else it goes straight back), it is read there once more and compared with the verified manifest, and only a match is deleted; anything else is moved back without replacing, next to its place as `<name>.cc-kept` if the place was taken (on a volume without hard links by a rename to an absent name). Before every batch and every file the old folder and the folders above it must still be the ones that were scanned (identity and no link), and the team space is asked directly whether it is listed, active, not archived and writable (the full tree check is made first and every ten minutes). Each quarantine folder has a marker beside it (`<key>.root`, never inside, so a file of any name can be quarantined) that names the old folder; without a marker that can be written nothing is moved aside. Whatever a crash left in the quarantine is put back at every start and before every scan, whatever the journal says, if its marker is sound (absolute, inside the personal folder, the one the folder was named after; otherwise it is left alone and reported); what cannot be put back, or a quarantine that cannot be read, makes the work `Failed` with a message that names the quarantine folder. The personal sync sees an ordinary deletion of a folder: it goes to the server's trash, where it can be restored, and the mass-delete guard sees it like any other (a big folder waits for approval, and the server keeps it until then). What changed after the check stays, and is counted in `kept`.

The work belongs to the session: sign-out, another account or stopping the agent ends it and is waited for before the next session reads the journal. A failure before step 7 leaves the old folder as it was. The half-made team space is reported (`Failed` with `failed_at`, `message` and `space`), never deleted. A journal (`from-folder.json` in the data folder) records the step and the space; when the agent stops in the middle, the next start reports the work as `Interrupted`. Asking again for the same folder and name goes on with the same space and needs no confirmation. Only one such work runs at a time (`409 conflict`). The claims are kept in memory only: after a restart the files are uploaded, which is slower and as safe.

Accept and decline, the members, the usage, the rename, leaving and the administrator's team space calls are passed on to the server, which decides; its refusals keep their status and code (the administrator's password stays in the body, is kept by nobody and is left out of every `Debug` output). An agent from before
team spaces answers these calls with `not_found` ("no such call"), which `craft-cloud-app` reports as `agent_too_old`.
