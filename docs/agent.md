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
  itself once it is back. Ctrl-C lets a running pass finish and stops; a second Ctrl-C stops at once.
- While `run` runs, `sync --approve-deletes` from another terminal hands the approval to it: exactly the
  deletions `status` listed. If more came since, it lists them instead, approves nothing and exits 2; check
  the list and run it again. Every list of waiting deletions comes with its set (`set 3f9a…`), also in
  `run`'s output: `sync --approve-deletes=<set>` approves exactly that set if it is the one waiting, without
  `status` first, and otherwise shows the waiting set, approves nothing and exits 2. A plain `sync` says the
  agent is already running.
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
| `GET /v1/admin/server` | `GET /admin/server` | the server's counters and kinds; a member is refused `forbidden` |
| `GET /v1/admin/users?cursor&limit&query` | `GET /admin/users` | a page of accounts by username; `next` is the cursor of the following page; `query` finds a username containing it |
| `POST /v1/admin/users` | `POST /admin/users` | `{username, role, password, totp_code?, recovery_code?}` → the account and its invite code |
| `PATCH /v1/admin/users/{id}` | `PATCH /admin/users/{id}` | `{role?, quota_bytes?, password, totp_code?, recovery_code?}` → the account (`null` removes the quota) |
| `POST /v1/admin/users/{id}/disable`, `…/enable` | the same | `{password}` → 204 |
| `DELETE /v1/admin/users/{id}` | the same | `{password}` → 202 |
| `POST /v1/admin/users/{id}/invite`, `…/password-reset` | the same | `{password}` (a reset code: also `totp_code?`, `recovery_code?`, `reset_two_factor?`) → `{code, join_url?, expires_ms}` |
| `DELETE /v1/admin/users/{id}/invite` | the same | `{password}` → 204 |
| `GET /v1/admin/users/{id}/devices` | the same | every device of the account |
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
