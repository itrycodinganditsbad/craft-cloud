# Craft Cloud — Design (Phase 2: Collaboration)

> Sharing between accounts on one self-hosted server: team spaces, membership, co-editing live documents across
> accounts, shared libraries, read-only share links, review comments and optional locking. Depends on Phase 1
> ([`sync.md`](sync.md)) and the live documents ([`live.md`](live.md)). Status: proposal, for approval phase by phase.

This page is written so the maintainers can approve it one phase at a time. Phases P1 (team spaces) and P2 (co-editing
across accounts) are specified down to tables, routes, frames and pull requests, because they are meant to be built
next. Phases P3 to P6 are shorter: they fix the direction and the constraints, and each gets its own detailed section
here before it is built.

Two kinds of marks are used throughout:

- **Decided (recommended)**: a choice this spec makes, with the reason. A maintainer may overturn it; the rest of the
  spec says what would change.
- **Open question (Qn)**: a decision only the maintainers can take. All of them are collected in
  [Decisions needed](#decisions-needed-from-the-maintainers) at the end.

## Contents

1. [Goals and non-goals](#goals-and-non-goals)
2. [Terms and roles](#1-terms-and-roles)
3. [Data model](#2-data-model)
4. [Permissions](#3-permissions)
5. [Protocol](#4-protocol)
6. [Agent](#5-agent)
7. [Quota, chunks, GC, trash](#6-quota-chunks-gc-and-trash)
8. [Co-editing across accounts](#7-co-editing-across-accounts)
9. [Share links and web file access](#8-share-links-and-web-file-access)
10. [Libraries](#9-libraries)
11. [Comments and review](#10-comments-and-review)
12. [Locking](#11-locking)
13. [Security and privacy](#12-security-and-privacy)
14. [UI](#13-ui)
15. [Phases and pull requests](#14-phases-and-pull-requests)
16. [Decisions needed from the maintainers](#decisions-needed-from-the-maintainers)

## Goals and non-goals

Goals:

- A studio of 2 to 50 accounts on one server can keep project folders that several accounts sync, with the same
  guarantees as a personal folder: versioned, conflict copies instead of lost edits, a trash, no data lost by any
  sequence of offline edits, crashes or restores.
- Two people on two accounts can edit the same live document at once, see each other's cursors with the other
  person's account name, and keep every edit.
- Every account keeps its personal space exactly as it is today. A server that never creates a team space behaves
  exactly like a Phase 1 server, and old agents keep working against a new server.
- Every check happens on the server, per request, from the signed-in device and the database; nothing the client
  says about a space or a role is trusted.

Non-goals (for all of Phase 2 unless a later section brings them in):

- Federation between servers, or accounts from outside the server. Every member is an account on this server.
- End-to-end encryption of team spaces. Chunk IDs stay opaque and the format byte stays, so it remains possible.
- Fine-grained permissions inside a space (per-folder access lists). The unit of access is the space (see
  [1.4](#14-how-a-share-grants-access)).
- A web editor for files. The web gains reading files (P4) and nothing more.

## 1. Terms and roles

### 1.1 Spaces

A **space** is one versioned tree of nodes with its own epoch, sequence numbers, change feed, trash, storage count and
live documents, as Phase 1 defines it. Phase 2 adds a **kind**:

| Kind | Created by | Owner | Members | Notes |
|---|---|---|---|---|
| `personal` | the server, with the account | the account (`spaces.owner`) | none: only the owner has access | Every account has exactly one. Unchanged from Phase 1. |
| `team` | an administrator (P1; see Q1) | one or more member accounts with the `owner` role | accounts with a role in `space_members` | Has a display name, unique on the server (case folded). Never tied to one account's life. |

A device still signs in as one account. Its account's personal space stays the **default space**: every request that
names no space works on it, exactly as today.

### 1.2 Roles in a space

| Role | Read files, history, trash | Write files (commit, restore, restore from trash) | Live documents | Manage members | Remove from trash, archive, rename |
|---|---|---|---|---|---|
| `viewer` | yes | no | open read-only, send presence | no | no |
| `editor` | yes | yes | open, edit, import | no | no |
| `owner` | yes | yes | open, edit, import | yes | yes |

- A personal space's owner has the `owner` role in it implicitly and no row in `space_members`.
- A team space has at least one active `owner` at all times, apart from the case in [1.3](#13-server-roles-and-space-roles)
  where every owner's account was disabled or removed; an administrator then names a new owner.
- Roles are a closed list in the code (`craft_cloud_api::SpaceRole`: `Viewer`, `Editor`, `Owner`, ordered so that
  `role >= Editor` reads as "may write").

### 1.3 Server roles and space roles

The server roles `admin` and `member` (migration 0008) stay as they are and mean the same thing: who may run the
server. They are independent of space roles.

- **Decided (recommended): an administrator has no implicit access to a team space's content.** An administrator
  creates team spaces, names and replaces owners, sets quotas, archives and lists spaces (name, size, member count),
  but reads files only if they are a member. Reason: in a studio the person who runs the NAS is not always allowed to
  see every client project, and an implicit read right would be invisible to the members. An administrator who
  needs access adds themselves as a member, which is audited and visible in the member list. (The administrator of a
  self-hosted server can read the disk anyway; this rule is about what the product shows and records, not a
  cryptographic boundary. See Q7.)
- A disabled or removed account (migration 0008's `disabled_ms`, `removed_ms`) has no access to any space; its member
  rows stay so that re-enabling restores them.

### 1.4 How a share grants access

- **Decided (recommended): a share grants a role on a whole space, never on a folder inside a space.** Reasons:
  - Every Phase 1 invariant (atomic batch commits, moves of whole folders, the change feed as one indexed query, the
    trash's folder rules, conflict handling) works per space. A folder-level access list would need a check on
    every node of every operation, including both ends of a move and every ancestor walk, and would turn "move a
    folder out of the shared part" into a delete for some members and a move for others.
  - The agent maps one space to one local folder; a folder-level share would need partial trees with holes.
- "Share this folder" in the app is offered as **"Make a team space from this folder"**: a new team space is
  created and the folder's content moves into it (P1 follow-up PR P1-8, see
  [6.2](#62-dedupe-across-spaces)). Whether this flow is wanted at all is Q2.
- Membership needs consent: adding an account to a team space creates an **invitation** that the account accepts or
  declines. **Decided (recommended)**, because joining a space can put gigabytes on someone's disk and into their
  view, and an unwanted space must never appear without the person agreeing. Q8 asks whether admin-made memberships
  should skip the acceptance step.

## 2. Data model

### 2.1 Changes to existing tables

`spaces` (migration 0001, extended by 0002, 0005 and 0008) gains a kind, a name and life-cycle fields, and its owner
becomes optional (set exactly for personal spaces).

| Column | Type (SQLite / Postgres) | Rule |
|---|---|---|
| `id`, `epoch`, `seq`, `oldest_seq`, `root`, `stored_bytes`, `live_bytes`, `quota_bytes` | as today | unchanged |
| `owner` | `BLOB` / `BYTEA`, now **nullable**, `REFERENCES users (id)` | set if and only if `kind = 'personal'` |
| `kind` | `TEXT NOT NULL DEFAULT 'personal'`, `CHECK (kind IN ('personal', 'team'))` | new |
| `name` | `TEXT` | team display name, NFC, 1 to 64 characters, no control characters; `NULL` for personal spaces |
| `name_fold` | `TEXT` | Unicode full case fold + NFC of `name`, as for node names; `NULL` for personal spaces |
| `created_ms` | `BIGINT NOT NULL DEFAULT 0` | new; set from `users.created_ms` for existing personal spaces |
| `archived_ms` | `BIGINT` | new; `NULL` while active |

Constraints and indexes:

- `CHECK ((kind = 'personal') = (owner IS NOT NULL))` and `CHECK ((kind = 'team') = (name IS NOT NULL AND name_fold IS NOT NULL))`.
- The column-level `UNIQUE` on `owner` is replaced by `CREATE UNIQUE INDEX spaces_personal_owner ON spaces (owner) WHERE owner IS NOT NULL`.
- `CREATE UNIQUE INDEX spaces_team_name ON spaces (name_fold) WHERE kind = 'team'`.

Every other space-keyed table (`nodes`, `versions`, `space_blobs`, `live_docs`, `live_updates`, `live_spaces`) is
unchanged: they are keyed by `space_id` already, and a team space is just another value of it.

### 2.2 New tables (P1)

**`space_members`**: who has which role in a team space.

| Column | Type (SQLite / Postgres) | Rule |
|---|---|---|
| `space_id` | `BLOB` / `BYTEA NOT NULL REFERENCES spaces (id)` | a team space |
| `user_id` | `BLOB` / `BYTEA NOT NULL REFERENCES users (id)` | the member |
| `role` | `TEXT NOT NULL CHECK (role IN ('viewer', 'editor', 'owner'))` | |
| `invited_ms` | `BIGINT NOT NULL` | when the invitation was made |
| `invited_by` | `BLOB` / `BYTEA` | the acting account; `NULL` for the command line. No foreign key, as in `audit_events` |
| `accepted_ms` | `BIGINT` | `NULL` while the invitation is pending; access needs it set |

- `PRIMARY KEY (space_id, user_id)`.
- `CREATE INDEX space_members_user ON space_members (user_id, space_id)` for "my spaces".
- A declined invitation, a member who leaves and a removed member are all deleted rows (the audit log keeps the
  history). A pending invitation has no expiry: it costs nothing and the owner sees and can withdraw it.

Phase 2's later tables (`share_links` in P4, `file_locks` in P6) are sketched in their sections and get their own
migrations then.

### 2.3 Migration order

| # | Name | Content | Phase |
|---|---|---|---|
| 0015 | `team_spaces` | the `spaces` changes above, `space_members` | P1 |
| 0016 | `share_links` | P4 | P4 |
| 0017 | `file_locks` | P6 | P6 |

Numbers after 0014 are only reserved by order: whatever lands on `main` first takes the next number.

**Postgres** (0015) is a plain sequence of `ALTER TABLE` statements: add the columns, drop `NOT NULL` on `owner`,
drop the unique constraint on `owner` (Postgres names it `spaces_owner_key`; the PR checks the name against a real
database in CI rather than assuming it), add the checks and the two partial indexes, set `created_ms` from the owner's
`users.created_ms`, create `space_members`.

**SQLite** (0015) cannot drop a column constraint, so `spaces` is rebuilt with SQLite's documented procedure: create
`spaces_new` with the new definition, copy every row, drop `spaces`, rename `spaces_new` to `spaces`, recreate its
indexes. Because other tables reference `spaces (id)`, the rebuild must run with foreign-key enforcement off.

- **Decided (recommended):** the migration runner in `crates/meta/src/schema.rs` gains one marker for SQLite
  migrations, a first line `-- craft: foreign-keys-off`. For such a migration the runner sets
  `PRAGMA foreign_keys = OFF` on its connection before `BEGIN` (the pragma has no effect inside a transaction), runs
  the statements, runs `PRAGMA foreign_key_check` inside the same transaction and aborts it if any row comes back,
  commits, and sets `PRAGMA foreign_keys = ON` again, also on failure. Reason: it is the only way to change the
  constraint without a hidden "team user" row in `users`, which every account list, sign-in and invite path would
  then have to filter out. The cost is one extra branch in a small runner, tested on its own.
- Tests for the migration (both backends): a database at 0014 filled with users, devices, nodes, versions, blobs,
  live documents and trash is migrated, and a fingerprint of every other table is equal before and after; every
  space is `personal` with its owner; `foreign_key_check` is empty; the new checks refuse a personal space without
  an owner, a team space with one, and a second personal space for an account.

### 2.4 How existing personal spaces carry over

Nothing a Phase 1 server stores changes meaning:

- Every existing space becomes `kind = 'personal'` with the same `owner`, `id`, `epoch`, `seq` and counters.
- `DeviceAuth.space` (the space resolved with the device token) stays the account's personal space. The token check's
  join `spaces s ON s.owner = u.id` keeps working because only personal spaces have an owner.
- No row goes into `space_members` for personal spaces.
- Agents, apps and the change feed see no difference until a team space exists and they ask for it.

## 3. Permissions

### 3.1 Where the check happens

One function decides access: `Meta::space_access(user, space) -> Option<SpaceRole>`, one indexed query:

- personal space: `Owner` if `spaces.owner = user`, else none;
- team space: the member row's role if `accepted_ms IS NOT NULL`, the space is not archived (an archived space gives
  at most `Viewer`), and the account is neither disabled nor removed; else none.

The server resolves it in a new layer, `require_space`, that runs after `require_device` on every space-scoped route
and puts a `SpaceAccess {space, role, personal: bool}` into the request. Handlers take the space from `SpaceAccess`,
never from `DeviceAuth.space` again (the PR that adds the layer replaces every use, and a test greps the handlers for
`auth.space`). Each handler declares its minimum role; the layer refuses before the handler runs. Write handlers also
re-check the role **inside their write transaction**, in the same statement that locks the space row, so a role
change that commits between the layer and the write is respected (the same pattern as the admin routes' "acting admin
in the transaction").

### 3.2 Route by role

`404 not_found` means "no access", and is the same answer as for a space that does not exist, so a non-member learns
nothing. `403 read_only` (a new error code) means "you are a member but your role does not allow this", so the agent
can switch the space to read-only without guessing. Neither is ever `401`: the agent reads every 401 on a signed-in
route as "this device was signed out".

| Route | none | viewer | editor | owner |
|---|---|---|---|---|
| `GET /space` | 404 | yes | yes | yes |
| `GET /tree`, `GET /changes` | 404 | yes | yes | yes |
| `GET /chunks/{id}`, `GET /manifests/{id}` | 404 | yes (blobs of this space only) | yes | yes |
| `POST /chunks/missing`, `PUT /chunks/{id}`, `PUT /manifests/{id}` | 404 | 403 `read_only` | yes | yes |
| `POST /commit`, `POST /saves` | 404 | 403 `read_only` | yes | yes |
| `GET /nodes/{id}/history` | 404 | yes | yes | yes |
| `POST /nodes/{id}/restore` | 404 | 403 `read_only` | yes | yes |
| `GET /trash` | 404 | yes | yes | yes |
| `POST /trash/{id}/restore` | 404 | 403 `read_only` | yes | yes |
| `DELETE /trash/{id}` (remove from trash) | 404 | 403 `read_only` | 403 `read_only` | yes |
| `GET /spaces/{id}/members` | 404 | yes | yes | yes |
| `POST/PATCH/DELETE /spaces/{id}/members…` | 404 | 403 (own row: leave only) | 403 (own row: leave only) | yes |
| `GET /spaces/{id}/usage` | 404 | yes | yes | yes |
| live `subscribe`, `subscribe_log`, `presence`, `unsubscribe` | connection refused 404 | yes | yes | yes |
| live `create` (recents, document import), `update`, `snapshot` | connection refused 404 | `error read_only` | yes | yes |

- **Decided (recommended): removing a node from the trash is an owner's right only**, in team spaces. Reason: once
  retention exists, removal from the trash is the step that can make data unrecoverable; a careless or malicious
  editor can delete files (which go to the trash) but cannot make them disappear. In a personal space the owner is
  the only member, so nothing changes there.
- History and trash are readable by every member, including viewers and members who joined later. The content of a
  team space belongs to the space, not to whoever uploaded it.
- Account routes (`/account…`, `/devices…`, `/pairings…`) and admin routes stay account-scoped and ignore any space
  header.

### 3.3 What a viewer can and cannot do

A viewer can list, download, read history and trash, open live documents read-only, send presence (so editors see
who is watching) and see the member list. A viewer cannot upload, commit, restore, import or edit a live document,
change members, or remove anything from the trash. The agent syncs a viewer's space download-only
([5.4](#54-read-only-spaces)).

### 3.4 Revocation

Removing a member, lowering a role, archiving a space, and disabling or removing an account all end access at once:

- **HTTP**: the next request is refused (the layer reads the database on every request; nothing is cached).
- **Change feed**: a membership change wakes the space's long-poll waiters (the same notifier commits use); each
  waiter re-checks access after waking and answers 404 or `read_only` instead of the changes. A waiter in another
  server process notices at the end of its wait (at most 60 s), like a revoked device today.
- **Live**: the server keeps, per process, a registry of live connections by `(space, user)`. A membership change or
  role change in the process closes that user's connections to that space at once (close code with reason
  `access_changed`); the agent reconnects and gets the new role or a refusal. Another process (two servers on one
  Postgres) and the command line are noticed at the next ping (about 30 s): the ping handler re-reads access, as it
  re-reads the epoch today.
- **Chunks already downloaded** cannot be taken back. The removed member's local copy stays on their disk, and the
  agent never deletes it ([5.6](#56-losing-access)). The member list makes this visible: removing someone stops
  future changes reaching them, it does not erase what they already had.

### 3.5 Audit events

New names in the closed list of `audit_events.action`, with `target_kind = 'space'` (a new value) and the space ID as
`target_id`. `details` holds typed values only: `user` (an account ID), `role_from`, `role_to` (role words),
`quota_bytes` (a number or `null`). **No space name, file name or path is ever stored in a row**, consistent with the
existing rule.

| Event | Written by |
|---|---|
| `space.created` | an administrator creating a team space (details: the first owner) |
| `space.renamed` | an owner or administrator renaming it (no name in the row) |
| `space.archived`, `space.unarchived` | an owner or administrator |
| `space.quota_changed` | an administrator |
| `space.member_invited` | an owner or administrator (details: `user`, `role_to`) |
| `space.member_joined`, `space.member_declined` | the invited account |
| `space.member_role_changed` | an owner or administrator (details: `user`, `role_from`, `role_to`) |
| `space.member_removed` | an owner or administrator (details: `user`) |
| `space.member_left` | the member |
| `cli.space_created`, `cli.space_member_changed` | the command line, with no actor |

Each event is written in the transaction of its change, a repeat or a no-op writes nothing, and the account's own
activity (`GET /account/activity`) shows events where it is the actor or the `user` in `details`. Files, uploads and
live documents remain unaudited, as today.

## 4. Protocol

### 4.1 Naming the space on a request

- **Decided (recommended): a request names its space with the header `craft-space: <space UUID>`**, on every
  space-scoped HTTP route and on the live WebSocket upgrade. No header means the device's personal space, exactly as
  today. Reasons: no route changes shape, so every Phase 1 route, test and agent keeps working unchanged; the space
  is chosen once per request in one layer; and the same header works for the WebSocket upgrade, where a path
  parameter would need a second router entry.
- Every answer of a space-scoped route **echoes** `craft-space` with the space it worked on (also when the header was
  absent: the personal space's ID). A new agent refuses any answer for a team space whose echo is missing or
  different and stops syncing that space with "the server is too old for team spaces". This closes the hole where an
  older server, ignoring the unknown header, would answer about the personal space and the agent would mix two trees.
- A malformed header is `400 invalid`. An unknown space or one without access is `404 not_found`.

### 4.2 New HTTP routes (P1)

All under `/api/v1`, JSON, behind `require_device`. Bodies are read up to 4 KiB (the tiny-body limit) except where
noted.

| Call | Who | Purpose |
|---|---|---|
| `GET /spaces` | any account | `{spaces: [{id, kind, name?, role, state: active\|invited, archived, created_ms, members, root, epoch}]}`: the personal space first, then team spaces by name; invitations included with `state: invited` (no `root` or `epoch` until accepted) |
| `GET /space` (with `craft-space`) | member | as today, plus `kind`, `name?`, `role` (appended fields; old agents ignore them) |
| `GET /spaces/{id}/members` | member | `{members: [{user, username, role, state, colour}]}`; `colour` is the palette index of [7.1](#71-presence-identity) |
| `POST /spaces/{id}/members` | owner, admin | `{username, role}` → the member row (`state: invited`). Unknown, disabled or removed account: `404 not_found` (one answer for all three). Already a member or invited: `409 conflict`. 30 per account per minute, then `429 rate_limited` |
| `PATCH /spaces/{id}/members/{user}` | owner, admin | `{role}` → the row; lowering the last owner is `409 last_owner` (new code) |
| `DELETE /spaces/{id}/members/{user}` | owner, admin; the member for their own row | removes the member or withdraws the invitation (204); the last owner leaving is `409 last_owner` |
| `POST /spaces/{id}/accept`, `POST /spaces/{id}/decline` | the invited account | accept → the space as in `GET /spaces`; decline → 204. No invitation: `404 not_found` |
| `GET /spaces/{id}/usage` | member | `{stored_bytes, quota_bytes, trash_bytes, history_bytes, live_bytes, estimates}` as `/account/usage`, for the space |
| `PATCH /spaces/{id}` | owner (name), admin (name, quota) | `{name?, quota_bytes?, password?}`: renaming needs no password; a quota change is an admin change and carries the admin confirmation (password, and the second factor when on) as every admin change does |
| `POST /admin/spaces` | admin, confirmation | `{name, owner: user id, quota_bytes?, password, totp_code?}` → the space. The owner is added as an **accepted** owner (an administrator naming the first owner is the consent step for that one row; see Q8). `409 conflict` for a taken name |
| `GET /admin/spaces?cursor&limit&query` | admin | `{spaces: [{id, name, created_ms, archived_ms, members, owners, stored_bytes, quota_bytes}], next?}` |
| `POST /admin/spaces/{id}/archive`, `/unarchive`; `POST /spaces/{id}/archive`, `/unarchive` | admin with confirmation; an owner with their password on the `/spaces` form | archive: every member becomes read-only, the space leaves `GET /spaces` lists except for owners; nothing is deleted. Unarchive reverses it |
| `PUT /admin/spaces/{id}/owner` | admin, confirmation | `{user}`: makes an account an accepted owner (used when every owner is gone) |

The command line gains `space create <name> --owner <user>`, `space list`, `space members <space>`,
`space add <space> <user> --role <role>` (added accepted: whoever runs the command line has the files), `space archive`
and `reset-epoch --space <id>`.

Error codes added to `craft_cloud_api::codes`: `read_only` (403), `last_owner` (409). Every other answer reuses
existing codes.

### 4.3 Compatibility and protocol versions

| Party | Old | New |
|---|---|---|
| HTTP protocol (`PROTOCOL_MAX` in `crates/proto/src/api.rs`) | 1 | **2**: the `craft-space` header and echo, the routes above. `PROTOCOL_MIN` stays 1 |
| Live protocol (`LIVE_PROTOCOL_MAX` in `crates/proto/src/live.rs`) | 2 | **3** (P2): the space on the upgrade, `welcome_space`, `presence_from`, `read_only` refusals |

- **Old agent, new server**: sends no header, speaks protocol 1 and live protocol 2; it gets its personal space and
  nothing else, byte for byte as today. It never learns that team spaces exist.
- **New agent, old server**: `/api/info` says protocol 1; the agent syncs the personal space as today and reports
  team spaces as unavailable ("the server needs an update for team spaces"). It never sends `craft-space` to a
  protocol 1 server, and the echo check covers the case where it would.
- **New agent, new server, live protocol 2 only** (P1 shipped, P2 not yet): the agent opens no live connection for a
  team space; documents of files in team spaces answer `not_live {reason: offline}` and the app works on the file as
  an ordinary file. Personal live documents are unchanged.
- Postcard rules: variants are only appended, above the marker line both enums carry today; no existing variant
  changes; golden tests pin the bytes of every Phase 1 frame so that a protocol 2 connection is byte-identical after
  P2.
- JSON rules: new fields are appended and optional; unknown fields are ignored both ways, as today.

### 4.4 Live frames (P2)

The space of a live connection is fixed at the upgrade (`craft-space` header). One connection carries one space.

- **Decided (recommended): one WebSocket per (device, space).** Reasons: the server's live module is already scoped
  by `SpaceId` (fan-out, presence channels, `live_spaces` generation, caps), so a connection per space reuses all of
  it; the welcome's epoch is a space's epoch; and revocation is "close that connection". The limit of 4 connections
  per device becomes 4 per device and space and 32 per device in total.
- New frames, appended:

| Frame | Direction | Content | Sent when |
|---|---|---|---|
| `WelcomeSpace {protocol, epoch, space, role}` | server → agent | as `Welcome`, plus the space and the caller's role | instead of `Welcome` when the negotiated protocol is 3 |
| `PresenceFrom {doc, bytes, user}` | server → agent | as `Presence`, plus the sending account's ID, stamped by the server | instead of `Presence` on protocol 3 |
| `Error {code: "read_only", …}` | server → agent | existing frame, new code | a viewer's `create`, `update` or `snapshot` |

- On protocol 3 the server never asks a viewer's device to `compact` (it could not upload the snapshot).
- On protocol 2 (old agents) a connection without the header is the personal space and gets exactly the old frames.

### 4.5 Local API additions (agent ↔ apps)

All under the agent's existing loopback rules. New calls answer the local API's `not_found` on an old agent, which
`craft-cloud-app` reports as `agent_too_old`, as it does for blobs.

| Call | Purpose | Phase |
|---|---|---|
| `GET /v1/spaces` | `{spaces: [{space, kind, name?, role, state, folder?, sync: on\|off\|unavailable, run_state?, last_sync_ms?}]}` | P1 |
| `POST /v1/spaces/{id}/sync` | `{on, folder?}`: turn syncing of a team space on or off on this computer, optionally choosing the folder | P1 |
| `POST /v1/spaces/{id}/accept`, `/decline`, `/leave` | pass through to the server (leave also turns syncing off and keeps the files) | P1 |
| `GET`, `POST`, `PATCH`, `DELETE /v1/spaces/{id}/members…`, `GET /v1/spaces/{id}/usage`, `GET`/`POST /v1/admin/spaces…` | passed through to the server for the Craft Cloud app's pages | P1 |
| `GET /v1/status` | appended `spaces: [{space, run_state, last_sync_ms?}]`; the top-level fields keep describing the personal space | P1 |
| `GET /v1/recents` (WebSocket) | `subscribe` gains an optional `space` (appended); without it, the personal list as today. Entries added with a path inside a team folder go to that space's recents | P2 |
| `GET /v1/docs` (WebSocket) | `open` gains optional `identity: true`; `opened` gains `read_only` and `space`; new binary kind 5 and text message `members` ([7.3](#73-agent-and-local-api)) | P2 |
| `PUT /v1/blobs`, `GET /v1/blobs/{hash}` | optional query `for=<path of the document's file>`: the blob goes to (or is read from) that file's space; without it, the personal space as today | P2 |

## 5. Agent

### 5.1 One folder per space

- The personal space keeps its folder (default `~/Craft Cloud/`) and its state files exactly where they are. An
  existing installation needs no migration.
- **Decided (recommended): team spaces sync to folders beside the personal one, by default
  `~/Craft Cloud Shared/<team space name>/`**, one folder per space, each with its own root marker
  (`.craft-cloud/root.json {space, root}`, which already names the space). Reasons:
  - synced roots never nest, so the personal engine needs no exclusion rule, and a Phase 1 user who already has a
    folder named `Shared` in the personal space keeps it;
  - each root has its own mass-delete guard, marker check and state, so a problem in one team space never pauses
    another or the personal space;
  - a folder per space keeps the mapping obvious when looking at the disk.
  The nested layout `~/Craft Cloud/Shared/<name>/` the brief suggests is possible too, at the cost of a reserved
  name in the personal root and an exclusion in the personal engine (Q5).
- The folder name is the space's name passed through the node name rules; a clash with an existing folder takes
  ` (2)`, ` (3)` and so on. A space renamed on the server keeps its local folder name (renaming a root under running
  apps would break their open paths); the app shows the new name and offers to rename the folder.
- The user may pick another folder per space (`POST /v1/spaces/{id}/sync {folder}`), with the same rules as the
  personal folder: never inside another synced root or the agent's data folder, empty or adopted with consent.

### 5.2 State

`account.json` keeps its fields and gains `spaces: [{space, folder, sync}]` (appended; an older agent ignores it).
Per team space, the agent's data folder holds `spaces/<space id>/state.json` and `spaces/<space id>/status.json`,
written atomically like the personal ones. Live data already lives under `live/<space>/` and needs no change. The
personal space's files stay at the top level.

### 5.3 Turning a space on, scheduling

- **Decided (recommended): an accepted team space starts with syncing off on every computer**, and the Craft Cloud
  app asks "Sync on this computer?" with its size. Reason: an administrator may add someone to a space of several
  terabytes, and nothing should fill a laptop without the person choosing it. Selective sync then applies as for
  the personal space (folders over the 5 GB threshold start cloud-only).
- One sync engine instance per synced space, each with its own `State`, root, marker and guards. The engine crate is
  unchanged in structure: the `Transport` it receives is bound to one space (it sets the header and checks the echo).
- The run loop keeps one scheduler. **Decided (recommended): passes run one at a time across spaces**, round-robin by
  what is waiting. Reasons: the agent speaks HTTP/1.1, the server's per-account body slots (8) are shared anyway,
  and the simulations stay deterministic per seed. Each synced space keeps its own long-poll (`GET /changes` with its
  header), so a change in any space wakes the scheduler; the server's 16 waiters per space are unaffected.
- `pause` and `resume` apply to every space. Status per space is in `GET /v1/status`'s `spaces`.

### 5.4 Read-only spaces

For a space where the role is `viewer` (from `GET /spaces`, or learnt from a `403 read_only`), the engine runs in a
read-only mode:

- Remote changes are applied as usual.
- A local edit is never uploaded and never overwritten. When the server's version of an edited file changes, the
  server's version lands under the original name and the local edit is kept under a conflict name (the existing
  conflict naming), marked local-only in the conflict list with "this space is read-only for you".
- A new local file stays local and is listed once as "not uploaded: read-only space".
- A local delete is undone by downloading the file again on the next pass, with a notice. (The alternative, keeping
  the deletion local, would make the folder silently differ from the space; undoing is safe and visible.)
- A role raised to `editor` switches the engine back; local-only files are then uploaded as ordinary new files on the
  next pass (conflict copies keep their names).
- Files are not given the operating system's read-only attribute: on Windows that makes apps fail to save in
  confusing ways, and the agent never blocks an app's save.

### 5.5 Conflicts across accounts

Nothing new: the engine's conflict rules are per space and already handle two devices editing one node. A conflict
copy's name carries the device name (`name (conflict, <device>, <date>).ext`); in a team space that is another
person's device name, which members see. Device names are chosen by their owners and already appear in their own
device list; the Craft Cloud app says so when naming a device. Save records ([`live.md`](live.md), "Save records")
work across accounts unchanged, so two people saving the same live document produce no conflict copy.

### 5.6 Losing access

When a space answers `404 not_found` (member removed, space archived for non-owners, invitation withdrawn):

- the agent stops that space's engine and live connection, keeps every local file and its state, and marks the space
  `run_state: no_access` with "You no longer have access to <name>. Your local copy is kept and no longer syncs.";
- local edits not yet uploaded stay on disk; live document journals with unacked updates stay in `live/<space>/`;
- the app offers "Remove local copy" (an explicit user action, which moves the folder to the operating system's
  trash where available) and "Keep as a plain folder" (removes the marker and the state);
- if access comes back (re-added), the agent reconciles the folder like an epoch change (never replacing or deleting
  a local file during the reconcile).

### 5.7 Live hub and recents per space

- The hub keeps one link (WebSocket) per synced team space with live protocol 3, plus the personal link.
- A document is routed by the path the app opens: the agent finds the synced root that contains the path (absolute
  paths are needed for team files; a relative path is relative to the personal folder, as today).
- Recents: each team space has its own recents documents (`recents_doc(space, app)` already derives the ID from the
  space). An app's `recents()` call keeps returning the personal list; the new `subscribe {space}` returns a team
  space's list. Whether team recents should be shared with all members is a privacy choice (Q4); the default
  proposed is on, since members can see the files anyway and the list shows which files were worked on recently,
  not who opened them.

## 6. Quota, chunks, GC and trash

### 6.1 Who is charged

- **Decided (recommended): a team space's bytes are charged to the team space, never to its members.** `stored_bytes`
  and `live_bytes` are per space already. A team space's limit is its `quota_bytes`, else the new optional config
  `[spaces] team_stored_bytes`, else none. The per-account cap `stored_bytes_per_user` applies to personal spaces
  only. Reasons: a member who leaves must not take the space's bytes with them or be charged after leaving, and an
  owner must not be blocked from their own work because a team space is large. Q3 asks for the default.
- Over its limit, uploads to the team space are refused with `507 quota_exceeded` and `quota: {used, limit}` as
  today; the agent shows it per space; other spaces keep syncing.
- `GET /admin/storage` lists team spaces beside accounts (top ten by stored bytes, kinds marked).

### 6.2 Dedupe across spaces

- Chunks and manifests are stored once in the store, as today. `space_blobs` records which spaces have which blob,
  `chunks/missing` answers per space, and a space can only download blobs it has: none of this changes, so dedupe
  still reveals nothing across spaces.
- A file copied from the personal folder into a team folder is uploaded to the team space again (its chunks travel
  once more). **P1-8 (follow-up, optional)** adds `POST /chunks/claim {from_space, ids}`: the server adds to the
  target space the blobs that `from_space` has, when the caller is an editor of the target and has at least read
  access to the source. It reveals nothing the caller could not download anyway. The "make a team space from this
  folder" flow uses it.

### 6.3 What "referenced" means for GC

A chunk or manifest is referenced when **any** space references it: a version of any node (current, older, saved or
trashed) in any space, a live document's snapshot or import snapshot in any space, or (until assets are rooted) any
manifest no version or snapshot names, as `verify` and `gc --dry-run` already treat them. Team spaces add no new kind
of root; GC simply must not filter by space when it marks. The generation rule (an upload or commit touches what it
references) covers uploads to every space. The GC PRs must include a test with one chunk referenced from a personal
and a team space where the team reference alone keeps it.

### 6.4 Trash and restore rights

- Deleting files: editors and owners (a commit).
- Restoring from the trash and restoring old versions: editors and owners.
- Removing from the trash: owners only in team spaces ([3.2](#32-route-by-role)).
- The trash list does not name who deleted a node: the deleting device is in the version history as for any change,
  and no account name is added to the trash rows.

### 6.5 Retention

Retention by thinning (M4) applies per space with the server's windows. An optional per-space override
(`spaces.retention` JSON with the three windows) is left for later; studios with client contracts may need longer
history than personal spaces (noted with Q3).

## 7. Co-editing across accounts

### 7.1 Presence identity

- The server stamps every relayed presence message with the sender's account ID (`PresenceFrom.user`). An app cannot
  claim to be someone else: the bytes are the app's, the identity is the server's.
- What a member learns about another member: the account's **username** and a **colour index**. Nothing else is
  added (no e-mail, no real name, no device name in presence). The username is already visible to members in the
  member list.
- **Decided (recommended): the colour is derived, not stored.** `member_colour(user) = BLAKE3("craft-cloud member
  colour v1" ‖ user ID) mod 12`, an index into a fixed palette of 12 colours defined once in `craft-cloud-api` (with
  light and dark values). Every app and machine computes the same colour with no new table and no setting. Two
  members may share a colour in a big space; the name disambiguates.

### 7.2 The live service's space scoping

The server's live module (`apps/craft-cloud-server/src/live.rs`) is per space already; P2 changes where the space
comes from:

- On upgrade: `require_space` resolves the header (absent: personal), refuses a non-member with `404` before the
  upgrade, and puts the role into the connection.
- Per frame: `create`, `update` and `snapshot` require `editor`; `subscribe`, `subscribe_log`, `presence` and
  `unsubscribe` require `viewer`. The role is re-read at every ping and on the in-process revocation signal.
- Documents of files: `node_doc(space, node)` is unchanged; a document's space is the connection's space.
- Admin command `live reset-doc` gains `--space <id>` for team spaces.

### 7.3 Agent and local API

- The agent forwards `PresenceFrom` to apps that opened the document with `identity: true` as binary kind 5:
  `[u8 5][u64 id, big-endian][16 bytes account ID][the app's bytes]`. Apps that did not ask get kind 3 exactly as
  today. Presence from the same account's own other machines carries that account's ID too.
- On `opened` and whenever it changes, the agent sends `members {id, members: [{user, name, colour, self}]}` (text),
  built from `GET /spaces/{id}/members`, cached and refreshed when a `PresenceFrom` names an unknown account. In a
  personal space the list is the account alone.
- `opened` gains `read_only: true` for a viewer. Updates an app sends to a read-only document are refused with
  `error {code: read_only}`, and the agent's copy is unchanged.
- `PUT /v1/blobs?for=<path>` puts the blob in the file's space. Without `for`, the personal space, as today.

### 7.4 The kit's API impact

Goal: an app needs no change to co-edit across accounts, and at most one line to show who is there and to keep
binary values in team documents.

| What | Change for apps |
|---|---|
| Opening, editing, undo, saving a document in a team folder | **none**: the app passes the file's path as it does today; the agent routes by path |
| A viewer opening a document | **none**: the kit maps `read_only` to the read-only open it already has for documents of another app or a newer schema |
| Showing who is there | optional: new `session.people() -> Vec<Person {name, colour, peers}>` (additive), grouping `session.peers()` by account. `craft-cloud-egui`'s badge shows the people without app code |
| Binary values (`{$chunk, $len}`) in team documents | **one line**: build the blob store with the new `session.blobs()` (an `AgentBlobs` bound to the session's path) instead of `AgentBlobs(Blobs::new(..))`. An app that keeps the old constructor stores team blobs in the personal space, where other members get `blob_not_found`; the kit logs a warning once per session when a team session's blob store is not bound to it |
| Presence bytes | **none**: the kit asks for `identity: true` itself and keeps handing apps their own bytes |

`Peer` keeps its fields (adding a public field would break apps that construct it in tests); identity lives in the
new `Person` type.

### 7.5 Limits per space

The live service's caps move from "per account" to "per space", with the same numbers, for team spaces:

| Cap | Today (per account) | Team space |
|---|---|---|
| Live bytes | `live_bytes_per_user`, 1 GiB | `[spaces] team_live_bytes`, default 4 GiB (Q9) |
| Documents of files | 65,536, of which 10,000 hold updates | same, per space |
| Recents documents holding updates | 64 | same, per space |
| Log above the snapshot per document | 16 MiB | same |
| Presence | 4 KiB, 20 per second per document per device | same |
| Subscribed documents | 256 per device | 256 per device across all its spaces (the agent's memory bound) |
| Connections | 4 per device | 4 per device and space, 32 per device |

## 8. Share links and web file access

Phase P4. Large, and the first time files are served to a browser; it can be left out entirely (Q11).

- **What it is**: a read-only link to one file or one folder of a team space (personal spaces too, if Q2 allows),
  with an expiry (at most 30 days, default 7), optionally a download limit, revocable at any time.
- **Table `share_links`**: `id`, `space_id`, `node_id`, `token_hash` (BLAKE3 of 32 random bytes; the token is shown
  once), `created_by`, `created_ms`, `expires_ms`, `revoked_ms`, `max_downloads`, `downloads`. Index on
  `token_hash` (unique) and `(space_id, node_id)`.
- **Who may create**: editors and owners. Viewers may not (a link lets content leave the member list). Owners see and
  revoke every link of the space; creators see theirs.
- **Server additions**: `GET /s/{token}` (outside `/api`): a server-rendered page in the style of the admin pages (no
  script), listing a shared folder's direct children only and offering downloads; `GET /s/{token}/file/{node}`
  streams a file assembled from its chunks, every chunk checked. Only nodes under the shared node, by an ancestor
  check in one bounded query. No search, no listing of anything above the shared node, no history, no trash.
- **Security**: tokens hashed, constant-time comparison of the hash lookup, expiry checked on every request, revoked
  links and links of archived spaces or removed nodes answer one `404` page whatever the reason, 30 requests per
  minute per address and per link, the existing body and header limits, `Referrer-Policy: no-referrer`,
  `X-Robots-Tag: noindex`, and the link token never logged. Downloads are not audited per file; creating and
  revoking a link are (`link.created`, `link.revoked`).
- **Member sign-in on the web**: browser sessions (migration 0011) are opened to members, not only administrators,
  with the same cookie, CSRF and two-factor rules, to read the spaces they belong to (browse, download, history).
  This is the larger half of P4 and could be its own phase.

## 9. Libraries

Phase P3. A **library** is a team space (or a folder of one) that holds shared assets, read by apps through the kit.

- **Layout**: `Libraries/<kind>/` at the root of a space, kinds `swatches`, `fonts`, `presets`, `symbols`, each with a
  manifest `library.json`: `{format: 1, kind, name, items: [{id, name, file, tags?, licence?}]}`. Items are plain
  files beside it (`.json` swatch sets, font files, preset files, symbol files in the app's own format). The manifest
  is a plain synced file: versioned, conflict-copied and restorable like any other.
- **Reading**: `craft-cloud-app` gains `libraries(kind) -> Vec<Library {space, name, path, items}>` over a new local
  call `GET /v1/libraries?kind=`, which reads the manifests of every synced space (personal and team). The kit adds
  nothing; apps read the files with their own loaders.
- **Editing**: editors edit the files (by hand or through an app that writes the manifest atomically). An app that
  edits a library concurrently with others may make `library.json` a live document of kind `document:library`; this
  needs no new protocol.
- **Versioning**: the space's history is the library's history. A named release (a version label on a manifest) is
  out of scope for P3.
- **Fonts and licences**: a font library holds only fonts the team has the right to share. The manifest's `licence`
  field names the licence per item (for example an SPDX ID or "studio licence"); apps show it. Craft Cloud never
  ships fonts, never fetches fonts, and does not check licences; the Craft Cloud app says so where libraries are
  managed.

## 10. Comments and review

Phase P5.

- **Decided (recommended): comments on a file are a live document of their own, kind `comments {node}`** (a new
  `DocKind` variant, appended), with the ID `comments_doc(space, node)`. Reasons: comments need offline writing,
  merging and real-time display, which the live service already gives; a table would need a second sync path, a
  change feed for comments and conflict rules. The cost: comments are not queryable by the server (no "all comments
  mentioning me" without the agent).
- **Content**: a Loro list of threads; each comment `{id, author (account ID, stamped by the agent from its own
  account), at_ms, text, anchor?, resolved?}`; `anchor` is app-defined bytes (a point on a canvas, a frame of a
  video, a text range).
- **Rights**: viewers may comment (review is the typical viewer's job): the server accepts `update` frames from
  viewers for `comments` documents only. Editing or deleting a comment is limited to its author by the kit (a server
  cannot check it without parsing Loro; the log keeps every change, so a forged edit is visible in history).
- **Mentions**: `@username` in text; the agent of the mentioned account shows a notification when a comments document
  of a synced space contains a new mention of it. No e-mail.
- Comments may be dropped from Phase 2 entirely if apps do not ask for them; P5 depends only on P2.

## 11. Locking

Phase P6, optional.

- **What**: an advisory checkout of one file for files that cannot be merged (layered images, video projects),
  so a second person sees "checked out by <username> since 14:03" before they start.
- **Table `file_locks`**: `space_id`, `node_id`, `user_id`, `device_id`, `acquired_ms`, `expires_ms`, `note`
  (at most 200 characters, never logged), `PRIMARY KEY (space_id, node_id)`.
- **Behaviour**: `POST /nodes/{id}/lock {ttl}` (editors; default 8 hours, at most 7 days, renewable),
  `DELETE /nodes/{id}/lock` (the holder, an owner, or an administrator, audited as `lock.overridden` when not the
  holder). A commit that updates, moves or deletes a locked node from another account is refused with a new conflict
  kind `locked`; the agent keeps the local edit as a conflict copy, as for any refused commit, so nothing is lost.
  Expired locks are ignored and removed on the next write to the table.
- **Old agents** do not know locks; for them the refusal is an ordinary conflict, which they already handle by
  keeping both copies.
- Locks never block reading, downloading or live documents (a live document is mergeable by definition).

## 12. Security and privacy

### 12.1 Threat model

| Threat | What stops it |
|---|---|
| A member reads a space they are not in | `require_space` on every route and the live upgrade; `404` for no access; chunk reads limited to the space's blobs; the space ID from the client is only a lookup key |
| A member guesses that a space or file exists | one `404` for missing and forbidden; `chunks/missing` per space; space names visible only to members and administrators |
| A malicious editor deletes or overwrites everything | versions and trash keep everything; removal from the trash is owner-only; retention is the administrator's; the mass-delete guard protects other members' local copies (a server-side mass delete is held on each machine until approved) |
| A malicious editor fills the disk | the team space's quota and live caps; per-account rate limits on bodies and frames as today |
| A member forges presence or comment authorship | the server stamps presence with the account; the kit stamps comment authors and history keeps every change |
| A removed member keeps syncing | access re-read on every request, long-polls woken, live sockets closed at once in-process and within one ping elsewhere |
| A revoked device | unchanged: tokens revoked, sockets closed; nothing in Phase 2 adds a credential to a device |
| A leaked share link (P4) | hashed tokens, expiry, revocation, rate limits, read-only, no listing above the shared node, no history |
| Enumerating accounts through invitations | members only (already signed in), one answer for unknown, disabled and removed accounts, 30 invitations per minute per account |
| A stolen admin token creating a space and adding itself | admin changes carry the password and second factor confirmation, as today; every step is audited |

### 12.2 Never lose data

- Losing access never deletes a local file ([5.6](#56-losing-access)).
- A viewer's local edits are kept as local-only files and conflict copies ([5.4](#54-read-only-spaces)).
- Archiving a space deletes nothing; removing a member deletes nothing in the space.
- A live document's unacked updates survive revocation in the agent's journal.
- Every Phase 1 guarantee (atomic writes, conflicts keep both copies, verified chunks, GC only of unreferenced
  blobs) holds per space, and GC counts references from every space.

### 12.3 Logging

- No file name, path, space name, username, link token or comment text at `info` or above. Space IDs and user IDs
  may appear at `info` (as device and target IDs do today).
- Audit rows hold IDs, role words and numbers only ([3.5](#35-audit-events)).
- Presence and comments are never logged at any level beyond their size.

## 13. UI

The redesign mockups (`WebFiles`, `LauncherCloud`) are taken as direction only; the screens below are described by
their behaviour, and their wording is the app's own.

### 13.1 Desktop (the Craft Cloud app)

- **Shared tab** (P1): a list of spaces: the personal space first, then team spaces with name, role, size and a sync
  switch for this computer ("Sync on this computer", off by default with the size shown). Each row opens the space.
- **Invitations** (P1): a banner and a section at the top of the Shared tab: "Ana invited you to <space> as editor",
  with Accept and Decline. Accepting asks whether to sync on this computer and where.
- **A space's page** (P1): folder location (open, change), status of its sync, usage against its quota, members,
  and Leave.
- **Members** (P1): the list with usernames, roles and pending invitations; owners get "Invite by account name"
  (a text field with the account name, a role picker), role changes and Remove, with a confirmation that says the
  person keeps what they already downloaded.
- **"Make a team space from this folder"** (P1-8, if Q2 is yes): from the personal space's folder list.
- **Admin pages** (P1): Team spaces: create (name, first owner, quota), list, archive, replace owner, quota.
- **Live** (P2): the overview's recent files show the people editing a live document ("Ana and Ben are here") from
  the agent's member and presence data; no new app call beyond the docs socket.
- **Libraries** (P3), **links** (P4), **comments** (P5), **locks** (P6): each adds its own page or column then.

Every screen gets kittest snapshots in `apps/craft-cloud-desktop/tests/snapshots/`, in light and dark.

### 13.2 Web

- P1: the admin pages gain a Team spaces page (list, create, quota, archive, replace owner), with the same
  confirmation as the JSON routes.
- P4: share link pages and member sign-in for reading files ([8](#8-share-links-and-web-file-access)).
- Nothing else changes on the web in P1 to P3.

## 14. Phases and pull requests

Each pull request merges on its own, keeps `cargo xtask ci` green on both database backends, and leaves the product
working: a PR that adds server support before the agent uses it is harmless because no client sends the header yet.

### P1: team spaces, membership, sync of a shared space, desktop UI

| PR | Content | Acceptance tests |
|---|---|---|
| **P1-1** meta: schema | migration 0015 for SQLite (rebuild with the runner's `foreign-keys-off` marker) and Postgres; `space_members`; `SpaceRole` in `craft-cloud-api`; `Meta::space_access`, `create_team_space`, member functions (invite, accept, decline, change role, remove, leave, list), `last_owner` rule in the transaction; new audit names | migration fingerprint test on a filled 0014 database (both backends); `foreign_key_check` empty; constraint tests; access matrix (personal owner, accepted member per role, pending, archived, disabled and removed accounts, other personal space); `last_owner` under concurrent role changes (two tasks, Postgres and SQLite) |
| **P1-2** server: space layer | `craft-space` header parsing and echo; `require_space` layer; every space-scoped handler takes `SpaceAccess`; role gates and in-transaction re-check; `read_only` code; protocol 2 in `/api/info` | table-driven test of every route × {none, viewer, editor, owner} × {personal, team}; no header is byte-for-byte the old behaviour (the existing server suite runs unchanged); a chunk of space A not readable through space B's header even when the store has it; a long-poll waiting on a team space answers 404 within 1 s of the member's removal; a commit racing a role downgrade is refused; grep test: no handler reads `auth.space` |
| **P1-3** server: routes and CLI | `/spaces…`, `/admin/spaces…` routes, rate limit on invitations, audit events, `space` CLI commands, `reset-epoch --space`, `docs/server.md` section | route tests for every success and error code; audit rows written once, none on a repeat; one answer for unknown, disabled and removed invitees; admin confirmation required; CLI tests |
| **P1-4** sync engine: read-only mode | `Transport` bound to a space; the read-only rules of [5.4](#54-read-only-spaces) | scenario tests: viewer edit kept and never uploaded; server change over a viewer edit makes a local-only conflict copy; local delete is downloaded again; role raised uploads local-only files; simulation seeds with a viewer agent never send a write |
| **P1-5** agent: several spaces | `account.json` `spaces`, per-space state under `spaces/<id>/`, one engine per synced space, scheduler, per-space long-poll, echo check, losing access ([5.6](#56-losing-access)), local API `/v1/spaces…`, status | network simulation (turmoil) with two accounts and one team space: edits converge, conflicts keep both copies; removal keeps local files and stops syncing; an old server (protocol 1) shows team spaces unavailable and sends no header; a server answering without the echo is refused; an existing Phase 1 data folder starts unchanged |
| **P1-6** app client and desktop UI | `craft-cloud-app` calls for spaces and members; Shared tab, invitations, space page, members, admin Team spaces page | model tests for every state; kittest snapshots of every new screen (light and dark); agent-too-old path |
| **P1-7** web admin page | Team spaces page in `/web` | page tests as for the existing admin pages (CSRF, confirmation, statuses) |
| **P1-8** (optional) claim and "team space from folder" | `POST /chunks/claim`; the desktop flow | claim refused without read access to the source; no bytes re-uploaded; source folder goes to the personal trash only after the team commit succeeded |

Rough size: large. About 7 PRs, of which P1-2 and P1-5 are the biggest; on the order of 8,000 to 12,000 lines
including tests. Risks: the SQLite table rebuild (mitigated by the fingerprint and foreign-key tests); missing one
handler when switching to `SpaceAccess` (mitigated by the grep test and the route matrix); agent scheduling across
spaces changing simulation determinism (passes stay sequential).

### P2: co-editing across accounts and presence identity

| PR | Content | Acceptance tests |
|---|---|---|
| **P2-1** proto | live protocol 3; `WelcomeSpace`, `PresenceFrom` appended; `member_colour` and the palette in `craft-cloud-api` | golden bytes of every protocol 2 frame unchanged; round-trip of the new frames; version negotiation 2↔3 both ways |
| **P2-2** server live | space from the upgrade header; role per frame; viewers never asked to compact; presence stamped; caps per space; connection registry and revocation | two accounts edit one document and converge (both backends); viewer `update` refused with `read_only`, `subscribe` and `presence` allowed; `PresenceFrom.user` is the sender's account whatever the bytes say; removed member's socket closed within 1 s in-process and at the next ping from another process (simulated); a protocol 2 agent without the header sees byte-identical frames |
| **P2-3** agent live | one link per team space; routing by path; `identity` and kind 5; `members` message; `read_only` in `opened`; blobs `for`; recents per space | documents convergence simulation with two accounts and a viewer; journals with unacked updates survive revocation; an app that does not ask for identity receives only kind 3; blob put with `for` lands in the team space and another member's agent reads it; recents of a team space reach another member |
| **P2-4** kit, app, egui | `Session::people()`, `session.blobs()`, read-only from `opened`, badge with people, `Blobs::for_path` | the kit demo's two-window test with two accounts on the fake hub; read-only session refuses local changes with the existing read-only path; warning when a team session's blob store is unbound |
| **P2-5** desktop | people editing shown on recent files | kittest snapshots |

Rough size: medium to large. About 5 PRs, 4,000 to 7,000 lines including tests. Risks: blob placement for apps that
keep the old constructor (mitigated by the warning and the one-line change); per-space caps and memory in the agent
(bounded by the existing 256-document limit); presence fan-out in large spaces (bounded by the existing per-device
rates).

### P3 to P6

| Phase | Content | Rough size | Depends on | Main risks |
|---|---|---|---|---|
| **P3** libraries | manifest format, `GET /v1/libraries`, `craft-cloud-app::libraries`, desktop Libraries page | small to medium, 2 to 3 PRs | P1 | agreeing the manifest format with the apps' maintainers; font licensing expectations |
| **P4** share links and web reading | `share_links`, `/s/{token}` pages and downloads, member web sign-in | large, 4 to 6 PRs | P1 | first file serving to browsers; streaming large files; abuse of public links |
| **P5** comments | `comments` document kind, kit comments API, mentions, desktop and app views | medium, 3 to 4 PRs | P2 | no server-side query; authorship enforced by clients only |
| **P6** locking | `file_locks`, lock routes, `locked` conflict, agent and app indicators | medium, 2 to 3 PRs | P1 | stale locks; users expecting locks to be enforced on old agents |

Tests per later phase follow the same pattern: a route × role matrix for every new route, a simulation scenario for
anything the agent syncs, golden bytes for every new frame, and kittest snapshots for every new screen.

## Decisions needed from the maintainers

| # | Question | Proposed default |
|---|---|---|
| Q1 | Who may create team spaces: administrators only, or every account (each creator becoming owner)? | administrators only in P1, with a config switch later |
| Q2 | Is folder sharing out of a personal space wanted at all, as "make a team space from this folder" (P1-8)? | yes, as an optional follow-up PR |
| Q3 | Default limit for a team space's stored bytes (`[spaces] team_stored_bytes`), and whether per-space retention overrides are needed | no limit by default, as for personal spaces; no retention override in Phase 2 |
| Q4 | Should a team space's recents be shared with all members? | yes; entries carry no account identity |
| Q5 | Local layout: team folders beside the personal folder (`~/Craft Cloud Shared/<name>/`) or nested (`~/Craft Cloud/Shared/<name>/`)? | beside |
| Q6 | Accept the SQLite table rebuild of `spaces` with a `foreign-keys-off` migration marker? | yes |
| Q7 | Should administrators have implicit read access to team spaces? | no; they add themselves, audited |
| Q8 | Should memberships made by an administrator skip the invitee's acceptance? | no, except the first owner named at creation |
| Q9 | Live bytes cap for team spaces (`[spaces] team_live_bytes`) | 4 GiB |
| Q10 | Comments as a live document per file rather than a table | live document |
| Q11 | Are share links and web file reading (P4) in scope for Phase 2 at all, and should member web sign-in be a phase of its own? | in scope, last; member sign-in split out if P4 grows |
| Q12 | Is the one-line blob change for apps with binary values in team documents acceptable, or must the agent place blobs without app help? | acceptable |
| Q13 | Wording of the font licensing note in the app and docs, and whether a font library should require a licence per item | the `licence` field is optional; the app shows a fixed note |
