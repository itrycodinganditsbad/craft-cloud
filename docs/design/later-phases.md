# Later phases: libraries, links, comments and locks

This document expands P3 to P6 from [collaboration.md](collaboration.md). It is a proposal for later pull requests. P1 and P2 remain prerequisites where stated. Names and routes explicitly called proposed are not present in the current code. Existing HTTP routes are under `/api/v1`; agents access their local service on `/v1`.

## Metadata backends

A possible MariaDB metadata backend, scope decision, dialect mapping, migration safety, code estimate, CI and open questions: [mysql.md](mysql.md).

## Decisions and changes to earlier decisions

- **Decided (recommended):** libraries remain ordinary files and manifests. No library database tables, special chunk ownership or server-side interpretation. This follows collaboration.md §9 and keeps history, conflict copies, restore and quota behavior uniform.
- **Decided (recommended):** share tokens are 32 random bytes encoded as unpadded base64url, shown once, and looked up by BLAKE3 hash. Token-bearing URLs use `/s/{token}`. Tokens are bearer credentials and must not enter logs, analytics or referrers.
- **Decided (recommended):** member web sessions and public link access are separate. A public link grants access only to its subtree; a member session uses ordinary space roles. Member browsing is split into its own PR within P4 if it delays public links.
- **Decided (recommended):** comments use the existing live service with an appended document kind. Server checks membership and viewer write exception by document kind; it does not parse Loro content or enforce comment authorship.
- **Decided (recommended):** locks are advisory. They never reject an old protocol client's commit by default; a newer agent receives an explicit `locked` conflict and preserves its local edit as a conflict copy. This resolves collaboration.md §11's ambiguity about enforcement: the server rejects a conflicting commit only when request advertises lock-aware capability; old requests retain current conflict behavior.
- No P1 or P2 decision changes. P4's draft route `GET /s/{token}/file/{node}` is refined below with bounded streaming and range support. P6's enforcement is clarified above without changing lock lifetime, roles or conflict-copy safety.

## P3: Libraries

### Model and manifest

A library is a directory in a synced personal or team space. No migration. Proposed manifest `library.json`, format 1:

```json
{"format":1,"kind":"swatches","name":"Studio palette","items":[{"id":"stable-id","name":"Warm","file":"warm.json","tags":["print"],"licence":"CC0-1.0"}]}
```

Kinds are the closed set `swatches`, `fonts`, `presets`, `symbols`. The manifest node is `Libraries/<kind>/library.json`; item files are siblings under that directory. `id` is unique within one manifest, 1–128 ASCII characters matching `[A-Za-z0-9._-]+`; `file` is one normalized direct-child filename, not a path. Names are NFC, nonempty, at most 128 characters. Tags are optional, at most 32 values of at most 64 characters. Manifest is UTF-8 JSON, at most 256 KiB, at most 2,000 items. Unknown fields are ignored; unknown format or kind is skipped with a diagnostic. Invalid item entries are skipped individually; malformed JSON rejects that manifest. These limits are proposed and must be shared with app maintainers before implementation.

Manifest and assets are ordinary versioned files. Concurrent manifest edits may conflict-copy; apps must not silently select a conflict copy. Library discovery uses only locally synced files and never downloads cloud-only content. Fonts are not bundled, fetched, scanned or licence-validated by Craft Cloud. `licence` is descriptive metadata only.

### Agent, local API, kit and app API

The agent needs no wire-protocol change. Add local `GET /v1/libraries?kind={kind}`. Proposed response:

```json
{"libraries":[{"space":"<space-id>","name":"Studio palette","kind":"swatches","path":"Libraries/swatches","items":[{"id":"stable-id","name":"Warm","file":"warm.json","tags":["print"],"licence":"CC0-1.0"}]}],"warnings":[]}
```

`kind` is optional; invalid values return `400 invalid`. `path` is relative to that space's configured local root. Response omits files not locally present and never returns file bytes. A bounded local read, no network call, no symlink traversal, and a 256 KiB manifest cap prevent untrusted local files from creating unbounded work. Existing local API authentication and Origin rules apply.

`craft-cloud-app` adds `libraries(kind) -> Result<Vec<Library>, Error>` and typed manifest records. The app does not link `proto`. `craft-cloud-kit` adds no API: applications continue using their own loaders on the returned paths. No user-facing claim that every app understands every kind.

### UI and security

Desktop adds Libraries view, filter by kind and space, item name, licence and Open file action. Invalid or stale items show a warning and can be refreshed. Edits remain in the owning app or file manager. No web page in P3. A team space's role still controls writes; a viewer can read locally synced assets only.

### P3 pull requests

| PR | Scope | Acceptance |
|---|---|---|
| P3-1 | Agree and document format, bounded parser in app client, manifest fixtures | Valid and malformed fixtures; limits; unknown format; traversal names rejected; invalid item skipped; fuzz/property coverage for parser bounds |
| P3-2 | Agent `/v1/libraries`, app API, desktop Libraries view | Personal and team manifests listed; kind filtering; stale/missing file warning; cloud-only omitted; local API auth enforced |
| P3-3 | App adoption guidance and compatibility notes | At least one app integration review; old agent returns ordinary 404 and app reports feature unavailable |

## P4: Share links and web file access

### Data and migration

Migration 0017 is reserved for `share_links` in SQLite and Postgres. Proposed columns: `id` UUID primary key; `space_id`, `node_id`; `token_hash` 32-byte BLOB/BYTEA unique; `created_by` user ID; `created_ms`, `expires_ms`; nullable `revoked_ms`; nullable `max_downloads`; `downloads` integer default 0. Foreign keys use the repository's existing deletion policy; link creation is refused for trashed, missing or archived nodes/spaces. Index `(space_id,node_id)` and unique `token_hash`. Migration creates no plaintext token and preserves existing rows/tables. Expired rows may be retained for audit; revocation is permanent.

A file link exposes one file. A folder link exposes that folder and descendants, direct children listing only, with bounded pagination. Link grants no history, trash, search, live documents, writes or metadata above its node. Directory traversal and node IDs are checked against current ancestry on every request, so a move outside the shared subtree removes access. If the link's root is moved, link stays attached to its node.

### Routes and shapes

Member routes, under `/api/v1/spaces/{space}/links`:

- `POST` JSON `{node, expires_ms?, max_downloads?}` → `201 {id, url, token, expires_ms, max_downloads}`. Token returned only on create. Default expiry 7 days; maximum 30 days. Download maximum optional and positive.
- `GET` → `{links:[{id,node,created_by,created_ms,expires_ms,revoked_ms,max_downloads,downloads,mine}]}`. Owners see all; creators see own. No token/hash.
- `DELETE /{id}` → `204`; creator or owner only; repeated revoke is 204. Audit `link.revoked` only on state change.

Editors and owners may create; viewers may not. Every request uses `require_space`. Creation audits `link.created`; audit data contains IDs and expiry, never token or URL.

Public web routes outside API:

- `GET /s/{token}` renders escaped HTML: file download page or folder listing. Query `?cursor=` optional, opaque and bounded. No script, external asset, user-controlled HTML or cacheable credential-bearing redirect. Headers include `Referrer-Policy: no-referrer`, `X-Robots-Tag: noindex, nofollow`, `Cache-Control: no-store`.
- `GET /s/{token}/file/{node}` streams bytes with `Content-Length`, `Content-Type: application/octet-stream`, safe `Content-Disposition: attachment`, and `X-Content-Type-Options: nosniff`. Proposed single byte range supported; invalid or multiple ranges return `416`. Each chunk passes store hash verification. Stream bounded memory, stop on client disconnect, never assemble a multi-GB file in RAM.

Link lookup, expiry, revocation, current node ancestry and download limit are checked on every page and file request. A download count increments atomically once when a full file response begins; HEAD and failed validation do not consume it. For range requests, count once per HTTP request; open question listed below. All invalid, expired, revoked, exhausted, inaccessible and missing-root cases render the same 404 response. No token in logs, traces, error messages, referrer or analytics.

Rate-limit public requests at 30/minute per source address and per link, with bounded in-memory state and `429 rate_limited`; reverse-proxy address trust must be explicitly configured. Apply existing request/header limits. Per-server configurable concurrent download cap and per-link download cap prevent resource exhaustion. Stream read concurrency is bounded; server quota remains charged to the space as today. Public downloads do not create another stored copy.

Member web access is separate, authenticated with existing session, CSRF, Origin and two-factor rules. Only members browse and download, according to role, and do not gain access through a link. Browser responses use safe attachment handling and no history/trash unless existing member routes explicitly authorize it.

### Old clients and UI

No agent or sync protocol change. An old agent ignores new routes; it keeps syncing normally. New desktop supports link create, copy-once display, list and revoke. It warns that anyone with URL can read until expiry/revocation. Clipboard copy is explicit. Lost token cannot be recovered; revoke and create another. Web page exposes no account details or link management.

### P4 pull requests

| PR | Scope | Acceptance |
|---|---|---|
| P4-1 | 0017 schema, metadata methods, audit | Both backends; constraints and migration preservation; token digest only; concurrent revoke/create/download limit correctness |
| P4-2 | Authenticated link management routes and desktop UI | Role matrix; token shown once; audit; expiry bounds; creator versus owner listing and revoke |
| P4-3 | Public HTML and bounded file/folder streaming | Chunk verification; subtree confinement; moved/deleted/archived/expired/revoked all same 404; large file bounded memory; disconnect; safe headers |
| P4-4 | Rate limiting, download limits, abuse controls | Address and link limits; configured proxy handling; concurrency cap; no token in logs; range accounting |
| P4-5 | Member web sign-in and browsing, only if retained in P4 scope | Session/CSRF/Origin/2FA checks; member role matrix; no access to non-member spaces |

## P5: Comments and review

### Data, protocol and permissions

No SQL migration. Append `Comments {node}` to `DocKind`; never renumber existing variants or change protocol 2 bytes. Stable document ID is `comments_doc(space,node)`. The comments document is a Loro list of thread records and replies. Each comment has stable ID, author account ID, timestamp, text, optional app-defined anchor bytes and tombstone/resolved state. Proposed limits: text 16 KiB UTF-8, anchor 4 KiB, 10,000 threads per file, and 100 comments per thread; clients validate and show over-limit content read-only. The server treats Loro payload as opaque.

The agent stamps author and timestamp from authenticated account and local clock when creating a comment. Because a malicious client can forge payloads and server cannot parse Loro, author identity is attribution, not authorization. Kit UI limits edit/delete to the author; edits remain visible in history. Viewers may append comments only. Server authorizes viewer `update` for this appended kind and retains existing viewer restrictions for all other kinds. Readers, presence and subscriptions follow P2 space membership. Live caps and rate limits remain; comment-specific notification fan-out is bounded and never e-mail.

Mentions are parsed by clients from text using exact account names, then the agent delivers local notifications only for members of that space. No global mention query or email. Notifications are best effort and deduplicated by comment ID; offline catch-up finds missed mentions. Removed members lose document access under P2 revocation.

### APIs and UI

`craft-cloud-kit` adds `CommentsSession` or equivalent typed wrapper over existing `Session`; it exposes threads, add reply, edit own, resolve and anchor callbacks. App API forwards comment events without adding a server path. Desktop and supported apps show comment panel, author, time, anchor and resolved state; app defines anchor geometry. Unsupported apps see no comments UI. Local API requires no new endpoint: live document socket carries it.

### P5 pull requests

| PR | Scope | Acceptance |
|---|---|---|
| P5-1 | Append doc kind, protocol negotiation and viewer-write authorization | Golden old frame bytes; P2 clients unchanged; viewer writes comments only; viewer writes normal docs refused; revoked member socket closes |
| P5-2 | Agent stamping, kit comments wrapper and bounded model | Offline merge; malformed/over-limit content; forged author visibly treated as untrusted attribution; edit policy tests |
| P5-3 | Desktop and first app integration, mention notification | kittest snapshots; two-account live convergence; offline mention recovery; no duplicate notification |
| P5-4 | Remaining app integrations if requested | Per-app anchors and accessibility reviewed; unsupported app behavior unchanged |

## P6: Advisory file locks

### Data and migration

Migration 0018 is reserved for `file_locks`. Columns: `space_id`, `node_id`, `user_id`, `device_id`, `acquired_ms`, `expires_ms`, nullable `note`; primary key `(space_id,node_id)`, foreign keys to spaces/nodes/users/devices. Note capped at 200 Unicode scalar values and never logged. Locks only apply to live, non-directory file nodes in active team spaces; personal space locks are not useful and are refused. Expired rows are treated as absent and lazily removed on acquire/list/release; no background cleanup required.

Lock lifetime default 8 hours, maximum 7 days. Lock is owned by one account and device. Reacquire by same account/device renews; another holder gets `409 locked` with holder display name, acquired and expiry times. Other users may release only if owner role or server admin; override is audited `lock.overridden`. Holder can release. Removing member, disabling account, deleting node or archiving space invalidates lock on access; cleanup may be lazy.

### Routes, agent and compatibility

Under `/api/v1/nodes/{id}/lock` with space header:

- `POST {ttl_seconds?, note?}` → `201 {node, holder:{user,device}, acquired_ms, expires_ms, note}`; same holder renewal → `200`.
- `GET` → `200 {lock:null|{holder:{user,device}, acquired_ms, expires_ms, note}}`; nonmember receives existing 404.
- `DELETE` → `204`; absent/expired is 204. Holder release; owner/admin override audited.

Commits touching, moving or deleting a locked node held by another account return the existing refused-commit envelope with conflict kind `locked`. Existing conflict copy path preserves local bytes. New agent advertises lock-aware protocol capability and presents specific reason. Old agent requests without capability receive ordinary conflict kind understood by existing code, preserving both copies; it is not told that the other user held a lock. Thus locks remain advisory for old agents.

Agent adds lock state to sync conflict/status and clears cached lock state on 404, expiry or space loss. Local API proposes `GET /v1/spaces/{space}/nodes/{node}/lock` and `POST`/`DELETE` equivalents only if apps need lock actions while offline; these proxy server state and return `503 unavailable` offline, never queue a lock as a file commit. `craft-cloud-app` exposes async acquire, renew, release, inspect methods. Kit adds optional `LockSession` status/events, not an automatic lease: application explicitly acquires before editing and renews while active. If renewal fails, app shows loss immediately and still saves local data.

### UI, security and tests

Desktop file details and app views show holder, start and expiry, with Acquire, Renew, Release and authorized Override. UI says advisory and explains old clients may not honor locks. No content or note in logs. Lock routes rate-limited per user and node; repeated renewals have a modest per-minute cap. Lock state reveals username only to space members. Never let a lock prevent download, live updates or local saving.

### P6 pull requests

| PR | Scope | Acceptance |
|---|---|---|
| P6-1 | 0018 schema, metadata transactions, lock routes and audit | Both backends; concurrent acquire exactly one winner; expiry and renewal; role checks; node move/delete race; no note in logs |
| P6-2 | Lock-aware commit result, agent and app/kit APIs | New client gets `locked`; old client gets ordinary conflict; both preserve local bytes; offline lock action not queued |
| P6-3 | Desktop and app indicators | Holder, expiry, renew failure, override role; snapshots and state tests; saves remain available after lock loss |

## Open questions

- **Q1:** Should P4 member sign-in and browsing ship with public links, or as separate phase? Recommendation: separate P4-5, merge only if maintainers retain it.
- **Q2:** Should personal spaces permit share-link creation? Recommendation: yes for the personal-space owner. collaboration.md §8 says “if Q2 permits”, but its Decisions needed table uses Q2 for making a team space from a folder and does not ask about personal-space links. Keep disabled until explicit approval.
- **Q3:** Should range requests consume one download each? Recommendation: yes per HTTP file request, simplest atomic limit; resumable download users may hit limits sooner.
- **Q4:** Which apps adopt each library kind and which exact item formats are stable? App maintainers must agree before P3-1 finalizes the manifest.
- **Q5:** Should comments include edit/delete after creation, or append-only replies? Recommendation: allow author edits and resolution; retain every change in live history.
- **Q6:** Should admins override locks across all spaces? Earlier text says yes; retain only with explicit audited action.
- **Q7:** Should lock conflicts reject commits from all clients or only lock-aware clients? Recommendation above preserves old behavior and makes locks advisory for old clients; confirm before P6-2.
