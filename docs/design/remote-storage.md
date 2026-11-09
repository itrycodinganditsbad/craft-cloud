# External file storage backend

> Design draft. No implementation approved. Goal: decide whether a server operator should be able to keep Craft Cloud's blobs on a rented storage box or file server they already have, reached over SFTP or WebDAV, instead of on local disk or in an S3-compatible bucket.

## Recommendation and scope

The use case is cheap capacity per terabyte. A rented storage box or file server is often much cheaper per terabyte than a local SSD or an object store, and many operators already have one. The costs are latency on every blob operation, weaker guarantees than a local file system, and a server whose availability now depends on a machine the operator does not control.

Recommended order of work:

1. **A scheduled backup to SFTP first** (see "Backups before a live backend"). It gives operators the cheap capacity for the data that matters most, and it changes no storage contract.
2. **An SFTP blob backend second**, behind a feature flag, and only after a benchmark against the kind of server the operators actually use.
3. **WebDAV over HTTPS third**, and only if operators ask for it.

Local disk and S3-compatible storage stay the supported choices until a remote backend passes the store's conformance tests and the failure tests below. Nothing in this document is implemented. Every configuration key, type and file named as new is a proposal and is marked as one.

Code references are to the newest branch that has the store's listing, size and compression work, which is the branch this design was written against. They are not all on `main` yet; check them again on the base the work starts from.

## Protocols

- **SFTP (recommended first).** It runs over SSH, so the connection is encrypted and the server can be authenticated with a pinned host key. Clients authenticate with a key pair, and an operator can restrict that key to one account and one directory. Servers are widely available. Atomic replacement of an existing file needs the `posix-rename@openssh.com` extension (see below); the plain SFTP rename is not guaranteed to replace an existing target.
- **WebDAV over HTTPS (second).** Hosting support is broad, but servers differ in how they handle conditional writes, `MOVE` with overwrite, listings and durability. It would need a certificate and host name check like any HTTPS client, and a tested definition of which `MOVE` behavior is required, before it could be offered.
- **FTPS (only on demand).** It is FTP with TLS on the control and data channels. The trade-offs are passive-port and firewall setup, certificate handling, and differences between servers in how they resume, list and rename. It adds operational work for no benefit over SFTP, so it should only be considered if a real operator cannot use SFTP.
- **Plain FTP (refused).** It sends the user name, the password and all file data unencrypted. Anyone on the network path can read the credentials and the blobs and can change data in transit. Craft Cloud will not offer it, not even behind a switch.
- **rsync and Borg (backups only).** They are backup tools. They have no per-object get, put, delete, size and ordered listing operations, and they do not publish an object atomically to a reader. They are suitable for the backup in step 1 and not as a live backend.

## The existing interface

The store's backend interface is `trait Backend` in `crates/store/src/lib.rs`. A backend moves raw bytes by path and checks nothing; `Store` does the checking. Its methods are:

| Method | Contract in the code |
|---|---|
| `kind` | A short name for the admin overview. Existing values are `disk`, `s3` and `memory`; the default is `other`. It is never a path, an address or a setting. |
| `put_raw(path, data)` | Writes the object atomically. A reader sees the old object, the new one or none, never a mix. |
| `get_raw(path)` | Returns `None` when there is no such object. |
| `get_raw_limited(path, max_bytes)` | Reads at most `max_bytes` plus one byte. The default reads everything and cuts it; backends should override it. |
| `size_raw(path)` | The stored size without reading the content, `None` when missing. The default returns a `StoreError::Backend` error, so a backend without it makes the size queries fail. |
| `delete_raw(path)` | Deleting a missing object succeeds. |
| `list_raw(prefix, after, limit)` | Paths below `prefix`, in ascending order, strictly after `after`. |

There is no separate `exists` method. `Store::object_len` returns `Option<u64>` from `size_raw`, and that is how existence is asked without reading content. A remote backend should answer it with one stat request.

`Store` (same file) wraps a backend. Its public methods are `put`, `get`, `get_sized`, `stored_size`, `object_len`, `recompress`, `sample_levels`, `delete` and `list`. It checks the BLAKE3 hash and the size limit on write and read, compresses before calling `put_raw`, and decompresses after `get_raw`. `Store::list` returns a `ListPage` with `blobs`, `foreign` (object names that are not valid blob names) and an opaque `next_cursor`; a page holds at most `MAX_LIST_PAGE_SIZE` (1000) entries. The cursor carries the last path, so a backend only needs `list_raw` with `after`.

The reference backends are `DiskBackend` in `crates/store/src/disk.rs` and `S3Backend` in `crates/store/src/s3.rs`; `crates/store/src/memory.rs` has `MemoryBackend` for tests. The disk backend writes a temporary file named `.tmp-` and a UUIDv7 in the target directory, calls `sync_all` on it, renames it over the target and syncs the directory, and removes the temporary file if the write fails. Its listing skips its own temporary names (only an exact `.tmp-<UUIDv7>` name counts as its own; any other name is reported as foreign), does not follow symbolic links, and reports names that are not UTF-8. A remote backend should follow the same rules.

`crates/store/tests/conformance.rs` is the behavior every backend has to meet. Its `conformance` function checks an empty listing, a paged listing in order with no foreign entries, a round trip for both blob kinds, a repeated write of the same bytes, a refused wrong hash and an over-size chunk, and deleting twice. Further functions there check the stored formats, recompression and levels, and damaged objects (they are errors, never data). The memory and disk tests always run. The S3 test runs only when `CRAFT_CLOUD_TEST_S3_ENDPOINT` and its companion variables are set, and fails when `CRAFT_CLOUD_REQUIRE_BACKENDS` is set and they are not. A remote backend would add a test in the same style.

## Mapping to `Backend`

Object paths come from `BlobKey::path()` in `crates/store/src/lib.rs`: `chunks/ab/cd/<64 hex characters>` or `manifests/ab/cd/<64 hex characters>`, where `ab` and `cd` are the first two bytes of the ID in lowercase hex. The two levels of directories keep each directory to at most 256 entries below the first level and avoid one huge folder, which many remote servers list slowly. The remote root must be a directory that belongs to Craft Cloud (a configured prefix), and the backend must only read and write below it.

| Operation | Proposed remote behavior |
|---|---|
| `put_raw` | Create the shard directories if needed. Write all bytes to a unique temporary name in the destination directory, using the same `.tmp-<UUIDv7>` form as `DiskBackend`. Publish with `posix-rename@openssh.com` only after the write is complete. |
| `get_raw` | Open and read the object. A missing object gives `None`. |
| `get_raw_limited` | Stop reading after `max_bytes + 1` bytes and close the file. It never buffers an unbounded remote object. |
| `size_raw` | One stat request. A missing object gives `None`. The content is not read. |
| `delete_raw` | Remove the object. A missing object is success. |
| `list_raw` | Return at most `limit` paths below `prefix`, ascending by path bytes and strictly after `after`, as relative paths below the configured root. It must not read the whole namespace for one page. |
| `kind` | A new name such as `sftp` (proposal). |

**Atomic publication.** The contract of `put_raw` is that a reader never sees a partial object. On SFTP this needs the `posix-rename@openssh.com` extension, which replaces an existing target in one step. The client must check at connection time that the server offers it. Without the extension the backend must refuse to start in the live mode, with an error that names the missing extension. It must not emulate replacement by deleting the target and then renaming, because a failure between the two steps loses the old object, and it must not claim atomicity it cannot give. A mode with immutable object names and a separate pointer would avoid the need, but it needs its own safe protocol and is out of scope. WebDAV would need an equivalent `MOVE` with overwrite whose behavior has been tested on the target server; without that it must not be enabled as a live backend.

**No fsync.** The disk backend calls `sync_all` on the file and syncs the directory. A remote protocol offers no portable equivalent. SFTP has an optional `fsync@openssh.com` extension on some servers, WebDAV has nothing comparable, and in both cases a successful reply means the server accepted the operation, not that the data survives a power loss at the provider. The design has to say so to operators: a successful write means acknowledged by the server, so the remote store needs its own backups.

**Partial writes and stale temporary files.** A dropped connection can leave a temporary file. Temporary names are unique, are never listed as blobs, and are removed only when they match the exact `.tmp-<UUIDv7>` form and are older than an age threshold (a proposal; the value needs design). A failed upload tries to remove its own temporary file, and a failure of that removal must not hide the original error. A retry after an upload with an unknown outcome writes a new temporary file and publishes it; it never resumes a half-written one.

**Case and character limits.** Blob names are lowercase hex with `/` separators, so a server that ignores case cannot make two blobs collide. The backend must still check at setup that the remote file system preserves case and accepts the path and name lengths in use (a blob path is at most 80 bytes, plus the configured root). It must not truncate or normalize a name. Server-returned names are checked before they are used: a listing entry that is not inside the root, or that has an empty segment, `..`, an absolute path or a NUL byte, is an error and not an object.

## Performance

Every blob operation costs at least one network round trip, and a put or get also moves the whole stored object. A sync that touches many small chunks therefore depends on latency, not only on bandwidth. The backend would need:

- a bounded connection pool, so a burst of requests does not open unlimited sessions;
- a limit on parallel operations per connection and in total;
- protocol pipelining where the client library and the server support it safely;
- reuse of authenticated connections, which saves the handshake but not the round trip of each request.

Providers limit concurrent connections, request rates, idle time and account quota, and the limits differ. They are **to verify per provider**; the design must not assume any fixed number, and the limits should be configurable with conservative defaults.

**Listing, inventory and garbage collection.** `apps/craft-cloud-server/src/gc.rs` currently implements only `gc --dry-run`; the command without `--dry-run` returns "deleting is not implemented yet" (`apps/craft-cloud-server/src/cli.rs`). The dry run lists both blob kinds with `Store::list` in pages of `MAX_LIST_PAGE_SIZE`, compares them with the database, and asks the store for a stored size (`Store::stored_size`) only for objects that the database does not record. A remote store therefore pays one listing round trip per page, which is acceptable, and one stat per unrecorded object, which is not if there are many. If a provider has no ordered continuation or no metadata-only size, the inventory is slow or unusable. A future deleting garbage collection must never act on an incomplete listing; the dry run already records a `listing_error` and marks the index incomplete.

**Storage tools.** `apps/craft-cloud-server/src/storage_tools.rs` has `measure`, `estimate` and `recompress`. They read blobs, and `recompress` also rewrites them, so on a remote store they transfer a lot of data. They stay available, but the documentation should say that they cost bandwidth and time on a remote backend.

**Compression.** Compression happens in `Store` before `put_raw` (`[storage] compression`, `compression_level`, `compression_min_saving`). A remote backend stores and transfers the compressed form, which reduces bandwidth and stored bytes, but not the number of requests. A level change affects new writes; `recompress` rewrites old blobs and costs a download and an upload for each one.

## Failure handling and health

- **Timeouts and retries.** Every connect, read, write and idle period has a timeout. Transient failures (a dropped connection, throttling, a temporary server error) are retried a bounded number of times with exponential backoff and jitter. Permission, authentication, path and out-of-space errors are not retried, because retrying does not fix them. Delete and read keep their documented behavior for missing objects across retries.
- **Reconnects.** After a lost connection the backend reconnects with the same bounded backoff and authenticates again. It never writes credentials to a log or an error message.
- **What admins see.** The server overview already reports `storage_kind` (`apps/craft-cloud-server/src/admin.rs`, from `Store::kind`). `apps/craft-cloud-server/src/health.rs` probes the store by asking for the size of a chunk with an all-zero ID that cannot exist. "No such object" within `PROBE_TIMEOUT` (2 seconds) means the store is reachable and its credentials and directory are right; anything else is reported as not reachable, and the answer is cached for `CACHE_FOR` (30 seconds). The probe writes nothing. A remote backend reuses this probe through `size_raw`, so it must implement `size_raw`. The probe does not prove that a write would succeed, and for a remote store that is a larger gap than for disk. Reporting an error category and the time of the last successful operation, without host, user or path, is a proposal that would need a small extension.
- **Read-only or full remote.** A write that the server refuses fails with a clear `StoreError::Backend` message, leaves the old object in place, and loses nothing. The existing settings `refuse_when_full` and `capacity` work like they do for S3: a remote file server has no portable free-space query, so `min_free` is not offered, and `refuse_when_full` would need `capacity`.

## Security

- **Where credentials live.** The `[storage]` table of the server's configuration file, as for S3. The server config already has a `Secret` type that never shows its value in `Debug` output, and for S3 it falls back to the standard `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` environment variables when the config omits them (`StorageConfig::s3` in `apps/craft-cloud-server/src/config.rs`). An SFTP or WebDAV backend should do the same: an environment variable or a key file path in place of an inline secret. Credentials never go to clients.
- **No logging of secrets.** Passwords, keys, tokens and URLs with credentials never appear in logs, error strings, `Debug` output, health responses or command output. Host names, user names and full remote paths stay out of anything shown to non-admins.
- **Host-key verification is mandatory.** The SFTP backend requires a pinned host key or a known-hosts file and refuses to connect when the key does not match. There is no accept-anything mode. WebDAV requires HTTPS with normal certificate and host name verification.
- **No path traversal.** A path is built only from `BlobKey::path()`, which is made from a 32-byte ID and a fixed directory name, so an ID cannot supply a path component. The backend also checks every path it receives against the exact `chunks/` and `manifests/` layout before it touches the network, and rejects anything else.
- **Least privilege.** Operators should use an account and key that can only reach the blob directory, and for SFTP a key that is not used elsewhere.
- **Dependencies.** An SFTP or WebDAV client library is a new dependency. It has to meet the license list in `AGENTS.md` (checked by `cargo xtask licenses`), together with all its transitive dependencies.

## Configuration sketch (proposal)

The current `[storage]` table has `kind = "disk"` with `path`, or `kind = "s3"` with `bucket`, `prefix`, `region`, `endpoint`, `access_key_id`, `secret_access_key` and `allow_http`. Both also accept `capacity`, `refuse_when_full`, `compression`, `compression_level` and `compression_min_saving`; `min_free` applies to disk only. The example in the header of `apps/craft-cloud-server/src/config.rs` documents them. The `kind` values `sftp` and `webdav` and every key with an `sftp_` or `webdav_` prefix below are **proposed names, not existing settings**, and would have to be reconciled with the config before implementation. Timeout, retry and connection-limit keys are left out on purpose; their names and safe defaults need design.

```toml
[storage]
kind = "sftp"                       # proposal
sftp_host = "files.example.org"     # proposal
sftp_port = 22                      # proposal
sftp_user = "craft-cloud"           # proposal
sftp_root = "/craft-cloud/blobs"    # proposal: the directory that belongs to Craft Cloud
sftp_key_file = "sftp-key"          # proposal: path to a private key, or taken from an environment variable
sftp_known_hosts = "known_hosts"    # proposal: the pinned host key; required
# capacity = "4 TiB"                # existing key
# compression = "zstd"              # existing key
```

```toml
[storage]
kind = "webdav"                                  # proposal
webdav_url = "https://files.example.org/dav/craft-cloud/"   # proposal: HTTPS only
webdav_user = "craft-cloud"                      # proposal
# webdav_password: from the config or an environment variable, like the S3 secret (proposal)
```

## Backups before a live backend

The simpler first deliverable is a backup. The server has no backup command today; `docs/server.md` leaves backups to the operator. The proposal is a scheduled job that copies a consistent dump of the metadata database and the blob directory to a remote server over SFTP, keeps a set number of generations, and has a documented restore drill. A documented setup with a standard backup tool such as restic is an alternative that needs less code. Either way the database has to be copied consistently (a supported dump or snapshot, not a copy of a file that is being written), and a restore must be tested, not assumed.

A backup is not the live store. Ordinary reads and writes stay on local disk or S3 and pay no remote latency, and the backend contract does not change. A live remote backend is a larger feature: every sync depends on the remote server's availability, its rename and listing behavior and the provider's limits. The backup work would still settle things the live backend needs, namely authentication, host-key pinning, monitoring and restore evidence. It would not show that remote rename, listing or performance are good enough, which is why the benchmark below comes before the backend.

## Order and estimates

All figures are rough engineering estimates for one developer, not commitments. They exclude review, release work and support for any particular provider.

| Phase | Scope | Estimate |
|---|---|---|
| (a) Backup to SFTP | Scheduled consistent database dump plus blobs, retention, restore drill, or a documented restic setup. | 2 to 4 days |
| (b) SFTP backend behind a feature flag | Backend, configuration and secrets, bounded pool, atomic rename requirement, health reporting, conformance and fault tests, one SFTP service in CI. | 8 to 14 days |
| (c) WebDAV | Separate backend and configuration, tested TLS and `MOVE` behavior, conformance and fault tests, one WebDAV service in CI. | 6 to 10 days after (b) |

Phase (a) comes first. Phase (b) starts only after a benchmark on a server of the kind operators use shows that it offers `posix-rename@openssh.com`, a usable stat and ordered listing, and enough concurrent connections. The feature flag stays until failure recovery, listing and data integrity have been shown by the tests below. Phase (c) needs real operator demand.

## Tests and CI

The proposal is to add a remote test to `crates/store/tests/conformance.rs` in the style of `s3_backend`: it reads connection settings from new `CRAFT_CLOUD_TEST_SFTP_*` environment variables (proposed names), skips when they are unset, and fails when `CRAFT_CLOUD_REQUIRE_BACKENDS` is set. CI would run it against an OpenSSH server in a container with a pinned version, and later a WebDAV server in a container. The test would cover the existing `conformance`, `formats` and `levels_and_recompress` checks plus: wrong credentials, a changed host key, atomic replacement of an existing object, case behavior, metadata sizes, ordered paging, missing objects, stale temporary files, and a restart with a new backend instance on the same data.

Fault injection would cover: a connection dropped during upload, read and rename; a slow server; a server restart; a read-only root; and a full remote disk. The assertions are that an interrupted replacement leaves the old object or the complete new one and never a partial blob, that a failed listing cannot lead to deletion, that retries are bounded, and that timeouts and reconnects are reported. A container cannot prove a provider's durability or quota behavior; those remain checks the operator makes on the real server.

## Open questions

- Which SFTP and WebDAV client libraries meet the license list, verify host keys, fit the async runtime and are maintained?
- Should SFTP require `posix-rename@openssh.com`, or also offer a slower mode that never replaces an existing object? The recommendation is to require it at first.
- How are secrets supplied (environment variable, key file) so that config validation and service management stay simple?
- Can a remote `list_raw` give a stable ordered continuation while objects are written and deleted? What consistency does a deleting garbage collection need?
- Should the inventory scan a remote store during an admin request, or run in the background and show how old it is?
- The current health probe only shows that the store is reachable. Is that enough for a remote store, or is a write check needed, and how can it avoid leaving durable test objects?
- What evidence about performance, retention and durability should a rented storage box or file server have to show before the feature is described as supported?
