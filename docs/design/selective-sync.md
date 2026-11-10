# Selective sync and bandwidth limits

This document expands selective sync and per-agent bandwidth controls left for later in [sync.md](sync.md). It describes proposed behavior and APIs; these features are not present in the current agent or desktop app. Selection is per configured space root, so it applies independently to personal and team spaces.

## Decisions

- **Decided (recommended):** exclusion means cloud-only, not deletion. The server tree remains authoritative for excluded content; exclusion never commits a delete.
- **Decided (recommended):** excluded folders have no placeholder files in the first release. The agent reports cloud-only folders through status and the desktop picker. Placeholders require platform-specific filesystem integration and a separate design.
- **Decided (recommended):** local changes discovered under excluded folders are preserved locally, not uploaded, overwritten or deleted. They are reported as local changes requiring an explicit choice to include the folder or move the files into an included folder.
- **Decided (recommended):** upload and download limits are per agent data directory and apply to file-sync HTTP transfers. Live WebSocket traffic uses a separate reserved path and is never delayed behind bulk transfers.
- No change to sync conflict, chunk verification, atomic writes, quota, or mass-delete guarantees.

## Model and saved state

Add `selective_sync` configuration to each saved space state, not to the server. Persist in the agent's versioned state file under the space record:

```json
{"selection_version":1,"excluded":["Projects/Archive","Media/Raw"]}
```

Paths are relative to space root, `/` separated and normalized with the same path rules as synced nodes. Root cannot be excluded. Exclusions are directory node IDs where known plus normalized paths as a recovery hint; stable node ID wins after a rename or move. If an excluded directory is deleted remotely, remove its rule after confirmed tree reconciliation. If it is moved into or out of an excluded ancestor, recompute effective selection from the current tree. A directory is excluded if itself or any ancestor is selected. New children inherit parent selection.

Add per-agent optional rates in the same private config:

```json
{"bandwidth":{"upload_bytes_per_second":null,"download_bytes_per_second":null}}
```

`null` means unlimited; `0` is invalid; positive values are bytes/second. Persist using atomic owner-only writes. Config corruption follows existing damaged-state behavior: stop safely and report repair guidance, never reset user selection silently. Unknown future fields are preserved or ignored according to existing state schema policy; schema migration must be explicit and tested.

A one-time migration creates an empty exclusion list and unlimited bandwidth. Existing data roots and files remain unchanged. A new device starts with all folders included unless the user selects cloud-only folders during join/adopt setup. The proposed default large-folder threshold reuses sync.md's 5 GB direction, but only suggests exclusions; it never excludes without user choice.

## Agent and sync engine behavior

Tree metadata and change-feed cursors continue to cover the whole space. Sync engine receives an exclusion predicate per node. Excluded subtrees skip chunk downloads, local directory creation and file materialization. The agent still retains enough node metadata to list cloud-only folders, discover later rule changes and detect conflicts. It must not fetch file manifests or chunk bytes for excluded files unless required for bounded metadata decisions.

On changing synced to cloud-only, first reconcile outstanding local changes. The agent refuses transition while pending upload, conflict copy, held delete or unconfirmed server change exists in that subtree. The UI lists affected paths and asks the user to sync first, resolve conflicts, or cancel. Once settled, persist the new rule atomically, then remove local files with the existing atomic/safe deletion rules only after confirming each file's current version is present on the server. A mass-delete guard can still stop this removal; no automatic approval.

On changing cloud-only to synced, persist intent, download missing content through the ordinary verified chunk path, then report complete. Interrupted downloads resume safely. Existing local path collision uses the ordinary conflict rules: never overwrite a local file; preserve a local-only file as a conflict copy and report it. Until all files arrive, directory status says downloading.

Local changes under excluded paths are detected by the watcher and compared with saved local fingerprints. They are not queued as commits and do not enter the upload scheduler. The agent reports them as `excluded_local_changes` with paths and a stable action token. They remain untouched on disk. If server version also changed, the agent leaves both local bytes and server version intact; when the user re-includes the folder, reconcile under normal conflict-copy rules. A local delete in an excluded path is not uploaded; if the user wants it deleted from cloud, they must include the folder and issue a normal delete, subject to delete approval.

The mass-delete guard counts only actual local or remote deletions performed during reconciliation, not nodes omitted because excluded. Turning a large folder cloud-only still requests deletion of local materialized files and is subject to existing thresholds and approval. The approval set must bind to the exact selection revision; a changed selection or tree makes the old set stale and approves nothing. No exclusion rule can satisfy or bypass `--approve-deletes`.

Excluded folders in team spaces are per device and do not change membership, server visibility, another member's selection, or shared quota. A member may exclude a team-space folder even when read-only. A missing local folder for an excluded subtree is expected, not a lost-root condition.

“Make a team space from this folder” is refused if the selected source folder or any descendant has active excluded local changes, pending transfers, unresolved conflicts, or cloud-only descendants: the operation requires a complete, reconciled source. The confirmation lists blockers. After full materialization, the existing atomic claim operation applies. An excluded subtree is never silently omitted from the new team space.

### Transport bandwidth scheduling

Upload and download token buckets operate at the agent HTTP transport, counting request and response body bytes for chunks and manifests. A configured limit is a sustained bytes/second rate with a small bounded burst (recommended 1 second of rate, capped at 8 MiB). Shared bucket across spaces, not one bucket per space, so team spaces cannot multiply configured bandwidth. Control requests, tree/change polling and auth are not throttled; their bodies remain under existing caps. Retries count bytes actually transferred, including failed partial bodies.

The live WebSocket has its own writer and reader and does not wait for HTTP tokens. Bulk file transfers yield between chunks and use bounded concurrency. When a limit changes while running, token buckets update without restart. Pause sync stops file transfers but leaves live sessions and presence connected. A separate “pause all live data” control is not included.

## Local API, CLI and app API

Proposed local API additions, authenticated and origin-checked like existing `/v1` routes:

- `GET /v1/spaces/{space}/sync-settings` → `{excluded:[{node,path}],upload_bytes_per_second,download_bytes_per_second}`.
- `PUT /v1/spaces/{space}/sync-settings` body `{excluded:[{node,path}],upload_bytes_per_second,download_bytes_per_second}` → `200` normalized state, or `409 pending_changes` with `{paths:[...]}`. Invalid paths/rates return `400 invalid`; unavailable space returns existing `404 not_found`.
- `GET /v1/spaces/{space}/sync-status` includes each directory's `synced|cloud_only|downloading`, pending excluded local paths, bytes queued, current rates and recent measured throughput. Existing fields remain optional-compatible.
- `POST /v1/spaces/{space}/sync-settings/approve-deletes` body `{set}` uses existing exact-set approval semantics; stale sets approve nothing and return current list.

CLI proposal: `craft-cloud-agent sync exclude <space> <path>` and `sync include <space> <path>`; `sync settings` lists rules and pending local changes. Preserve existing bare `sync` and `sync --approve-deletes[=<set>]` behavior. Rate flags/configuration: `craft-cloud-agent bandwidth show` and `bandwidth set --upload <bytes-per-second|unlimited> --download <bytes-per-second|unlimited>`. Values accept explicit `B/s`, `KiB/s`, `MiB/s` suffixes; bare integers mean bytes/second. Both settings are optional. Commands go through the running daemon, as other mutating commands do; settings persist across restart.

`craft-cloud-app` adds list/set selection and get/set bandwidth methods. No `craft-cloud-kit` change is required: apps receive local file paths and must handle missing cloud-only paths through app-level file selection. Old app clients ignore new optional status fields; old agents ignore new local routes and retain their current full-sync behavior. No server route or protocol negotiation changes.

## Desktop UI and launcher

The Craft Cloud desktop app adds a per-space Sync folders page. It shows folder tree, remote logical size, current state and a synced/cloud-only control. Before exclusion, show pending paths, require confirmation, then use existing mass-delete approval if threshold is crossed. During inclusion, show progress and allow cancel; cancellation leaves already downloaded bytes and the rule in a safe, resumable state. Do not suggest placeholders.

Bandwidth controls live in Settings and show upload/download limits independently, including Unlimited, current throughput and queued bytes. Validation rejects zero and malformed or overflowing values. Changes apply immediately. If the agent is older and local route returns 404, disable controls and show “Update the agent to manage sync folders and bandwidth.”

Launcher integration is a separate PR to `storytold/craft-launcher`, following the existing sync.md direction. The Craft Cloud app remains source of truth for rules and agent RPC; launcher must not edit the agent state file directly. Launcher shows folder state and limits only after agent capability detection. It must not hide the same mass-delete confirmation or local-change warning.

## Edge cases and security

- Exclusion path validation rejects absolute paths, `..`, drive letters, NUL, reserved names and symlink escapes. The agent resolves node IDs from its own server tree, never trusts arbitrary local API paths.
- Concurrent rule changes serialize through the daemon. Each operation has a selection revision; stale delete approvals are refused.
- A rename preserves exclusion by node ID. A move from excluded into included downloads the subtree; move into excluded stops materialization only after pending local edits are reconciled. Never delete an unuploaded local edit.
- Server restore, epoch reset, backup restore and missing root marker retain selection rules. Reconciliation follows sync.md recovery rules and does not turn an excluded remote deletion into local delete approval.
- A device removed from a team space keeps its local files and config as existing losing-access behavior requires; exclusion does not erase them.
- Limits are local controls, not a security boundary. No bandwidth value is sent to the server. Enforce arithmetic bounds and reject values that overflow token bucket duration math.
- Throttling must not block live keepalives, presence, local API responsiveness or shutdown. HTTP scheduler cannot hold a global lock while waiting for tokens.

## Tests and acceptance

- State migration: old state loads as all included and unlimited; atomic round trip; corrupt state reports repair action and preserves source.
- Exclusion scenarios: remote unchanged file excluded without download; local dirty file blocks exclusion; local edit after exclusion remains untouched and unsent; local delete unsent; server update while excluded preserved; include downloads and conflict-copies collisions; folder rename preserves node rule; move across boundary changes materialization; nested include cannot override excluded ancestor unless explicitly specified by the model.
- Team space scenarios: per-device selection differs; viewer exclusion works; removal keeps local data; excluded descendants block make-team-space-from-folder.
- Guard scenarios: exclusion above threshold requires exact set; stale set, changed selection and changed tree approve nothing; excluded omission itself never counts as a server delete.
- Bandwidth scenarios: sustained upload and download rates within burst tolerance; aggregate across spaces; changing limit while active; retries charged; zero rejected; huge values bounded; control calls responsive; live updates and presence remain responsive during saturated transfers; pause sync leaves live connected.
- Compatibility scenarios: old app tolerates additional status fields; old agent remains all-included and full-sync; new desktop identifies unsupported local API without mutating files.

## Pull request plan

| PR | Scope | Acceptance |
|---|---|---|
| S1 | Finalize state schema, selection model and engine predicate | Migration tests; exclusion/inclusion simulations; no network or disk deletion for excluded remote entries |
| S2 | Local API and agent CLI selection commands | Auth/Origin; pending changes refusal; stale delete-set rejection; daemon serialization; old state compatibility |
| S3 | Desktop per-space picker and mass-delete flow | Snapshots; local-change and conflict blockers; exact approval; partial downloads resumable |
| S4 | Agent transport token buckets and bandwidth CLI/local API | Upload/download rate tests; fairness across spaces; control and live traffic responsive |
| S5 | Desktop bandwidth controls and status | Validation; immediate updates; persisted settings; older agent feature detection |
| S6 | Launcher integration, separate repository PR | Capability detection; same confirmation and warnings; launcher does not edit state directly |

## Open questions

- **Q1:** Should selection rules be node-ID-only with display path cache, or retain path hints for future server tree reinitialization? Recommendation: node ID plus path hint, as specified.
- **Q2:** Should cloud-only changes appear as notifications in status by default, or only after local edits occur? Recommendation: report local edits prominently; remote metadata changes remain quiet.
- **Q3:** Should folder inclusion support cancel after downloads begin? Recommendation: yes, leave the folder marked downloading and safely resumable until user removes the rule.
- **Q4:** Do maintainers want future placeholders as a separate platform-specific phase? Recommendation: defer; do not add them to first release.
- **Q5:** Which CLI rate units should be canonical? Recommendation: explicit suffixes plus bare bytes/second, as proposed.
- **Q6:** Does `--approve-deletes` need a distinct confirmation marker for deliberate cloud-only transitions? Recommendation: existing exact set is sufficient if it names the purpose and binds selection revision.
