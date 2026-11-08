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
  created and the folder's content moves into it (P1-8, see
  [6.2](#62-dedupe-across-spaces)). Q2 is decided: the flow is wanted, and only administrators may use it.
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
| `name` | `TEXT` | team display name, a `SpaceName` (NFC, 1 to 64 characters, no control characters, no leading or trailing whitespace, and not in any UUID text form); `NULL` for personal spaces |
| `name_fold` | `TEXT` | Unicode full case fold + NFC of `name`, as for node names; `NULL` for personal spaces |
| `created_ms` | `BIGINT NOT NULL DEFAULT 0` | new; set from `users.created_ms` for existing personal spaces |
| `archived_ms` | `BIGINT` | new; `NULL` while active |

Constraints and indexes:

- `CHECK ((kind = 'personal') = (owner IS NOT NULL))` and `CHECK ((kind = 'team') = (name IS NOT NULL AND name_fold IS NOT NULL))`.
- The column-level `UNIQUE` on `owner` is replaced by `CREATE UNIQUE INDEX spaces_personal_owner ON spaces (owner) WHERE owner IS NOT NULL`.
- `CREATE UNIQUE INDEX spaces_team_name ON spaces (name_fold) WHERE kind = 'team'`.

The migration SQL runner splits statements on `;`, so migration comments must not contain semicolons.

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
| 0015 | `physical_size` | existing integration-branch migration | Phase 1 |
| 0016 | `team_spaces` | the `spaces` changes above, `space_members` | P1 |
| 0017 | `share_links` | P4 | P4 |
| 0018 | `file_locks` | P6 | P6 |

The team spaces migration is 0016 in both dialects, after 0015 `physical_size`. A development database from the earlier team branch, in which version 15 meant team spaces, cannot be upgraded on this integration branch and must be recreated; no released build used that numbering. The tests count the migration versions 15 to 16 and undo the team spaces migration before the older migration tests run.

**Postgres** (0016) is a plain sequence of `ALTER TABLE` statements: add the columns, drop `NOT NULL` on `owner`,
drop the unique constraint on `owner` (Postgres names it `spaces_owner_key`; the PR checks the name against a real
database in CI rather than assuming it), add the checks and the two partial indexes, set `created_ms` from the owner's
`users.created_ms`, create `space_members`.

**SQLite** (0016) cannot drop a column constraint, so `spaces` is rebuilt with SQLite's documented procedure: create
`spaces_new` with the new definition, copy every row, drop `spaces`, rename `spaces_new` to `spaces`, recreate its
indexes. Because other tables reference `spaces (id)`, the rebuild must run with foreign-key enforcement off.

- **Decided (recommended):** the migration runner in `crates/meta/src/schema.rs` gains one marker for SQLite
  migrations, a first line `-- craft: foreign-keys-off`. For such a migration the runner sets
  `PRAGMA foreign_keys = OFF` on its connection before `BEGIN` (the pragma has no effect inside a transaction), runs
  the statements, runs `PRAGMA foreign_key_check` inside the same transaction and aborts it if any row comes back,
  commits, and sets `PRAGMA foreign_keys = ON` again, also on failure. Reason: it is the only way to change the
  constraint without a hidden "team user" row in `users`, which every account list, sign-in and invite path would
  then have to filter out. The cost is one extra branch in a small runner, tested on its own.
- Tests for the migration (both backends): a database at 0015 filled with users, devices, nodes, versions, blobs,
  live documents and trash is migrated, and a fingerprint of every other table is equal before and after; every
  space is `personal` with its owner; `foreign_key_check` is empty; the new checks refuse a personal space without
  an owner, a team space with one, and a second personal space for an account.

### 2.4 How existing personal spaces carry over

Nothing a Phase 1 server stores changes meaning:

- Every existing space becomes `kind = 'personal'` with the same `owner`, `id`, `epoch`, `seq` and counters.
- `DeviceAuth::personal_space()` resolves the account's personal space for device-scoped reads. The token check's
  join `spaces s ON s.owner = u.id` keeps working because only personal spaces have an owner.
- No row goes into `space_members` for personal spaces.
- Agents, apps and the change feed see no difference until a team space exists and they ask for it.


## 3. Permissions

### 3.1 Where the check happens

One function decides access: `Meta::space_access(user, space) -> Option<SpaceRole>`, one indexed query:

- personal space: `Owner` if `spaces.owner = user`, else none;
- team space: the member's role if the membership is accepted and the account is enabled and not removed; else none. An archived space keeps its memberships and stored roles, but effective access is capped at `Viewer`.

The server resolves it in a layer, `require_space`, that runs after `require_device` on every space-scoped route and puts a `SpaceAccess {space, role, personal, required}` into the request. `required` is the route's minimum role, and the write transaction uses it again for its re-check. Handlers take the space from `SpaceAccess`, never from `auth.space` or `device.space`. Account-level reads use `DeviceAuth::personal_space()` instead: the device epoch, `GET /account/usage`, `Session::new`, and the request without a header. A grep test pins the remaining callers.

`SpaceAccess::meta` supplies `Meta::guarded(WriteGuard {user, min})`. Writes to a team space re-check the role inside the write transaction: commit, restore from trash, restore version, attach save, request purge, and chunk and manifest uploads. Writes to a personal space need no re-check. On Postgres, the space row is locked with `FOR UPDATE` for commits and restores and with `FOR NO KEY UPDATE` for uploads. An upload updates the space row's byte count, and a weaker lock (`FOR SHARE`) would let two concurrent uploads deadlock. A refused re-check is `read_only` when the role is too low or the space is archived, and `not_found` when access has been lost.

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
  waiter re-checks access after waking and answers `404 not_found` if access ended. A waiter in another
  server process notices at the end of its wait (at most 60 s), like a revoked device today.
- **Live**: the server keeps, per process, a registry of live connections by `(device, space)`, capped at 4 per device
  and space and 32 per device. A membership or role change within the process closes that user's connections with
  `access_changed` (WebSocket close code 4403). A connection trusts the account state for `Limits::live_recheck` (default 5 s). After that interval,
  before relaying update pages or `presence_from`, before accepting presence, and on writes and subscribes, it
  rechecks the device and then the access. Presence causes no database read per frame. A removal made by another
  process or by the command line therefore stops sending and receiving within about 5 s, not only at the 30 s ping.
  `Notifier::subscribe_members` triggers a check immediately and then debounces further checks to once per second.
  Disabled or removed accounts have their devices revoked, and their connections close with `unauthorized`
  (`this device was signed out`).
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
| `cli.space_created`, `cli.space_member_changed`, `cli.space_archived` | the command line, with no actor |

Each event is written in the transaction of its change, a repeat or a no-op writes nothing, and the account's own
activity (`GET /account/activity`) shows events where it is the actor or the `user` in `details`. Files, uploads and
live documents remain unaudited, as today.


The following details apply to the audit events and the member routes:

- `details` keys are `Field::{User, SpaceRoleFrom, SpaceRoleTo}`. `space.created` and `cli.space_created` always include `quota_bytes`, which is `null` when no explicit limit was given. The command line events are `cli.space_created`, `cli.space_member_changed` (for an invitation, a role change or a removal) and `cli.space_archived` (for `space archive`). The audit list has 45 names. Web admin space events use the administrator as actor and no device (`AdminVia::Web`); the existing `SpaceActor::Admin(AdminActor)` represents this.
- `GET /account/activity` also matches `details.user`, through `details LIKE '%"user":"<id>"%'`. `PATCH /spaces/{id}` validates the name and the quota together before it writes anything. Owner changes to members, name and archive state are checked against the owner's role; only the `/spaces` archive and unarchive routes ask for the owner's password. Administrator confirmation applies even to a rename. The quota always requires an administrator (`403 forbidden` for members, `404` for everyone else). An administrator who is also an owner acts as administrator when confirming and as owner otherwise. A refusal for a too-low role inside the transaction is `403 read_only`.
- `DELETE` on a caller's own row means that an accepted member leaves or an invited account declines. The last owner leaving gets `409 last_owner`. An invitation returns `200` with `{state: invited}`. Once the caller is authorized, the rate limit is applied before the username lookup, and unknown names count against it. `Retry-After` is rounded up, with a minimum of one second. `space add` on a pending invitation sets the role and accepts it. Team names shaped like a UUID are rejected. The administrator's search uses folded, trimmed text (`craft_cloud_api::fold_search`) of at most 64 bytes, not `SpaceName` validation. Owner counts in the admin views include only accepted owners who can sign in. Replacing the owner adds an accepted owner and removes none; the event is `space.member_role_changed`, with `role_from` when the row already existed, and a no-op writes no event.
- Member, role, rename, archive and owner changes, and the web archive, unarchive, quota and owner changes, call `Notifier::members_changed(space)` after the commit. Disabling or removing an account wakes each of its member spaces, from both the JSON and the web admin routes. A long-poll rechecks after the wake and answers `404`; other processes notice by the end of the wait, at most 60 seconds. `GET /spaces` lists archived memberships with their stored role, and clients must use `archived` rather than the role alone. `members` is the count of accepted members, and an archived invitation is hidden.

## 4. Protocol

### 4.1 Naming the space on a request

- **Decided (recommended): a request names its space with the header `craft-space: <space UUID>`**, on every
  space-scoped HTTP route and on the live WebSocket upgrade. No header means the device's personal space, exactly as
  today. Reasons: no route changes shape, so every Phase 1 route, test and agent keeps working unchanged; the space
  is chosen once per request in one layer; and the same header works for the WebSocket upgrade, where a path
  parameter would need a second router entry.
- Every answer of a space-scoped route **echoes** `craft-space` with the space it worked on (also when the header was
  absent: the personal space's ID). A new agent refuses any answer for a team space whose echo is missing or
  different, and stops syncing it, with the run state `Protocol` and the message `the server is too old for team spaces; ask the administrator to update it`. This closes the hole where an
  older server, ignoring the unknown header, would answer about the personal space and the agent would mix two trees.
- The header must be exactly one hyphenated UUID of 36 characters, in any letter case. A braced, URN or simple (non-hyphenated) form, or a duplicate header, is `400 invalid`.
- The echo is on every answer of the space layer, including `404 not_found` and `403 read_only`, except the `400` for a malformed header and `401`. `craft-epoch` is the epoch of the space worked on; routes outside the space layer keep the personal epoch. The layer's own `404` and `403` carry no `craft-epoch`.
- The agent checks the echo on success and on `403`, `404`, `409` and `410`. A `400`, `401` or `429` can come before the layer or from a proxy, so it is not checked. A missing or different echo is a `Protocol` failure with the message `the server is too old for team spaces; ask the administrator to update it`: the agent stops passes for that space, retries slowly, and shows this run state.
- `POST /chunks/claim` answering `404 no such endpoint` is the exception. It means that there is no claim route, and it carries no echo.

### 4.2 New HTTP routes (P1)

All under `/api/v1`, JSON, behind `require_device`. Bodies are read up to 4 KiB (the tiny-body limit) except where
noted.

| Call | Who | Purpose |
|---|---|---|
| `GET /spaces` | any account | `{spaces: [{id, kind, name?, role, state: active\|invited, archived, created_ms, joined_ms?, members, root, epoch}]}`: the personal space first, then team spaces by name; invitations are included only for spaces that are not archived (with no `root` or `epoch` until accepted); `members` counts accepted members (1 for a personal space). Archived spaces remain listed for every accepted member with their stored role and `archived: true`, and access is capped at viewer. `joined_ms` is the time the membership was accepted; it is stable across role changes, is new after a re-invitation, and is absent for personal spaces, invitations, and older servers. |
| `GET /space` | member | as today, plus `kind` (`personal` or `team`), `role`, and team `name` (also without header) |
| `GET /spaces/{id}/members` | member | `{members: [{user, username, role, state, colour}]}`; `colour` is the palette index of [7.1](#71-presence-identity) |
| `POST /spaces/{id}/members` | owner, admin | `{username, role}` → `200` and member row (`state: invited`). Unknown, disabled or removed account: `404 not_found` (one answer for all three). Already a member or invited: `409 conflict`. 30 per account per minute, then `429 rate_limited` |
| `PATCH /spaces/{id}/members/{user}` | owner, admin | `{role}` → the row; lowering the last owner is `409 last_owner` (new code) |
| `DELETE /spaces/{id}/members/{user}` | owner, admin; the member for their own row | removes the member or withdraws the invitation (204); the last owner leaving is `409 last_owner` |
| `POST /spaces/{id}/accept`, `POST /spaces/{id}/decline` | the invited account | accept → the space as in `GET /spaces`; decline → 204. No membership row, or an invitation to an archived space: `404 not_found`; a row in the wrong state is also `404 not_found` |
| `GET /spaces/{id}/usage` | member | `{stored_bytes, quota_bytes, trash_bytes, history_bytes, live_bytes, estimates}` as `/account/usage`, for the space |
| `PATCH /spaces/{id}` | owner (name), admin (name, quota) | `{name?, quota_bytes?, password?}`: renaming needs no password; a quota change is an admin change and carries the admin confirmation (password, and the second factor when on) as every admin change does |
| `POST /admin/spaces` | admin, confirmation | `{name, owner: user id, quota_bytes?, password, totp_code?}` → the space. The owner is added as an **accepted** owner (an administrator naming the first owner is the consent step for that one row; see Q8). `409 conflict` for a taken name |
| `GET /admin/spaces?cursor&limit&query` | admin | `{spaces: [{id, name, created_ms, archived_ms, members, owners, stored_bytes, quota_bytes}], next?}` |
| `POST /admin/spaces/{id}/archive`, `/unarchive`; `POST /spaces/{id}/archive`, `/unarchive` | admin with confirmation; an owner with their password on the `/spaces` form | archive: every member's access is capped at viewer; the space stays in accepted members' lists with `archived: true` but is hidden from invitees; nothing is deleted. Unarchive reverses it |
| `PUT /admin/spaces/{id}/owner` | admin, confirmation | `{user}`: makes the named account an accepted owner; existing owners remain |

The command line gains `space create <name> --owner <user>`, `space list`, `space members <space>`,
`space add <space> <user> --role <role>` (added accepted: whoever runs the command line has the files), `space archive`
and `reset-epoch --space <id>`.

Error codes added to `craft_cloud_api::codes`: `read_only` (403), added in P1-2, and `last_owner` (409), added in P1-1. Every other answer reuses
existing codes.

### 4.3 Compatibility and protocol versions

| Party | Old | New |
|---|---|---|
| HTTP protocol (`PROTOCOL_MAX` in `crates/proto/src/api.rs`) | 1 | **2**: the `craft-space` header and echo, the routes above. `PROTOCOL_MIN` stays 1 |
| Live protocol (`LIVE_PROTOCOL_MAX` in `crates/proto/src/live.rs`) | 2 | **3** (P2): the space on the upgrade, `welcome_space`, `presence_from`, `read_only` refusals |

- **Old agent, new server**: sends no header, speaks protocol 1 and live protocol 2; it gets its personal space and
  nothing else, byte for byte as today. It never learns that team spaces exist.
- **New agent, old server**: `/api/info` says protocol 1; the agent syncs the personal space as today and reports
  team spaces as unavailable ("the server needs an update for team spaces"). The agent asks `/api/info` before it asks `/spaces`: protocol 1, or a `/spaces` answer of 404, means that team spaces are unavailable, and the version is checked again every 20 long-poll waits. A `/spaces` 404 after protocol 2 has been seen is only a failed look: it triggers a recheck of `/api/info`, and only protocol 1 means unavailable. When the list refresh waits after an accept or decline, failed discovery is retried after 3 seconds for up to 15 seconds. The agent never sends `craft-space` to a
  protocol 1 server, and the echo check covers the case where it would.
- **New agent, new server, live protocol 2 only** (P1 shipped, P2 not yet): the agent opens no live connection for a
  team space; documents of files in team spaces answer `not_live {offline}` and the app works on the file as
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
| `WelcomeSpace {protocol, epoch, space, role}` | server → agent | as `Welcome`, plus the space and the role | instead of `Welcome` when the negotiated protocol is 3, including for the personal space (`role: owner`) |
| `PresenceFrom {doc, bytes, user}` | server → agent | as `Presence`, plus the sending account's ID, stamped by the server | instead of `Presence` on protocol 3 |
| `Error {code: "read_only", …}` | server → agent | existing frame, new code | a viewer's `create`, `update` or `snapshot` |

- On protocol 3 the server never asks a viewer's device to `compact` (it could not upload the snapshot).
- On protocol 2 (old agents) a connection without the header is the personal space and gets exactly the old frames.

### 4.5 Local API additions (agent ↔ apps)

All under the agent's existing loopback rules. New calls answer the local API's `not_found` on an old agent, which
`craft-cloud-app` reports as `agent_too_old`, as it does for blobs.

| Call | Purpose | Phase |
|---|---|---|
| `GET /v1/spaces` | `{spaces: [{space, kind, name?, role, state, folder?, sync: on\|off\|unavailable, run_state?, last_sync_ms?, read_only}]}`; the personal space comes first | P1 |
| `POST /v1/spaces/{id}/sync` | `{on, folder?, adopt?}`: turn syncing of a team space on or off on this computer and optionally choose the folder; a non-empty folder requires `adopt` | P1 |
| `POST /v1/spaces/{id}/accept`, `/decline`, `/leave` | accept and decline pass through to the server; leave calls the server first and, on success, ends syncing and keeps the files | P1 |
| `GET`, `POST`, `PATCH`, `DELETE /v1/spaces/{id}/members…`, `GET /v1/spaces/{id}/usage`, `GET`/`POST /v1/admin/spaces…` | passed through to the server for the Craft Cloud app's pages | P1 |
| `GET /v1/status` | appended `spaces: [{space, run_state, last_sync_ms?}]`; the top-level fields keep describing the personal space | P1 |
| `GET /v1/recents` (WebSocket) | `subscribe` gains an optional `space` (appended); without it, the personal list as today. Entries added with a path inside a team folder go to that space's recents | P2 |
| `GET /v1/docs` (WebSocket) | `open` gains optional `identity: true`; `opened` gains `read_only` and `space`; new binary kind 5 and text message `members` ([7.3](#73-agent-and-local-api)) | P2 |
| `PUT /v1/blobs`, `GET /v1/blobs/{hash}` | optional query `for=<path of the document's file>`: the blob goes to (or is read from) that file's space; without it, the personal space as today | P2 |


The P1-7 web admin page is `GET /web/spaces?query&cursor` (search by folded name, 50 per page, showing owners, members, usage and limit, and the active or archived state) plus `GET /web/spaces/{id}` (read-only members and roles, the limit, archive and unarchive, and the owner). Writes use `POST /web/spaces`, `/quota`, `/archive`, `/unarchive` and `/owner`, with CSRF, Origin and administrator confirmation checks. Owners' member changes and renaming remain in the app, not on the web page. The name entered for create or owner is validated before the confirmation, so a mistyped name consumes no code.

## 5. Agent

### 5.1 One folder per space

- The personal folder and its state remain unchanged.
- **Decided (recommended):** team roots live in a folder named `Craft Cloud Shared` beside the personal folder, wherever the user placed it; with the default personal folder this gives `~/Craft Cloud Shared/<team space name>/`. A personal folder without a parent directory requires the user to choose the team folder.
- The folder name follows the node name rules: `/ \ < > : " | ? *` and control characters become `_`; trailing spaces and dots are dropped; a leading dot becomes `_`; the name is at most 100 characters and NFC; reserved names gain ` (team space)`; an empty name becomes `Team space`. A case-insensitive clash with another root or an existing on-disk folder adds ` (2)`, ` (3)` and so on. A rename on the server does not rename the local root; the app shows the new name and offers to rename the folder.
- `POST /v1/spaces/{id}/sync` takes `{on, folder?, adopt?}`. An absolute folder must lie outside the personal root, the synced team roots and the data folder. A non-empty folder needs `adopt`, otherwise the answer is `folder_has_files`; a marker of another space refuses the folder. Turning sync on creates the folder, the marker and fresh state. An accepted space has sync off on every computer by default.

### 5.2 State

`account.json` keeps its fields and gains `spaces: [{space, folder, sync, name?, root?}]` (appended; an older agent ignores it). `name` is the last name the server gave the space, kept for lost-access messages; `root` is the identity named by the marker. The field is omitted while there are no entries, so an installation that never saw a team space keeps its file byte for byte.

Per team space, the agent's data folder holds `spaces/<space id>/state.json` and `spaces/<space id>/status.json`,
written atomically like the personal ones. Live data already lives under `live/<space>/` and needs no change. The
personal space's files stay at the top level.

### 5.3 Turning a space on, scheduling

- **Decided (recommended):** an accepted team space starts with sync off on every computer. The app asks whether to sync and where; no computer starts syncing by itself.
- Each synced space has its own engine, state, root, marker, scheduler, watcher and stability window, and long-poll. `HttpTransport::bound_to(space)` sets `craft-space` on every request, and P1-5 checks the echoes. `state.json` records the space and the canonical folder and applies only to that pair; otherwise the first sync merges both sides without deleting anything. Turning sync off forgets the state, and turning it on again is a fresh merge. Signing out removes the team state but keeps the folders, markers and files. Moving or turning off a space closes its session and waits for its pass thread before the state or folder changes.
- `PassGate` alternates personal and team passes. The team passes run on their own thread, round-robin across spaces. Pausing ends the team long-polls, and resuming starts them again. Turning a space off or moving it waits only for that space's session and pass, while other spaces, the local API and stop handling continue. The call answers when the session is closed or after 50 seconds; if the space is still busy, the answer is `this space is still finishing its last pass, try again in a moment`. A queued pass never runs after the close, including during a stop. A concurrent change to the same space gets `409` (`this space is being changed, try again in a moment`). Turning sync on waits only for that space's own active session and pass and, after 50 seconds, returns the same still-finishing error. A stop during a turn-off saves the off state for the next start.
- A second `run` stop ends the team spaces immediately: no queued pass starts, and the pass thread is not joined. If the first stop is waiting on a hung pass, the loop keeps reading signals, and the second stop interrupts the wait (`SpacesHandle::stop_now`). The wait is not time-bounded.
- Mass-delete approval is stored in `spaces/<id>/approval.json`. It is given with `craft-cloud-agent sync --approve-deletes[=<set>] --space <id or name>` or with `POST /v1/spaces/{id}/approve-deletes` `{set?}`, which returns `{approved, message}`. Each computer approves its own pass, and the command needs a running agent. The approval set is the one last shown or the named waiting set.
- `POST /v1/spaces/from-folder` takes `{folder, name, quota_bytes?, password?, totp_code?, recovery_code?}` and returns `FromFolderStatus`; `/v1/status` adds `from_folder`. The server validates the source and creates the space; then the agent copies, syncs, verifies and quarantines the source before reporting Done. The journal `from-folder.json` records the progress. After a restart the job is reported as `Interrupted` with `failed_at`, and retrying with the same folder and name resumes the same space without a new confirmation. Only one job runs at a time; otherwise the answer is `409 conflict`. A half-created team space is never removed.

### 5.4 Read-only spaces

For a space where the role is `viewer` (from `GET /spaces`, or learnt from a `403 read_only`), the engine runs in a
read-only mode:

- Remote changes are applied as usual.
- A local edit is never uploaded and never overwritten. When the server's version of an edited file changes, the
  server's version lands under the original name and the local edit is kept under a conflict name (the existing
  conflict naming), and is listed in the skipped list of the pass report as `edited here, not uploaded: ...`. The conflict copy is an ordinary untracked file, not an item of the conflict list.
- A new local file stays local and is listed as `not uploaded: this space is read-only for you; the file stays on this computer`. The list is reported again only when its contents change.
- A local delete is undone by downloading the file again on the next pass, with a notice. (The alternative, keeping
  the deletion local, would make the folder silently differ from the space; undoing is safe and visible.)
- A role raised to `editor` switches the engine back; local-only files are then uploaded as ordinary new files on the
  next pass (conflict copies keep their names).
- Files are not given the operating system's read-only attribute: on Windows that makes apps fail to save in
  confusing ways, and the agent never blocks an app's save.


The engine enters read-only mode through `Engine::set_read_only` or on its first refused write (`403 read_only`, surfaced as `TransportError::ReadOnly`). A refused write stores nothing and ends the writes of that pass.

- Each read-only pass checks `GET /space`; a role of editor or higher ends read-only mode for that pass. A missing role (old server) or a failed read changes nothing. A role change requests a pass immediately.
- A local delete, rename or move is undone by restoring the server copy at its old path; a moved file remains as a new untracked local file. Deleted or renamed folders are restored with their files.
- A folder path occupied by a file or a link belongs to the user: the agent never puts anything through it or moves it aside, and links produce a node filesystem error. A restored folder is reported once as restored, not as created.

The following further rules apply in read-only mode:

- Edits remain local and are never uploaded. Conflict copies are ordinary untracked files. They are listed in the skipped list of the pass report as `not uploaded: this space is read-only for you; the file stays on this computer`, and edited tracked files as `edited here, not uploaded: ...`. The list is updated only when its contents change.
- If the server deleted a file that a viewer had edited, the edit remains as an untracked file; it is neither deleted nor uploaded. On an epoch restore, a newer local copy remains local-only.
- A read-only pass reads an edited file only when its size matches the agreed content. The answer is cached while the size, time, ID and agreed content stay unchanged.
- A refusal in a later batch preserves the earlier committed work and the conflict copies that were already moved aside. Content that was not committed is not reported as uploaded, and the affected files are listed as kept.
- After the role rises, the first editor push may hold back a local delete until the next pass.
- A tracked file swapped for another tracked file counts as missing and is restored; it is not listed as kept. This affects the listing only, and both files remain.
- When two Craft saves have changed in read-only mode, they take the ordinary conflict-copy path; there is no case in which "ours goes up".


The following details apply to the agent lifecycle and to the from-folder flow:

- The team spaces share one thread of passes, and the personal space and the team spaces alternate through `PassGate`. The first stop can wait on a hung team pass while the loop keeps reading signals; a second stop ends the wait (`SpacesHandle::stop_now`) without joining the pass thread. The wait has no time bound. Pausing stops the long-polls, and resuming restarts them.
- `POST /v1/spaces/from-folder` takes `{folder, name, quota_bytes?, password?, totp_code?, recovery_code?}` and returns `FromFolderStatus`; `/v1/status` adds `from_folder`. The server checks the source and creates the team space; then the agent runs the steps SettingUp, Copying, Syncing, Verifying and Trashing, and ends in Done, Failed or Interrupted. It copies first and trashes the source only when the server tree has verified manifests and the source, read again, is unchanged. The deletion of each file rechecks this. Changes made after the copy remain and count as kept, and unsupported files such as `*.tmp` remain too. The personal sync sees the local deletion, the mass-delete guard applies, and the server trash retains the files.
- `from-folder.json` journals the step and the space. After a restart the job is reported as `Interrupted` with `failed_at`, and retrying with the same folder and name resumes the same space without a new confirmation. A half-made space is never removed, and only one job runs at a time (`409 conflict`). The session number and the account are checked before and after every wait; on a mismatch the job writes no journal, status or job and returns `not_signed_in`, and an empty created space remains. The job is registered under the `end_session` lock and is refused or cancelled if the session has ended.
- Recovery after a failed creation adopts only an empty space of which the account is the sole owner and member, and which was created after the stored server `Date`, with 30 s of slack. The `Date` is captured before the creation, journaled before the request and never refreshed on a retry; with no `Date`, nothing is adopted. A Trashing journal whose old folder is missing counts as Done only when the cause is not-found.
- A stall in Syncing is measured by the stored bytes (`GET /spaces/{id}/usage`) and the committed files: ten minutes with neither growing is a stall. The tree is read at the end of a team pass; otherwise the agent polls every 0.4 s, doubling up to 5 s. Destination files are compared by manifest, never by size or time. The kept reasons are named separately: files changed after the copy (not in the team space as it is now), files that could not be deleted because a program holds them (in the team space), and other files that stayed. The kept count includes only the first two.
- Trashing refreshes the server list before the first batch and then every 1,000 files or 60 seconds. It requires a listed, active, unarchived, non-viewer membership and server tree manifests for every remaining file. A failure stops with `Failed` at `Trashing` and the message `nothing more was deleted`. A cheap direct `GET /spaces` membership check runs before each batch; the full tree check runs before the first batch and at most every 10 minutes.
- Each file is quarantined under `<personal folder>/.craft-cloud/from-folder/<key>/<same path>` on the same volume, where the sync ignores it. Nested folders are checked for symlinks, junctions and other reparse points. The file is read again against the verified manifest: a match is deleted, and a mismatch is restored without overwriting anything.
- Putting a quarantined file back uses a hard link and then removes the old name. Occupied names are refused, with the fallbacks `<name>.cc-kept` and `<name>.cc-kept-2`; where hard links are unsupported, the file stays in place and counts as kept.
- The recovery marker is an atomic sibling file, `<key>.root`; reading it back must give the old folder path, and if it cannot be written, nothing is moved. Recovery trusts only an absolute, normalized path that lies inside the personal root but is not equal to it and that matches the quarantine key; an invalid marker leaves the folder untouched and is reported. At every start, and before the scan, all marked quarantines are recovered, independent of the journal and the account. A failure to restore reports `Failed` at Trashing and is retried at the next start. A later successful recovery changes an earlier move-back error to `Interrupted` at Trashing.
- The paths and IDs of the folder chain are checked at the scan and before each batch and file. A link, or a mismatch of ID and path, fails with `moved or replaced`, and nothing more is deleted. The check and the move remain separate operations, so a race window of microseconds remains, and a write to an open file between the final read and the delete can be lost on Windows and Unix.

### 5.5 Conflicts across accounts

Nothing new: the engine's conflict rules are per space and already handle two devices editing one node. A conflict
copy's name carries the device name (`name (conflict, <device>, <date>).ext`); in a team space that is another
person's device name, which members see. Device names are chosen by their owners and already appear in their own
device list; the Craft Cloud app says so when naming a device. Save records ([`live.md`](live.md), "Save records")
work across accounts unchanged, so two people saving the same live document produce no conflict copy.

### 5.6 Losing access

A space stops only when the server list no longer has an active membership. A `404` in a pass or long-poll triggers an immediate list refresh but never stops the space by itself, because a `404` can also mean a missing chunk. Archived memberships stay active; their access is capped at viewer, and the agent continues with read-only sync.

When the membership ends, the agent stops syncing and the live link, keeps all files, and reports `run_state: no_access`. Live journals remain in `live/<space>/`. The app says `Your local copy is kept as it is and no longer syncs` and offers Open folder. No agent call forgets a space or removes its marker; the user removes the files in the file manager. Debug output omits the names of no-access spaces, and logs use the space ID only.

When access returns, the dropped state forces a fresh first-sync merge. The merge never replaces or deletes a local file: a locally deleted file returns from the server, a file deleted remotely is uploaded again from the local copy, and divergent files keep both through a conflict copy. `joined_ms` is stored with the state; a different stamp, or a missing stamp when the server supplies one, forces a first sync. Without a server stamp, the old behavior remains.

### 5.7 Live hub and recents per space

- The hub keeps one link (WebSocket) per synced team space with live protocol 3, plus the personal link.
- A document is routed by the path the app opens: the agent finds the synced root that contains the path (absolute
  paths are needed for team files; a relative path is relative to the personal folder, as today).
- Recents: each team space has its own recents documents (`recents_doc(space, app)` already derives the ID from the
  space). An app's `recents()` call keeps returning the personal list; the new `subscribe {space}` returns a team
  space's list. Team recents are shared with all members; entries carry no account identity.



The following details apply to the local API:

- `GET /v1/spaces` lists the personal space first and appends `read_only` to each entry. `GET /v1/status.spaces` has one entry for each syncing or lost space: `{space, run_state, last_sync_ms?, read_only}`. `run_state` gains `no_access {message}` and `unavailable {message}`, and `Paused.reason_code` may be `server_full`, `quota_exceeded` or `read_only`.
- Accept and decline pass through, preserve the server's answer, and then refresh the list. `/leave` sends the server `DELETE` first; if the server refuses (`last_owner`, `429`, no connection, revoked device), nothing changes locally. On success the membership is removed, then syncing ends and the files are kept, and the lost-access entry remains. The desktop app blocks a last-owner leave before it sends the request.
- The P1-6 pass-throughs include members, usage, rename and admin limit, and admin list, create, archive, unarchive and owner. A missing server route is `409 server_too_old` (`the server needs an update for team spaces`); the app displays `The server doesn't support team spaces yet. Ask its administrator to update it.`

## 6. Quota, chunks, GC and trash

### 6.1 Who is charged

- **Decided (recommended): a team space's bytes are charged to that space, never to its members.** `stored_bytes` and `live_bytes` are per space already. A team space's limit is its `quota_bytes`, otherwise none; the config key `[spaces] team_stored_bytes` is not implemented. `stored_bytes_per_user` applies to personal spaces only. Q3: there is no default limit and no per-space retention override in Phase 2.
- Over its limit, uploads to a team space return `507 quota_exceeded` with `quota: {used, limit}`; the agent reports it per space, and other spaces keep syncing.
- `GET /admin/storage` lists team spaces beside accounts, showing the top ten by stored bytes.

### 6.2 Dedupe across spaces

- Chunks and manifests are stored once in the store, as today. `space_blobs` records which spaces have which blob,
  `chunks/missing` answers per space, and a space can only download blobs it has: none of this changes, so dedupe
  still reveals nothing across spaces.
- A file copied from the personal folder into a team folder is uploaded to the team space again (its chunks travel
  once more). **P1-8** adds `POST /chunks/claim {from_space, ids}`: the server adds to the
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

Retention by thinning (M4) applies per space with the server's windows. Per-space retention overrides are not implemented.

`POST /chunks/claim {from_space, ids}` uses the `craft-space` header as the target. The caller needs an editor role in the target and read access to the source (an archived source counts); an inaccessible source receives exactly the unknown-space response. At most `MAX_MISSING_IDS` (10,000) IDs are accepted, otherwise the answer is `400 invalid`. The answer `{claimed, missing}` partitions every requested ID exactly once, in request order; `claimed` includes blobs that are already in the target. Added blobs are charged to the target at their logical stored size, and a quota overflow rejects the whole request with `507 quota_exceeded`. The call is idempotent, moves no bytes and writes no audit event. When an upload is refused at the header level, the server drains or drops the remaining body, up to the largest manifest plus 1 MiB and for at most 10 seconds, so that clients receive `413` or `507`; oversized or slow bodies are still cut off at those bounds.

## 7. Co-editing across accounts

### 7.1 Presence identity

- The server stamps every relayed presence message with the sender's account ID (`PresenceFrom.user`). An app cannot
  claim to be someone else: the bytes are the app's, the identity is the server's.
- What a member learns about another member: the account's **username** and a **colour index**. Nothing else is
  added (no e-mail, no real name, no device name in presence). The username is already visible to members in the
  member list.
- **Decided (recommended):** the colour is derived, not stored. `member_colour` hashes the account ID with BLAKE3, interprets the 32-byte digest as a big-endian number and takes it modulo 12. The fixed palette lives in `craft-cloud-api` with light and dark sRGB values; the colours are named red, orange, amber, lime, green, teal, cyan, blue, indigo, purple, pink and slate. P2-1 adds BLAKE3 to the API crate. The palette values and the colours of five known accounts are pinned by tests and remain a design choice to be reviewed against the apps' colours.

### 7.2 The live service's space scoping

The server's live module (`apps/craft-cloud-server/src/live.rs`) is per space already; P2 changes where the space
comes from:

- On upgrade: `require_space` resolves the header (absent: personal), refuses a non-member with `404` before the
  upgrade, and puts the role into the connection.
- Per frame: `create`, `update` and `snapshot` require `editor`; `subscribe`, `subscribe_log`, `presence` and
  `unsubscribe` require `viewer`. Access is rechecked by the periodic account check and by the member counter signal, and guarded operations check the current role.
- Documents of files: `node_doc(space, node)` is unchanged; a document's space is the connection's space.
- Admin command `live reset-doc` gains `--space <id>` for team spaces.


`member_colour` and the palette (`PALETTE`, with light and dark sRGB values, and `palette_colour(index)`) live in `craft-cloud-api::colour` and are re-exported through proto; the API crate gains BLAKE3. The twelve names are red, orange, amber, lime, green, teal, cyan, blue, indigo, purple, pink and slate. The palette values and five fixed account colours are pinned by tests and remain open for review against the apps' colours.

Team live spaces use protocol 3 only:

- The upgrade and `hello` each check the role, and `welcome_space` carries it. `update`, `create`, `create_document` and the compaction response use guarded editor writes with the transaction re-check; an archived space counts as below Editor. Loss of access closes the connection with `access_changed`, and a viewer receives `read_only`. `subscribe` and `subscribe_log` need viewer. Viewers are never asked to compact.
- A protocol 2 `hello` for a team space is answered with `protocol_mismatch`; personal protocol 2 remains unchanged. In P2-1 the transitional server negotiation answered at most 2 while the agent asked for 2; from P2-3 the agent asks `{1, 3}` on all links. A protocol 3 personal link gets `welcome_space {role: owner}`. A team link must receive `welcome_space` for its own space, otherwise it is a protocol mismatch.
- A `404` on a team live link triggers a space-list look immediately and a retry in 10 minutes; file sync is unaffected.

The server's checks and limits for live connections are these:

- `PresenceNote` carries the sender's account; protocol 3 sends `presence_from {user}`, while protocol 2 keeps `presence`.
- Role and access are checked at the upgrade and at `hello`, on each write and subscription, and at each ping. A change made by another process or by the command line is found at the next ping, write or subscribe. `Limits::live_ping` defaults to 30 seconds.
- `Limits::live_recheck` defaults to 5 seconds: within that interval the device and access are checked before update pages or `presence_from` are relayed and before presence is accepted, and writes and subscribes also confirm the device. The space's member notifier looks immediately and then debounces to once per second.
- The connection registry caps 4 connections per device and space and 32 per device, per process. Disabled or removed accounts have their devices revoked, and their connections close with `unauthorized` (`this device was signed out`).

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


- One hub per synced team space has its own thread, replica, WebSocket, and `live/<space>/` documents and journals, and follows the enabling, disabling, moving and loss of sync for that space. An absolute path inside a synced team folder routes to that hub, and everything else goes to the personal hub. A file in an unsynced team space opens as `not_live {not_in_folder}`; once its hub is gone, it opens as `not_live {offline}`.
- `opened` carries the space and the `read_only` state at open time (omitted when false); a role change is not pushed into an open session. An `update` from a viewer gets `error {id, seq, code: "read_only"}` and is neither journaled nor sent, so the app's copy is ahead until it reopens. A viewer cannot create a missing live document; the plain file opens as `not_live {offline}`. An unacknowledged journal survives the loss of access: when access returns, an editor or owner uploads it immediately, while a viewer or an archived space keeps the updates waiting and logs their count by space ID only.

- `members {id, members: [...]}` goes only to apps that request identity, lists active members only, and is refreshed on the first open and at most every 5 seconds when presence from an unknown account appears. Same-machine peers get identity kind 5 only for callers that request identity; other callers receive kind 3 only.
- A blob `for=<path>` routes an absolute synced team path to its space and a relative or personal path to the personal space; any other absolute path is `400 invalid`. A team `PUT` without write access is `403 read_only`. Team blobs live under `live/<space>/blobs/`, and the retry covers every synced team space.
- A recents subscription takes an optional space. That space must be synced locally, otherwise the answer is `error not_found`. Entries name their space, and an absolute team path adds an entry relative to the root.

### 7.4 The kit's API impact

Goal: an app needs no change to co-edit across accounts, and at most one line to show who is there and to keep
binary values in team documents.

| What | Change for apps |
|---|---|
| Opening, editing, undo, saving a document in a team folder | **none**: the app passes the file's path as it does today; the agent routes by path |
| A viewer opening a document | **none**: the kit maps `read_only` to the read-only open it already has for documents of another app or a newer schema |
| Showing who is there | optional: new `session.people() -> Vec<Person {name, colour, peers}>` (additive), grouping `session.peers()` by account. `craft-cloud-egui`'s badge shows the people without app code |
| Binary values (`{$chunk, $len}`) in team documents | **one line**: build the blob store with the new `session.blobs()` (an `AgentBlobs` bound to the session's path) instead of `AgentBlobs(Blobs::new(..))`. An app that keeps the old constructor stores team blobs in the personal space, where other members get `blob_not_found`; the app calls `Session::check_blobs(&store)`, which records a warning once in `Session::warnings()` and returns false (the kit has no logging) |
| Presence bytes | **none**: the kit asks for `identity: true` itself and keeps handing apps their own bytes |

`Peer` keeps its fields (adding a public field would break apps that construct it in tests); identity lives in the
new `Person` type.


The following details apply to the kit:

- `Person` is `{name, colour: PaletteColour, is_self, peers: Vec<u64>}`; `peers` are Loro peer IDs, not a count. `session.people()` puts the user first once the member list has arrived, then the active members sorted by name who have presence within the last 10 seconds. The user's own peers are the first person's peers; unknown accounts are omitted, and without a member list the result is empty. The kit asks for `identity: true` on every open and reopen and maps the account identity internally; `session.peers()` remains the app-byte peers.
- A viewer is distinct: `Status::viewer` and `KitError::ViewOnly`; `Status::read_only` remains the path for newer apps. Both follow the same read-only behavior: the badge shows Read-only, there is no undo kind, undo, redo, change and save point are refused (`NoRecord`), and there is no send queue. A refused update is not retried; pending unsent changes remain queued, and later changes are refused locally.
- `session.blobs()` returns `Result<AgentBlobs, KitError>`. The path binding comes from `Blobs::for_path(path)` and `bound_path()`, and put and get use `put_for` and `get_for`. `Session::check_blobs(&store)` records one warning in `Session::warnings()` and returns false for an unbound team store; the kit has no logging. `BlobStore::space_bound()` defaults to `None`, and the team status is fetched once from `/v1/spaces`.
- `people_badge` shows up to 4 initials and `+N`, in the palette's theme colour, with hover names marked `(you)`; an empty list draws nothing. The existing badge is unchanged except for the viewer tooltip reason. The tests of the fake hub provide team members, account-stamped presence, viewer refusal, team-scoped `?for=` blobs and the spaces endpoint.

### 7.5 Limits per space

The live service's caps move from "per account" to "per space", with the same numbers, for team spaces:

| Cap | Today (per account) | Team space |
|---|---|---|
| Live bytes | `live_bytes_per_user`, 1 GiB | `[spaces] team_live_bytes`, default 4 GiB (Q9) |
| Documents of files | 65,536, of which 10,000 hold updates | same, per space |
| Recents documents holding updates | 64 | same, per space |
| Log above the snapshot per document | 16 MiB | same |
| Presence | 4 KiB, 20 per second per document per device | same |
| Subscribed documents | 256 per device | 256 per device and space, and 1024 per device in total (`MAX_SUBSCRIPTIONS_PER_SPACE`, `MAX_SUBSCRIPTIONS_PER_DEVICE_TOTAL`); the agent's own bound remains 256 |
| Connections | 4 per device | 4 per device and space, 32 per device |

## 8. Share links and web file access

Phase P4. Large, and the first time files are served to a browser; it can be left out entirely (Q11).

- **What it is**: a read-only link to one file or one folder of a team space (personal spaces too, if Q2 permits),
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

- **Shared tab** (P1): lists team spaces only; the personal space remains the overview. Team rows show the name, role and size, and a sync switch for this computer (off by default, with the size shown). Each row opens the space.
- **Invitations** (P1): a banner and a section at the top of the Shared tab: "You're invited to <space> as <role>", with Accept and Decline. Accepting asks whether to sync on this computer and where. The inviter's name is not shown.
- **A space's page** (P1): the folder location (open, change), the sync status, usage against the quota, the members, and Leave.
- **Members** (P1): usernames, roles and pending invitations. Owners can invite by account name, change roles and remove members, with a confirmation that says the person keeps what they already downloaded. The user's own row has no role picker or Remove.
- **"Make a team space from this folder"** (P1-8, administrators only): from a top-level folder in the "On this computer" list on the home page. It asks for the administrator's password and second factor, and the administrator's account becomes the first owner.
- **Admin pages** (P1): Team spaces: create, list, archive, quota and owner. The first owner is looked up before the confirmation, and the limit is entered in MiB (minimum 4). The page shows counts and sizes, not the member list.
- **Live** (P2): the overview's recent files show the people editing a live document, from the member and presence data.
- **Libraries** (P3), **links** (P4), **comments** (P5), **locks** (P6): each adds its own page or column then.

Every screen gets kittest snapshots in `apps/craft-cloud-desktop/tests/snapshots/`, in light and dark.


The following details apply to the desktop app:

- The Shared tab follows Files and lists team spaces only; the personal overview is separate. The invitation text is `You're invited to <space> as <role>`, with no inviter name. The invitation count appears on the Shared header and on the overview tag. It is loaded on the account check, when the tab opens and on Refresh, and is not polled.
- Per-space usage and members are loaded when the space is opened. List usage is loaded once when the tab opens or on Refresh, sequentially on one thread. A change of account resets the tab and discards in-flight answers.
- An owner's own member row has no role picker or Remove; Leave handles it. Rename is owner-only and unavailable for archived or lost spaces. A retry after a `409` is tied to its space and action and is discarded on any click, navigation or error.
- When an administrator creates a space, the owner is looked up by exact, case-insensitive name before the confirmation; the quota is entered in MiB, with a minimum of 4. The admin space page shows counts and sizes, not member lists.
- The from-folder action appears on the rows of top-level personal folders, for administrators only. It asks for the administrator's password and a TOTP or recovery code, and the administrator's account becomes the first owner.

The held-delete UI calls `/approve-deletes` with no set to fetch the waiting set: `{set,total,truncated,entries[{path,side}],held_set,held_total,held[{path,links}]}`. An old agent omits these optional wire fields. The limits are 1,000 entries and 512 KiB of paths, and 100 held entries; the command line message is capped at 16 KiB in the local API. The UI shows the counts and the path list and sends the returned set name to approve exactly the listed set; a changed set is listed again. The desktop app never approves deletions held by links; the command line names the set. The agent's status does not describe held team-space deletions. The set `look` is a non-waiting sentinel that is used to fetch the data.

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
| **P1-1** meta: schema | migration 0016 for SQLite and Postgres (after 0015 `physical_size`; SQLite rebuild with runner's `foreign-keys-off` marker); `space_members`; `SpaceRole` in `craft-cloud-api`; `Meta::space_access`, `create_team_space`, member functions (invite, accept, decline, change role, remove, leave, list), `last_owner` rule in the transaction; `last_owner` error code; new audit names | migration fingerprint test on a filled 0015 database (both backends); `foreign_key_check` empty; constraint tests; access matrix (personal owner, accepted member per role, pending, archived, disabled and removed accounts, other personal space); `last_owner` under concurrent role changes (two tasks, Postgres and SQLite) |
| **P1-2** server: space layer | `craft-space` header parsing and echo; `require_space` layer; every space-scoped handler takes `SpaceAccess`; role gates and in-transaction re-check; `read_only` code; the exact UUID header and the echo status rules; protocol 2 in `/api/info` | table-driven test of every route × {none, viewer, editor, owner} × {personal, team}; no header is byte-for-byte the old behaviour (the existing server suite runs unchanged); a chunk of space A not readable through space B's header even when the store has it; a long-poll waiting on a team space answers 404 within 1 s of the member's removal; a commit racing a role downgrade is refused; grep test: no handler reads `auth.space` |
| **P1-3** server: routes and CLI | `/spaces…`, `/admin/spaces…` routes, rate limit on invitations, audit events, `space` CLI commands, `reset-epoch --space`, `docs/server.md` section | route tests for every success and error code; audit rows written once, none on a repeat; one answer for unknown, disabled and removed invitees; admin confirmation required; CLI tests |
| **P1-4** sync engine: read-only mode | `Transport` bound to a space; the read-only rules of [5.4](#54-read-only-spaces) | scenario tests: viewer edit kept and never uploaded; server change over a viewer edit makes a local-only conflict copy; local delete is downloaded again; role raised uploads local-only files; dedicated seeded viewer simulation in `crates/sync/tests/read_only.rs`; agent turmoil simulation covers personal space only |
| **P1-5** agent: several spaces | `account.json` `spaces`, per-space state under `spaces/<id>/`, one engine per synced space, scheduler, per-space long-poll, echo check, losing access ([5.6](#56-losing-access)), local API `/v1/spaces…`, status | the agent's loopback-server test `apps/craft-cloud-agent/tests/team_spaces.rs`: two accounts, invitations and editing both ways, viewer, removal/re-add, rename and folder clashes; turmoil simulation covers personal space only; an old server (protocol 1) shows team spaces unavailable and sends no header; a missing echo is refused; the existing Phase 1 data folder starts unchanged |
| **P1-6** app client and desktop UI | `craft-cloud-app` calls for spaces and members; Shared tab, invitations, space page, members, admin Team spaces page | model tests for every state; kittest snapshots of every new screen (light and dark); agent-too-old path |
| **P1-7** web admin page | Team spaces list/search/detail and admin forms in `/web`; owners retain member changes and rename | CSRF, Origin, confirmation, validation-before-confirmation and statuses |
| **P1-8** claim and "team space from folder" | `POST /chunks/claim`; the desktop flow | a claim requires an editor role in the target and a readable source; a quota failure is atomic; no bytes move; the deletion of the from-folder source revalidates the server tree and each file, with quarantine and recovery |

Rough size: large. About 7 PRs, of which P1-2 and P1-5 are the biggest; on the order of 8,000 to 12,000 lines
including tests. Risks: the SQLite table rebuild (mitigated by the fingerprint and foreign-key tests); missing one
handler when switching to `SpaceAccess` (mitigated by the grep test and the route matrix); agent scheduling across
spaces changing simulation determinism (passes stay sequential).

### P2: co-editing across accounts and presence identity

| PR | Content | Acceptance tests |
|---|---|---|
| **P2-1** proto | live protocol 3; `WelcomeSpace`, `PresenceFrom` appended; `member_colour`, palette, and BLAKE3 in `craft-cloud-api` | golden bytes of every protocol 2 frame unchanged; round-trip of the new frames; the transitional agent and server negotiation remains capped at protocol 2 until P2-3 |
| **P2-2** server live | space from the upgrade header; role per frame; viewers never asked to compact; presence stamped; caps per space; connection registry and revocation | two accounts edit one document and converge (both backends); viewer writes refused with `read_only`; `subscribe` and `presence` allowed; protocol 2 team link rejected; per-space role checks and revocation; `PresenceFrom.user` is the sender's account whatever the bytes say; removed member's socket closed within 1 s in-process and at the next ping from another process (simulated); a protocol 2 agent without the header sees byte-identical frames |
| **P2-3** agent live | one link per team space; routing by path; `identity` and kind 5; `members` message; `read_only` in `opened`; blobs `for`; recents per space | documents convergence simulation with two accounts and a viewer; journals with unacked updates survive revocation; an app that does not ask for identity receives only kind 3; blob put with `for` lands in the team space and another member's agent reads it; recents of a team space reach another member |
| **P2-4** kit, app, egui | `Session::people()`, `session.blobs()`, read-only from `opened`, badge with people, `Blobs::for_path` | fake-hub tests with teams and two accounts; a viewer maps to `Status::viewer` and `KitError::ViewOnly`; identity and the people badge; the blob binding check records a warning in `Session::warnings()` |
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


The acceptance of P1-4 is a dedicated seeded viewer simulation in `crates/sync/tests/read_only.rs`; the agent's turmoil simulation covers the personal space only. The two-account team-space coverage of P1-5 is `apps/craft-cloud-agent/tests/team_spaces.rs`, which runs against a real loopback server, not turmoil. It covers invitations and editing in both directions, a viewer, removal and re-adding, renaming, folder clashes, a missing echo, protocol 1 and an existing Phase 1 data folder. The acceptance of P1-8 includes the claim's access, quota and idempotence checks and the guarded from-folder flow.

The following details apply to the P2 pull requests:

- P2-1: the golden bytes of protocol 2 stay unchanged, the new frames round-trip, and the known palette colours are pinned.
- P2-2 verifies the role gates and rechecks, the live caps and the revocation.
- P2-3: the agent supports `{1, 3}`, team hubs, path routing, identity, read-only mode, blobs and recents.
- P2-4: the fake hub gains team, member and read-only helpers and blob routing, and the kit demo tests two windows across accounts.
- P2-5: the desktop change is people on the recents.
- Team live caps: `[spaces] team_live_bytes` defaults to 4 GiB; `[spaces]` has only this key, and `team_stored_bytes` is not implemented. The existing document, log and presence caps are per space; the subscription cap is 256 per device and space plus 1024 per device in total.
- The reset-doc invocation for a team keeps the required `<user>`, who must be a member. A reset through `--space` of an archived space requires `--force` and writes `cli.live_reset` in the transaction (`target_kind=space`, the target ID, no details and no actor); a personal reset remains unaudited. The closed audit list has 45 names. `MAX_MISSING_IDS` is 10,000.

## Decisions needed from the maintainers

| # | Question | Proposed default |
|---|---|---|
| Q1 | Who may create team spaces: administrators only, or every account (each creator becoming owner)? | administrators only |
| Q2 | Is folder sharing out of a personal space wanted at all, as "make a team space from this folder" (P1-8)? | yes, implemented as an administrator-only flow |
| Q3 | Default limit for a team space's stored bytes (`[spaces] team_stored_bytes`), and whether per-space retention overrides are needed | no limit by default; `team_stored_bytes` and per-space retention overrides are not implemented |
| Q4 | Should a team space's recents be shared with all members? | yes; entries carry no account identity |
| Q5 | Local layout: team folders beside the personal folder (`~/Craft Cloud Shared/<name>/`) or nested (`~/Craft Cloud/Shared/<name>/`)? | beside, with `Craft Cloud Shared` beside the chosen personal folder |
| Q6 | Accept the SQLite table rebuild of `spaces` with a `foreign-keys-off` migration marker? | yes |
| Q7 | Should administrators have implicit read access to team spaces? | no; they add themselves, audited |
| Q8 | Should memberships made by an administrator skip the invitee's acceptance? | no, except the first owner named at creation |
| Q9 | Live bytes cap for team spaces (`[spaces] team_live_bytes`) | 4 GiB |
| Q10 | Comments as a live document per file rather than a table | live document |
| Q11 | Are share links and web file reading (P4) in scope for Phase 2 at all, and should member web sign-in be a phase of its own? | in scope, last; member sign-in split out if P4 grows |
| Q12 | Is the one-line blob change for apps with binary values in team documents acceptable, or must the agent place blobs without app help? | acceptable; the session blob API binds by document path |
| Q13 | Wording of the font licensing note in the app and docs, and whether a font library should require a licence per item | the `licence` field is optional; the app shows a fixed note |
