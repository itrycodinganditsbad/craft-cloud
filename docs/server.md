# Running the server

Set up the server first; the agent ([`agent.md`](agent.md)) syncs folders with it.

```sh
cargo build --release -p craft-cloud-server
# Create a data directory (SQLite + local disk) and the first admin. The password is read from stdin.
printf '%s\n' 'choose a password' | ./target/release/craft-cloud-server init --dir ./cc-data --admin you
./target/release/craft-cloud-server serve --config ./cc-data/config.toml
```

The server speaks plain HTTP on `127.0.0.1:8080`. To reach it from other machines, put a TLS reverse proxy
(Caddy, nginx, Traefik …) in front of it:

1. Set `listen` to the proxy-facing address, and set `behind_proxy = true`.
2. Put the proxy's address in `trusted_proxies`. The server refuses to start behind a proxy without it,
   since every client would then share the proxy's address and a few wrong passwords would lock everyone
   out of signing in. If that is really what you want, set `ignore_forwarded_for = true` instead (and leave
   `trusted_proxies` empty: the server refuses both together).
3. Make the proxy set a request header timeout. The server cuts off request bodies that are too slow to
   arrive itself, and limits each account's uploads, so one slow device only slows its own account.

For a non-loopback `--listen`, `init` also needs `--behind-proxy` and the proxy's address, for example
`init --dir ./cc-data --admin you --listen 0.0.0.0:8080 --behind-proxy --trusted-proxy 10.0.0.2`.

For Postgres, S3 or an SFTP server, edit `[database]` and `[storage]` in `config.toml` (see "Blobs on an SFTP server").

### Storage capacity

The admin's storage overview (`GET /admin/storage`, shown by the Craft Cloud app) tells how much room the server has.
Say how much the blob store may hold with `capacity` in `[storage]`, as a size such as `"1 TiB"` or `"500 GB"`: a whole
number or one with one decimal (`"1.5 TB"`), then `B`, `KB`, `MB`, `GB`, `TB` (powers of 1000) or `KiB`, `MiB`, `GiB`, `TiB`
(powers of 1024). A plain positive integer is a number of bytes, as for `stored_bytes_per_user`. Anything else, and
zero, stops the server from starting with a message that names the setting.

```toml
[storage]
kind = "disk"
path = "/var/lib/craft-cloud/blobs"
capacity = "1 TiB"
```

- `init` makes a disk store and takes the size as `--capacity "1 TiB"` (it asks nothing: its only input is the password
  on standard input, so it can run in a script). Without `--capacity` none is written. For an S3 store, which `init` does
  not make, add `capacity` to `[storage]` by hand: S3 has no size to ask for, so the capacity you set is the only size the
  overview can show for it. A server without `capacity` starts as before and shows no capacity.
- For a disk store the overview also has the total and free bytes of the filesystem that holds the store's directory.
  For the server's own data (the SQLite file) it has the total and free bytes of its filesystem and the size of the
  database, including its `-wal` and `-shm` files. With Postgres it has none of these: the database server's disk is not
  the server's to measure. "Free" is what the server's own user may still use.
- The numbers are read when the overview is asked for, with a filesystem query each and without scanning the stored
  data. The capacity is information only: the server does not refuse uploads when the store is full or past the capacity.

`init` also makes `two-factor.key` next to `config.toml` and names it in the config (`[two_factor] key_file`). It is the key
for two-factor sign-in (below). **Back it up, apart from your database backups.**

The server compresses what it stores with zstd where that saves at least 5% (text, code and many project files
shrink a lot; photos, video and archives are stored as they are). Computers and apps never notice: every blob is
checked against its ID as before, and downloads are the original bytes. `compression = "none"` in `[storage]`
turns it off for new uploads; blobs already compressed stay compressed and are still read.

Three more settings in `[storage]` tune it:

```toml
compression = "zstd"          # or "none"
compression_level = 3         # zstd level of new writes, 1 to 19
compression_min_saving = 5    # keep a compressed blob only when it is this many percent smaller (1 to 50)
```

`compression_level` and `compression_min_saving` are checked when the server starts and refused with the line
number when they are out of range. Levels 20 to 22 are not offered: they use windows of 32 to 128 MiB, and the reader
caps a stored stream's window at 8 MiB so that a damaged object can't make the server allocate more. Chunks are at most
4 MiB and compressed one by one, so level 19's 8 MiB window already spans a whole chunk; long-distance matching or a
bigger window would find nothing more in one chunk. `compression = "none"` wins over any level: with it, no blob is
compressed whatever `compression_level` says.

**What a level costs.** Higher levels make smaller objects but compress more slowly; reading is as fast at every level
(decompression speed hardly depends on the level). Every upload of 64 KiB or more is compressed on a blocking thread of its own, so
uploads don't hold up the rest of the server. One core, 1 MiB
pieces, this repository's text, programs and a synthetic 3D mesh (`cargo test -p craft-cloud-store --release --test
level_bench -- --ignored --nocapture` reproduces the table on your machine):

| level | text and code | programs and libraries | 3D mesh buffers | bytes that don't compress |
|---|---|---|---|---|
| 1 | 71.7 % saved, 442 MiB/s | 66.0 %, 538 MiB/s | 65.2 %, 360 MiB/s | 0 %, 2261 MiB/s |
| 3 (default) | 74.7 %, 341 MiB/s | 69.3 %, 374 MiB/s | 68.9 %, 238 MiB/s | 0 %, 1404 MiB/s |
| 5 | 76.4 %, 141 MiB/s | 70.2 %, 175 MiB/s | 68.0 %, 120 MiB/s | 0 %, 803 MiB/s |
| 9 | 77.8 %, 62 MiB/s | 71.2 %, 83 MiB/s | 76.0 %, 78 MiB/s | 0 %, 380 MiB/s |
| 12 | 78.1 %, 45 MiB/s | 71.4 %, 42 MiB/s | 77.1 %, 41 MiB/s | 0 %, 417 MiB/s |
| 15 | 78.5 %, 19 MiB/s | 71.4 %, 19 MiB/s | 78.6 %, 14 MiB/s | 0 %, 138 MiB/s |
| 17 | 79.6 %, 10 MiB/s | 72.9 %, 10 MiB/s | 86.8 %, 8 MiB/s | 0 %, 24 MiB/s |
| 19 | 79.8 %, 6 MiB/s | 75.0 %, 5 MiB/s | 84.0 %, 3 MiB/s | 0 %, 19 MiB/s |

Each cell is the saving and the compression speed in MiB of plain data per second on one core of a desktop
machine that was also busy building. The "programs" are this project's own debug-info-rich libraries, which
compress better than release executables do; the "3D mesh" is synthetic. Decompression ran at 500 to 1,100 MiB/s at
every level. On about 440 GiB of real data (documents, downloads, media and project folders), level 3 saved 84 % on
text, 48 % on 3D and game assets, 33 % on programs, 23 % on PSD and PNG (PSD 49 % at level 9) and under 3 % on video,
audio and archives, which stay plain.

Level 3 (the default) compresses at 240 to 370 MiB/s per core, which is faster than most uploads arrive, and several
uploads compress on different cores at once. It gets most of what the higher levels gain: level 19 saves 1 to 5
percentage points more on text and programs for 50 to 70 times the CPU, and the incompressible bytes still cost
the full compression time before the server finds out that they don't shrink. Level 19 at 3 to 6 MiB/s per core
would make a big first sync wait on the CPU. So keep `compression_level` low for uploads and spend the slow levels
on data at rest instead, which is what the next section does. Raising it makes sense on a server that mostly receives
small, text-heavy changes and has cores to spare.

### Going further on data at rest

Three commands, run on the server's host with the same `--config`, look at and improve what is stored. They are safe
while the server runs and never delete anything.

- `craft-cloud-server store estimate [--sample 100] [--levels 1,3,9,19] --config …` takes a random sample of stored
  chunks, compresses each at every level without storing anything, and prints what each level would save (in percent
  of the data's size) and how fast it compresses. The result is kept with its date, so the admin overview can show
  "at level 19: NN % possible" without scanning anything when it is opened. It writes nothing but that one value.
- `craft-cloud-server store recompress --level 19 [--dry-run] [--limit-bytes N] --config …` rewrites stored objects
  at a higher level, only where that makes them at least 1% smaller. For each object it reads and checks the stored
  bytes against the ID, compresses them, decompresses the result and checks it against the ID again, and only then
  replaces the object (the same atomic replace uploads use). An object that cannot be read or does not check out is
  left exactly as it is and listed in the report; the command then exits with an error. `--dry-run` only reports.
  `--limit-bytes` stops after reading that many bytes of objects, to work in slices. It can be stopped at any time
  and started again: every blob it finished is recorded at its level and skipped next time, so a second run at the
  same level costs one database scan. It refuses to run when the config says `compression = "none"`.
- `craft-cloud-server store measure --config …` fills in the stored size of blobs written before sizes were tracked.
  It asks the store for each object's length (a `HEAD` on S3) and reads no content.

Recompressing while the server runs is safe because an object's ID is the hash of its plain bytes: a rewrite changes
how the same bytes are stored, never which bytes. On disk the new object is written next to the old one and renamed
over it, so a reader that has the old file open keeps reading the old bytes and the next reader gets the new ones. On S3
a `PUT` replaces the object atomically, and S3 is strongly consistent for reads after a write. If an upload of the same
chunk lands at the same moment, both writes hold the same plain bytes and the last one stays. Expect the
cost to be CPU: run it with `nice`, and in slices with `--limit-bytes` on a busy server.

The overview (`GET /api/v1/admin/storage`) shows the algorithm and level in use, the logical size of the blobs, the
physical size where it is known, how much of the data that covers, and the saving in percent. Blobs written before
this version have no recorded size until they are uploaded again, recompressed or measured, so the saving is
reported only over the blobs it is known for, and the share it covers is shown with it.

**Before going back to an older server version:** a server from before compression refuses compressed blobs, and a
blob once stored compressed stays that way (nothing converts it back, except that the same blob uploaded again
with `compression = "none"` is written plain). Going back is only possible if
`compression = "none"` was set before the first upload under the newer version. Otherwise, stay on a version that
reads compressed blobs. The same holds for recompressed objects. The database also gains columns and a table
(`blobs.physical_size`, `blobs.compress_level`, `server_state`); an older server refuses a database made by a newer
one, so going back means restoring the database from a backup taken before.

Quotas (`stored_bytes_per_user` below) count each blob at its original size, so turning compression on or off, or
recompressing, doesn't change what an account is charged.

To cap what one account may store, set `stored_bytes_per_user` (in bytes; there is no cap by default) in
`config.toml`. It counts every chunk and manifest an account uploaded once, and nothing frees space until
garbage collection arrives, including what a refused file stored before it was refused. A file that would go
over it is skipped by the agent, which says so; the rest of the folder keeps syncing. Nearly full counts as
full: with less than 4 MiB (one chunk) left, the agent pauses and says the account's storage is full.
A cap under 4 MiB is refused when the server starts, since not even one chunk would fit. An admin may override
the cap for an individual account as described below.

Signed-in devices can read `GET /api/v1/account/usage` for the space's stored bytes, the effective quota, and labeled
estimates of bytes referenced only by trash or old versions. The estimates deduplicate shared blobs per space, exclude
live-document snapshots and unrooted app assets, and are never added to stored bytes. The history estimate treats a
version an app saved (one with a save record) as kept. The effective quota is the account override when set, otherwise
the configured `stored_bytes_per_user` cap; it is `null` when neither applies. An incomplete manifest index makes both
estimates `null`.

Live documents have a cap of their own: `live_bytes_per_user` (in bytes; 1 GiB, `1_073_741_824`, by default) is
the most one account's live documents (recent-file lists and documents of files) may hold together. It counts
every update in their logs, including the ones a snapshot has replaced: those are kept until retention rules
arrive, so the total only goes down when the admin resets a document (`live reset-doc`). An update that would go
over it is refused; the agent keeps it on the machine and tries again later, and the app shows its changes as not
yet on the server. Snapshots of live documents are uploads like files' content, so they count under
`stored_bytes_per_user` instead. An account may also hold updates in at most 10,000 documents of files; the first
change to one more is refused the same way.

To add accounts: `printf '%s\n' 'their password' | craft-cloud-server user --config cc-data/config.toml add name`.
`--config` can come before or after `add`.

Or invite someone, with no password to hand over: `craft-cloud-server user --config cc-data/config.toml invite name`
prints a join string (`cloud.example.org/ABCD-EFGH-JKMN-PQRS`) and its link, good once for 7 days, to enter on their
first computer (`craft-cloud-agent join …`). It makes the account if there is none; they choose its password when
they join. For an account that exists, the invite just adds a computer, without its password. Set
`public_url = "https://cloud.example.org"` (the address computers reach the server at) at the top of `config.toml`
to have the whole join string printed; without it, `invite` prints the code and you put the address in front. A new
invite replaces the old one, and `user invite --revoke name` ends it. Every later computer joins with a code from one
that is signed in (`craft-cloud-agent add-computer`, see [`agent.md`](agent.md)). Codes that don't work are refused alike (wrong,
expired or used), and an address that tries ten in a minute is held off for the rest of it.

## Devices, passwords and signing out

An account's devices (computers and apps) each hold a token that does not expire. Accounts end them like this:

- A person changes their own password in a Craft Cloud app that offers it (`POST /api/v1/account/password`). That signs
  out every other device of the account, deletes its open invite and add-a-computer codes, and keeps the device
  that asked. Five tries (wrong passwords count) are allowed per account every 15 minutes.
- `craft-cloud-server user --config cc-data/config.toml reset-password name` sets a new password (read from standard
  input), **signs out every device of the account** and deletes its open invite and add-a-computer codes: a token that
  was stolen does not survive the reset. It prints how many devices were signed out. Add `--keep-devices` to leave the
  devices signed in (the old behaviour); the codes are deleted either way.
- `craft-cloud-server device --config cc-data/config.toml revoke-all name` signs out every device of an account
  without touching the password. `device revoke name DEVICE_ID` signs out one; `device list name` shows them.

The command line runs in its own process, so it cannot reach the server's open connections. A signed-out device's live
connection ends at its next ping (within about 30 seconds), and a change-feed long-poll within 60 seconds (when it
ends, it is answered `401 unauthorized`, never with changes); every other request is refused at once (`401
unauthorized`). Devices signed out through the API end their live connections
immediately.

## Two-factor sign-in

An account can turn on a second factor in an app that offers it: a code from an authenticator app (TOTP: Aegis, 1Password,
Google Authenticator and the like), with ten recovery codes to keep. With it on, a right password does not sign anyone in:
the app also asks for the code, and so does joining a computer with an invite or an admin's password reset code. A
computer joined with an add-a-computer code (a pairing) asks for nothing, since only a computer that is already signed in
can make one: **any signed-in computer can add more**, and turning two-factor sign-in on signs every other one out.

- **The key.** The authenticator's secret is stored only encrypted (AES-256-GCM, bound to its account), with the key in the
  file `[two_factor] key_file` names (32 random bytes; a relative path is relative to `config.toml`). `init` makes it; **the
  server never makes it** (a second server on the same database, or a restore without the key, would otherwise seal
  secrets nobody can open). **Without this key the secrets in a database backup are unusable**: keep a copy apart from
  the backups, and keep it out of everyone's reach. On Unix `init` makes it owner-only;
  **on Windows the server sets no file permissions**, so put it somewhere only the account that runs the server can read.
  The recovery codes do not depend on the key, so they still work if it is lost; the authenticator codes do not.
- **Failing closed.** When the database holds two-factor secrets, `serve` opens every one of them at start and refuses to
  run if the key is missing, unreadable, or does not open all of them; the error names the key file the config expects.
  A server without a key and without any secrets runs, and setting it up answers `503 two_factor_unavailable`. Several
  servers on one Postgres database must all have the same key.
- **Rotating the key.** A running server keeps the keys it started with, so rotate in this order: (1) make a new key file,
  put it in `key_file` and the old one in `key_file_old`; (2) **restart every server instance on the database** with that
  config (until then an instance still holds only the old key and goes on sealing new secrets with it); (3) run
  `craft-cloud-server two-factor rekey --config cc-data/config.toml` (it rewrites every secret, active and set-up, under the
  new key, one account at a time, reads each account's secrets fresh under its lock so a set-up confirmed meanwhile is not
  missed, scans again at the end, can be run again, prints only counts, and refuses without changing anything if a secret
  was sealed with a key that is neither); (4) remove `key_file_old` and restart **only after `rekey` has reported that every
  secret is under the current key** (an error that says secrets are still under another key means it is not done: restart the
  instances you missed and run it again). The server runs in the middle of a rotation, with both keys.
- **A locked-out account.** A person who lost their authenticator and their recovery codes: `craft-cloud-server user
  --config cc-data/config.toml reset-2fa name` deletes its two-factor secret, recovery codes and sign-in challenges, lifts
  any lock, **signs every device out** (it prints how many) and deletes its open invite and add-a-computer codes. They sign
  in with their password and can set it up again. Over HTTP an admin does the same with `POST /admin/users/{id}/reset-two-factor`
  (below). Both need no key.
- **Wrong codes lock for a while.** Ten wrong codes or recovery codes in a row (at sign-in, joining, turning it off,
  making new recovery codes, an admin's confirmation) lock the account's second factor for 15 minutes; each wrong one after
  that doubles the time, up to 24 hours; a right one resets it. While it lasts everything answers `429 two_factor_locked`
  with `Retry-After`. The lock is never for good, so someone who only has a password can not lock an account out forever.
  Wrong codes also count against the sign-in limit (10 per account and 30 per address in 5 minutes, then `429`).
- **Recovery codes** are ten of 16 letters and digits (shown in groups of four), each good once. Only a hash is stored.
- **Older apps.** An agent from before this release shows every `401` at sign-in as "wrong password", so an account with
  two-factor sign-in can't sign in with it (it never gets a token). Update the server first, then the Craft Cloud
  desktop package with its agent, then stand-alone agents.
- **Logs** never carry a secret, code, recovery code, challenge, password or key.

## Managing accounts over HTTP

Accounts have a role, `admin` or `member`, and the routes under `/api/v1/admin` are for admins only (the first admin is
still made by `init`). They are an API for a tool or an admin app; the server's own admin pages are an option, see "The admin
pages in the browser" below. Every change asks
for **a fresh confirmation in its body**, the acting admin's own current password (and, for an admin who has two-factor
sign-in, a `totp_code` or `recovery_code` too), because a device's token never expires and must not be enough on its own:

```sh
curl -s -X POST https://cloud.example.org/api/v1/admin/users/USER_ID/disable \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"password":"your password"}'
```

- `GET /admin/server` shows counters (accounts, signed-in devices, stored bytes, live-document bytes) and the kinds of
  database and storage, nothing else. `GET /admin/users` lists accounts 50 at a time (`limit` up to 200, `cursor` from
  the last answer's `next`, `query` to find a username).
- `GET /admin/storage` returns bounded storage totals, effective quota totals and near/over-quota counts, the ten
  largest accounts, manifest-index counts, and the room the server has (see "Storage capacity"). `GET /admin/users/{id}/usage` returns one account's stored bytes,
  effective quota, and labeled trash/history estimates; unknown and removed accounts return `404 not_found`.
- `GET /admin/storage` also has a `compression` part, and `PUT /admin/storage/compression` with `{"level": 1 to 19, "password": …}`
  sets the level new blobs are compressed at, without a restart. The config file stays the source of truth for the
  algorithm: `compression = "none"` wins over any level (the overview shows `none`, and the level an admin set is kept
  for when it is switched back). Otherwise an admin's level wins over `compression_level` in the file, is kept in the
  database, and is taken again at every start. It changes only what is written from now on; use `store recompress` for what
  is stored. Each change is an `admin.compression_changed` event with both levels. Another server process on the same
  database takes the new level at its next start. The totals in the overview are summed over the blobs table and kept for 30
  seconds.
- `POST /admin/users` makes an account (role `admin` or `member`) with no password and gives an **invite**, shown once:
  the person chooses their password when they join with it (a whole join link is included when `public_url` is set).
  `POST /admin/users/{id}/invite` makes a new invite, but only for an account that still has no password; for one with a
  password, make a **password reset code** (`POST …/password-reset`, good for an hour). Whoever redeems
  it in Craft Cloud chooses a new password, **every device of the account is signed out, and the computer that redeems
  it is signed in**. If the account has two-factor sign-in, redeeming the code needs its second factor too, unless the
  admin asked for `"reset_two_factor": true`, which turns it off with the redeem. Codes are only shown in the answer,
  never in the log.
- `PATCH /admin/users/{id}` changes the role and/or `quota_bytes`, a limit on what that account may store that replaces
  `stored_bytes_per_user` for it (at least 4 MiB; `null` removes it; effective quota is the override, else configured
  cap, else unlimited; it also holds when there is no server-wide cap; one
  below what the account stores deletes nothing and refuses new bytes).
- `POST …/disable` signs every device of an account out and refuses its sign-in and joins (`account_disabled`);
  `POST …/enable` allows them again but signs nothing back in. `DELETE /admin/users/{id}` marks an account for removal:
  it is disabled for good and leaves the list, its name stays taken, and **nothing it stored is deleted yet** (that comes with
  garbage collection). You cannot disable or remove yourself, and the last enabled admin cannot be demoted, disabled or
  removed (`last_admin`).
- `GET /admin/users/{id}/devices` lists an account's devices and `DELETE …/devices/{device_id}` signs one out.
- `POST …/reset-two-factor` takes an account's two-factor sign-in away (it lost its authenticator and its recovery codes):
  its secret, recovery codes and sign-in challenges go, and **every device is signed out**. Not your own account (turn
  that off in your account), and nothing to reset is `409 conflict`.
- Resetting another **admin's** password or two-factor sign-in over HTTP needs two-factor sign-in on your own account
  (`403 forbidden` without it); the command line on the host is the way if no admin has it.
- **A code serves one change.** The server accepts one 30-second step of an account's authenticator once, and only a later
  step after it; sign-in and the admin's confirmations share that. So a second change in the same 30 seconds takes the
  next code (wait for it) or an unused recovery code.

A wrong confirmation password is `403 wrong_password` (never 401); five wrong ones in 15 minutes hold the account off for
the rest of the window. A wrong or missing code for an admin who has two-factor sign-in is `403 two_factor_invalid`. A signed-out device's live connection ends at once in the server process that did it; another
server process on the same database, or the command line, finds out at the connection's next ping (within about 30 seconds).
The command line stays as the way in without HTTP and enforces neither the confirmation nor `last_admin`.

## The admin pages in the browser

The server can serve admin pages: users, invites and reset codes, a user's devices, the audit log and its download, in
plain server-rendered HTML (no script, no Node, nothing to build). They are **off by default** and only administrators can
sign in. They do the same things as the admin API with the same confirmation (your password again for every change, and a
code when your account has two-factor sign-in).
The Craft Cloud desktop app also has admin storage views: a server overview with quota and manifest-index summaries,
and per-account usage details. Both use the authenticated admin API; members do not see admin pages.

```toml
public_url = "https://cloud.example.org"   # the address of the pages, https, no path
behind_proxy = true
trusted_proxies = ["10.0.0.2"]

[web_admin]
enabled = true
idle_minutes = 480      # a session ends when unused for this long (5 to 1440, default 8 hours)
absolute_hours = 168    # and at the latest this long after signing in (1 to 720, default 7 days)
```

Open `https://cloud.example.org/web` and sign in as an admin. The server refuses to start with `enabled = true` unless
`behind_proxy = true` and `public_url` is an `https://` address with no path: the pages' cookie is `Secure` (a browser only
sends it over HTTPS) and every form is checked against the origin of `public_url`.

**Behind the proxy.**

- The proxy terminates TLS and forwards `/web` and `/api` (and `/join`) to the server's `listen` address. It must pass the
  `Origin` and `Cookie` headers through and must not rewrite the `Host` to something that changes the origin the browser
  sees: browsers send `Origin: https://cloud.example.org`, and anything else is refused with `403`. Serving the pages under a
  path prefix is not supported (`public_url` may not have a path).
- Put the proxy in `trusted_proxies` and let it **overwrite** `X-Forwarded-For` with the client's address; the sign-in limits
  count the address that yields. A proxy that passes a client's own `X-Forwarded-For` along lets a visitor pick the address
  the limits count. A misconfigured server shows itself at start (the config is refused) or as `403` on every form (the
  proxy changed the origin).
- Send `Strict-Transport-Security` from the proxy; the server does not. Do not cache `/web` in the proxy: every answer says
  `Cache-Control: no-store`.
- Bind `listen` to the proxy-facing address only. The cookie never leaves the browser over plain HTTP, but the plain HTTP
  port must not be reachable by anyone but the proxy.

**Sessions.** The session is a random value in the cookie `__Host-craft_cloud_session` (`Secure`, `HttpOnly`,
`SameSite=Strict`); the database keeps only its hash. A session ends when you sign out, after `idle_minutes` without use,
after `absolute_hours`, and **whenever the account changes**: a password change or reset, disabling, enabling or removing it,
and turning its two-factor sign-in on or off or resetting it. A demoted admin is out at the next request. Signing in again
makes a new session; there is no "remember me". Because the cookie is `SameSite=Strict`, opening the pages from a link on
another site shows the sign-in page once, and the next click on the pages themselves works.

**Cleaning up.** Expired sessions are deleted a few at a time when someone signs in. On a server that is signed in to rarely,
or to empty the table:

```sh
craft-cloud-server web-sessions --config cc-data/config.toml prune            # deletes what is over, prints how many
craft-cloud-server web-sessions --config cc-data/config.toml prune --dry-run  # only counts
```

Run it from cron or the task scheduler (daily is plenty), like `audit prune`. A valid session is never touched. There is no
background task.

**What the log shows.** A sign-in to the pages is `auth.login` without a device; changes made there are the usual
`admin.*` events with the admin as the actor and no device. Failed sign-ins, second-factor failures and the lock are the
usual `auth.*` events. Nothing secret is logged: not the cookie, the page's forms, a code or a password.

**Accessibility.** The pages are plain HTML: labelled fields, tables with captions and header cells, a skip link, visible
focus and no script, so they work with a keyboard and a screen reader and scale with the browser's zoom. They have been
checked by reading the markup and the tests, **not yet with a screen reader**: report what does not work.

## The security audit log

The server keeps a log of the events that matter for who may do what, in its database: sign-ins and failed sign-ins (a
row per account, client kind and minute, with a count), devices signed out, password changes, an administrator's changes to accounts,
roles, quotas, invites, reset codes and devices, codes redeemed on a new computer, and what the command line does to an
account (`user reset-password`, `user reset-2fa`, `device revoke`, `device revoke-all`). Two-factor sign-in writes its
changes, a recovery code's use, a lock and the wrong second steps of a sign-in or a join (never a code), but not each wrong code
on a signed-in route. Reading it:

- Every account sees its own events, and what was done to it, with `GET /api/v1/account/activity`.
- Admins read all of it with `GET /api/v1/admin/audit` (filters `user`, an account ID, and `action`, an event name like
  `auth.login` or a group like `auth.`; pages by `cursor`) and export it with `GET /api/v1/admin/audit/export`, newline
  delimited JSON, newest first. The **last line** says whether the export is whole: `{"complete":true,"exported":n}`, or
  `{"truncated":true,"exported":n}` when the log holds more than the 100,000 events an export carries (narrow it with
  `user` and `action`). An export that ends without either line broke off: do not treat it as complete. Reading the log is
  not logged.
- On the server's host: `craft-cloud-server audit --config cc-data/config.toml list [--user NAME] [--action auth.] [--limit N]`
  prints the newest events as text (accounts by ID, not by name).

**What is in it, and what is not.** A row has a time, the IDs of the account and device that acted and of what it acted
on, an event name, success or failure, and a few typed values (how many devices were signed out, the role before and after,
which kind of client and which platform out of a short list). Client kinds are `agent`, `desktop`, `cli` and `web`; `web`
means a browser session of the admin pages. It has **no IP addresses, no file names, no device names, no
names of accounts that do not exist, no passwords, codes, tokens or other secrets**; what a client types is never copied
into it. Because it holds account IDs and times it is personal data about the people who use the server: tell them
what you keep it for (finding out who changed what and who signed in or tried to), and for how long. An event keeps the ID
of an account that was removed; the name is not stored.

**How long.** Events are kept 365 days. Set `retention_days` (1 to 3650) in an optional `[audit]` section of `config.toml`:

```toml
[audit]
retention_days = 365
```

`craft-cloud-server audit --config cc-data/config.toml prune` deletes the events older than that and prints how many
(`--dry-run` only counts). The server also deletes expired events when it writes a new one, at most once an hour and in small
batches that never hold up a sign-in; **a server nobody uses keeps its expired events** until `prune` runs, so run it from cron
or the task scheduler, for example daily. There is no background task.

The log is append-only in the server's code, not enforced by the database (a design choice: the same code runs on SQLite and Postgres, and SQLite has no database roles; triggers exist but are not used). Someone
who can write to the database can change it; keep the database file and Postgres user private.

## Upgrading

This release adds migrations (`0007`, device sessions, `0008`, the admin API: reset codes, per-account quotas, the
removal mark, `0009`, the audit log, `0010`, two-factor sign-in: the secrets, recovery codes and sign-in challenges,
`0011`, the admin pages' browser sessions, `0012`, the history index, `0013`, the trash, and `0014`, the manifest reference
index). The server applies them when it starts. A database it
has migrated cannot be opened by an older server: that one refuses to start ("the database was created by a newer Craft
Cloud"). Rolling back therefore means restoring a backup taken before the upgrade, so take one first. Upgrade the
server first: agents and apps keep working unchanged, **except that an account that turns on two-factor sign-in can't sign in
with an agent from before this release** (see "Two-factor sign-in"). Then update the desktop package with its matching agent,
then agents installed on their own. The `init` of a data directory made before this release did not make a key: add one
with `[two_factor] key_file` in `config.toml` (32 random bytes, owner-only) before anyone sets up two-factor sign-in. A config
that has the new `[audit]` or `[web_admin]` section is refused by an older server (it rejects unknown settings), so remove the section before
going back to one. To turn the admin pages on after the upgrade: take the backup, start the new server (it migrates), then add
`[web_admin]` as described above and restart behind the proxy. Several servers on one Postgres database must all run this
release before any of them is given `[web_admin]`, as for every migration. Likewise a config with `capacity` in `[storage]`
is refused by an older server, so remove the line before going back to one. The new fields of `GET /admin/storage` are
optional: an older app reads a newer server's answer, and a newer app reads an older server's, without them.
## Version history

The server keeps every version of every file; nothing is thinned or removed yet. A signed-in device can list a file's
versions (`GET /api/v1/nodes/{id}/history`) and make an old one current again (`POST /api/v1/nodes/{id}/restore`),
which adds a new version at the end of the file's history rather than rewinding it, so other computers pick it up
through the normal change feed. This needs no setting and no database migration, and an older server answers
`not_found` for both paths. The agent and the apps do not use them yet.

## The trash

A deleted file or folder stays in the trash, and in the server's records, until it is removed for good. A signed-in
device lists the trash (`GET /api/v1/trash`), brings a node back (`POST /api/v1/trash/{id}/restore`, which names it
differently if its name has been taken meanwhile) or takes it out of the trash (`DELETE /api/v1/trash/{id}`). Taking
it out only marks it: **nothing is deleted from the server yet**, no version or chunk is freed, and the node can't be
brought back from the trash afterwards. A folder in the trash is listed alone: everything that is in the trash below it
(deleted and not removed from the trash) comes back with it when you bring the folder back, and goes with it when you
take the folder out, **also a file that was deleted on its own, earlier, inside that folder**. Those items are listed
only through the folder. A folder brings back, or takes out, up to 10,000 items below it at once, the shallowest first; if
there are more, the rest then appears in the trash list on its own (its folder is live again or out of the trash), and
you bring it back or remove it by a further call. The only setting is the number of days a deleted node waits, shown to clients as
`purge_after_ms`; it removes nothing yet:

```toml
[retention]
trash_days = 30   # 1 to 3650
```

A file brought back gets a new version with its old content, so another computer's delete that was sent just before can't delete it again. Update the agents with or soon after the server: an older agent can delete a *folder* a second time if that folder was brought back right after a delete whose answer it never heard (the folder is empty and can be brought back again).

The change adds two columns and an index to the metadata (migration 0013). An older server refuses the
upgraded database and rejects the `[retention]` section, so take a backup before upgrading and do not roll back.

## Checking the storage

`craft-cloud-server verify --config cc-data/config.toml` checks the database and the blob storage against each other
and prints what it finds; `--json` prints a machine-readable report and `--skip-chunk-reads` skips reading every chunk
object (which hashes it, and takes as long as reading all your data). It changes nothing and exits with an error if
it found a problem. It opens a SQLite database read-only: the database file and its WAL keep their bytes, also after a
crash that left committed data in the WAL, which it reads. Opening may leave an empty `-wal` and a `-shm` file behind
(SQLite's rebuilt index of the WAL, not data). The report also says how many manifests have a complete reference index, which is what a later
cleanup would need before it could free anything. After upgrading, run `craft-cloud-server index build --config
cc-data/config.toml` once: it indexes the manifests stored before the index existed, writing only that index, in small
steps, and it can be stopped and run again. Nothing in the server frees storage yet.

The change adds two tables to the metadata (migration 0014). An older server refuses the upgraded database,
so take a backup before upgrading and do not roll back.

`craft-cloud-server gc --dry-run --config cc-data/config.toml` inventories stored chunks and manifests, compares them with
the database and prints a preview; `--json` prints schema version 1. It changes no data, on SQLite or on PostgreSQL.
Like `verify`, it opens a SQLite database that exists already and creates nothing; opening a WAL-mode database may leave
an empty `-wal` file and a `-shm` file behind (SQLite's rebuilt index of the WAL, not data), and the database file and
the WAL's contents stay as they were, also after a crash that left data in the WAL. Store objects with no `blobs` row are orphan candidates, but need two runs apart to establish a future
first-sight grace period. Recorded chunks no complete indexed manifest names are candidates only when every manifest index
is complete and database checks pass. They come from one read-only database snapshot; the store listing is a separate,
time-point diagnostic. A future collector must recheck roots before claiming a chunk. Current versions, saved versions,
trash items, live snapshots and import snapshots protect their chunks. Unrooted manifests are protected because they may be live document assets. Without
`--dry-run`, the command refuses: deleting is not implemented yet. Listing errors fail the command; they do not mean the
store is empty. Nothing is deleted.

## Blobs on an SFTP server

`kind = "sftp"` keeps the blobs on a file server you rent or own, such as a storage box, instead of local disk or an
S3 bucket. The database stays where it is (SQLite or Postgres on the server's own disk). It suits cheap capacity per
terabyte, and it costs a network round trip for every blob operation, so a sync that touches many small chunks is
slower than on a local disk. Local disk and S3 stay the tested choices; use this on a server whose connection to the
file server is good, and try it before you rely on it. The server needs the `sftp` feature, which the default build
has.

```toml
[storage]
kind = "sftp"
sftp_host = "files.example.org"
sftp_user = "craft-cloud"
sftp_root = "/craft-cloud/blobs"         # absolute, or relative to the account's home directory; created if missing
sftp_key_file = "sftp-key"               # relative to config.toml; or the CRAFT_CLOUD_SFTP_KEY_FILE environment variable
sftp_host_key = "SHA256:…"               # the server's host key; or sftp_known_hosts = "known_hosts"
# sftp_port = 22
# sftp_key_passphrase = "…"              # or the CRAFT_CLOUD_SFTP_KEY_PASSPHRASE environment variable
# sftp_connections = 4                   # connections open at once, 1 to 16
# sftp_timeout_secs = 30                 # a connect or a request, 1 to 300
# sftp_retries = 3                       # tries again after a dropped connection or a timeout, 0 to 8
# capacity = "4 TiB"                     # as for any storage: only for the admin's overview (SFTP has no free-space query)
```

- **The server is identified by a pinned host key, and there is no mode without one.** Take it from the server's own
  files, not from the first connection: `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` prints the `SHA256:…`
  fingerprint to put in `sftp_host_key` (a public key line such as `ssh-ed25519 AAAA…` works too, and so does a
  `known_hosts` file in `sftp_known_hosts`). The server refuses to start, or to connect, when the key it meets is not
  that one. Ed25519 and ECDSA host keys are supported; RSA host keys are not.
- **Signing in is by key only.** Make a key just for Craft Cloud (`ssh-keygen -t ed25519`), install its public half for
  an account that can reach only this directory, and put the private half in `sftp_key_file`. Passwords are not
  supported. The key file, its passphrase and the environment variables are read like S3's keys: nothing is logged or
  shown by the admin pages, and the config file is owner-only on Unix.
- **The file server must offer the `posix-rename@openssh.com` extension** (every OpenSSH-based SFTP server does). A write
  goes to a file named `….tmp-<id>` in the target's directory and is renamed over the target in one step, so a reader sees
  the old blob or the new one and never a part. A server without the extension is refused at start with a message that
  names it: Craft Cloud never replaces a blob by deleting it first. A failed upload removes its temporary file, on a fresh
  connection when the first one is gone. When that cannot be done (the server was killed, the file server went away), the
  temporary file stays in its shard directory, and the listing does not show it. `craft-cloud-server store clean-temp
  --config … [--older-than-hours 24]` removes the ones older than that (only exact `.tmp-<id>` names, by the time in the
  name; an upload that is running or finished is never touched). Run it now and then (say weekly); it walks
  every shard directory, so on a big store it takes a while.
- **A successful write means the file server accepted it.** SFTP has no portable `fsync`; when the server offers
  `fsync@openssh.com` the file is flushed before the rename, but what happens at the provider after that is not
  something Craft Cloud can see. Keep your own backup of the remote directory (see "Backing up to an SFTP server" for
  the database and a copy of the blobs on a second server).
- Commands that only use the database (`user`, `device`, `audit`, `reset-epoch` and the like) do not open the blob store,
  so they work while the file server is away. `serve` and every command that reads or writes blobs connect at start, and
  refuse to start when they cannot.
- The server keeps a few connections open and tries a request again, up to `sftp_retries` times with growing waits, when
  a connection drops or times out. A refusal (permissions, a wrong key, a full disk) is not retried and is reported as an
  error to the one request. Some providers allow only a few connections: lower `sftp_connections`.
- `store estimate` and `store recompress` read every blob they look at over the network, and `recompress` writes it back:
  expect time and traffic. `store measure` asks for each object's size with one request. `verify` reads every chunk.
- The setting `min_free` is not offered for SFTP, and the server does not refuse uploads when the remote directory is
  full: the file server's own refusal comes back as an error.

## Backing up to an SFTP server

`craft-cloud-server backup` copies the data to an SFTP server, and `restore` builds a data directory from it. It runs
while the server runs, does not stop it, and is safe to repeat.

```sh
craft-cloud-server backup --config /var/lib/craft-cloud/config.toml \
  --to sftp://backup@files.example.org/craft-cloud-backup \
  --key-file /etc/craft-cloud/backup-key --host-key 'SHA256:…'
```

- **What it copies.** First a consistent copy of the SQLite database (taken in one transaction with `VACUUM INTO`, so
  writes during the backup do not tear it), then every blob the backup server does not have yet. Blobs never change, so
  a repeated backup sends only what is new; a name that is already there is skipped (a blob that was damaged *there* is
  not repaired by a repeat: check the copy with `restore` into a scratch directory and `verify`). Last, a
  `manifest.json` with the date, the server's version, the database's hash and the counts. A backup without its
  manifest is not offered by `restore`. The blobs are read through the store, so each is checked against its ID on the
  way; a blob that cannot be read stops the backup with an error that names it.
- **Where.** `sftp://user@host[:port]/directory` (`/~/directory` for one in the account's home). Below it:
  `chunks/` and `manifests/` as in the store, and `generations/<UTC stamp>/` with the dump (`meta.sqlite`) and the manifest.
  `--keep N` (default 7) keeps the newest N complete database generations and removes older ones; blobs are never removed.
  A generation is named by the second its backup began in. If the backup server has that name already (a second backup
  running at once, or a clock set back), the new backup stops with a message and changes nothing; run one backup at a
  time. The generation just written is never removed, and one without its manifest is left alone for a day before it is
  taken for a backup that died.
- **Signing in.** The same rules as for an SFTP blob store: a key (`--key-file`, or `CRAFT_CLOUD_SFTP_KEY_FILE`), the
  key's passphrase from `CRAFT_CLOUD_SFTP_KEY_PASSPHRASE`, and a pinned host key (`--host-key` or `--known-hosts`) that
  the command insists on. A password in the address is refused. The settings can sit in the config instead:

  ```toml
  [backup]
  to = "sftp://backup@files.example.org/craft-cloud-backup"
  key_file = "backup-key"
  host_key = "SHA256:…"
  keep = 7
  ```

  Command-line options win over `[backup]`. The dump is made in the system's temporary directory first; `--work-dir`
  names another place when that is small. It needs room for the database.
- **Not copied, on purpose:** `config.toml` (it holds secrets) and `two-factor.key`. Keep copies of both apart from the
  backup: a backup does not hold them, and without the key the two-factor secrets in a database cannot be opened.
- **Postgres** is not dumped by this command. Run `pg_dump --format=custom` yourself (Craft Cloud does not run it for
  you) and keep the dump with the rest; the command refuses a Postgres server with a message that says so, and the
  blobs of such a server are not copied either.
- **Schedule it** with a systemd timer (the server has no scheduler of its own):

  ```ini
  # /etc/systemd/system/craft-cloud-backup.service
  [Service]
  Type=oneshot
  User=craft-cloud
  ExecStart=/usr/local/bin/craft-cloud-server backup --config /var/lib/craft-cloud/config.toml
  # /etc/systemd/system/craft-cloud-backup.timer
  [Timer]
  OnCalendar=daily
  Persistent=true
  [Install]
  WantedBy=timers.target
  ```

  Enable it with `systemctl enable --now craft-cloud-backup.timer`, and look at `systemctl status craft-cloud-backup`
  now and then: a backup nobody reads the result of may have stopped long ago.

**Restoring** (the restore drill; do it once before you need it):

```sh
craft-cloud-server restore --from sftp://backup@files.example.org/craft-cloud-backup \
  --key-file /etc/craft-cloud/backup-key --host-key 'SHA256:…' --dir /var/lib/craft-cloud-restored
```

The directory must be empty or missing; a directory with anything in it is refused and left alone. `restore` takes the
newest complete generation (or the one named with `--generation 20261107T031500Z`), checks the database dump against the
hash and size in its manifest and opens it, then copies the blobs. It builds everything in a directory beside the target
(`<dir>.restore-<id>`) and moves it into place only when it is all there, so a restore that fails, or is killed, never
leaves a database with part of its blobs at the target and a re-run is not refused. After a kill, delete the leftover
`….restore-<id>` directory. The result is `meta.db` and `blobs/`. Then put your `config.toml` and the two-factor key beside them (storage path `…/blobs`, database
`sqlite:…/meta.db`), run `craft-cloud-server verify --config …`, and, as for any restore, `reset-epoch --all` before
`serve` (see "Restoring a backup"). The blobs of the backup are a superset of what the restored database names: ones
uploaded after the dump are not referenced and only take up room until a future garbage collection.

## Restoring a backup

With the server stopped, restore the database and the chunk storage from the backup (and the two-factor key file, if
accounts use two-factor sign-in: the server will not start without it), then run
`craft-cloud-server reset-epoch --config cc-data/config.toml --all`, and only then start `serve` again. Every
account then starts a new epoch, so each synced machine reconciles with the restored state instead of trusting its
own change counter. `reset-epoch --config … name` resets one account. (A reset run while the server is up still
works: open live connections end at their next ping, within 30 s, and connect again.)

## A full recents document

An app's recents log holds at most 16 MiB of updates per account. Once it is full, machines keep their new recents
locally and say so. `craft-cloud-server live --config cc-data/config.toml reset-doc name wordcraft` empties that log;
every machine then sends up what the emptied log lacks, so nothing any machine holds is lost, and file syncing isn't
affected. A document whose own history is over 16 MiB stays full until compacting the log is supported. An account
may have 64 live documents holding updates and 1,024 in all (one per app today); past that, new ones stay on the
machines. Live documents can't be deleted yet.

## Docker

`packaging/docker/` holds a hardened container image of the server and a Docker Compose deployment: the server
behind Caddy, which terminates TLS. By default the database (SQLite) and the blobs live on a Docker volume; Postgres
and S3 are overlays. It needs Docker Compose 2.32 or newer on Linux and builds the image locally: there is no image
to pull and nothing is published to a registry. The first build wants about 4 GiB of memory for the Docker engine (it
uses two compiler jobs).

### Quick start

```sh
cd packaging/docker
./install.sh --domain cloud.example.org --admin you
```

`install.sh` checks Docker and the free disk space, writes `.env` (mode 0600, never committed) with random database
passwords, builds the image, starts the stack and creates the first admin. The admin's password is asked for without
echo, or read from a pipe, and reaches the server through standard input only. On a terminal it asks for the
settings you did not give; `./install.sh --help` lists the options (`--tls acme|internal|custom`, `--email`,
`--db sqlite|postgres`, `--http-port`, `--https-port`, `--no-admin`, `--no-start`). It never overwrites an existing
`.env`. Afterwards `docker compose ps` and `docker compose logs -f server` show the state, and the domain's
`/api/info` answers through Caddy.

To set it up by hand, copy `.env.example` to `.env`, fill it in, and run `docker compose build` and
`docker compose up -d --wait`.

### What runs

| Service | Image | Notes |
|---|---|---|
| `server` | built from `packaging/docker/Dockerfile` | distroless `cc`, uid 65532, plain HTTP on port 8080 inside the stack |
| `caddy` | `caddy:2.11-alpine` | the only service with published ports (80 and 443); TLS and the redirect from HTTP |
| `secrets-init` | `caddy:2.11-alpine` | one-shot, no network: renders `config.toml` and writes the secret files |
| `db`, `db-init` | `postgres:17-alpine` | only with `compose.postgres.yaml` |

Every base image is pinned by the digest of its multi-arch index, with the tag in a comment next to it. To update
one, change the tag and the digest together (`docker buildx imagetools inspect <image>:<tag>` prints the index
digest).

Every service has a read-only root filesystem, no capabilities (`cap_drop: [ALL]`), `no-new-privileges`, memory and
process limits, and a `noexec` tmpfs for `/tmp`. The exceptions are explained next to each service: Postgres and
`secrets-init` start as root and need a few capabilities to fix ownership, Caddy needs to bind ports 80 and 443, and
`secrets-init` (in the Postgres and S3 overlays) is not read-only because Compose writes its secret files into the
root filesystem.

Three networks keep the parts apart: `edge` (published ports and outbound access, Caddy only), `frontend` (internal;
Caddy to the server) and `backend` (internal; the server to Postgres, Caddy is not attached).

### Configuration and secrets

The server reads only its TOML file. `config.toml.in` is the template and `render-config.sh` fills it. The
`secrets-init` service renders it into the `secrets` volume (mode 0400, owner 65532) and the server mounts it
read-only at `/etc/craft-cloud/config.toml`. The file holds the database URL (with the password, for Postgres) and,
for S3, the keys. The server's `Debug` output hides the URL's credentials, and `docker inspect` of the running
services shows no secret, since none is in an environment or a Compose file. The values are checked against short
lists of allowed characters before they are written, so a value cannot break the TOML or add a setting.

To change a setting in `.env` (`CC_*`, see `.env.example`), edit the file, run `docker compose up -d --force-recreate secrets-init`
(it must be recreated to see the new values: Compose gives a container its secrets when it is created, and a plain
`up` would start the old container again and write the old values back), then `docker compose restart server`.

The server believes `X-Forwarded-For` only from Caddy. `trusted_proxies` takes single addresses, no networks, so
Caddy has a fixed address (`CC_CADDY_IP`, `172.29.215.2` in the network `CC_FRONTEND_SUBNET`, `172.29.215.0/24`). If
these defaults clash with a network on your host, change both together, and `CC_FRONTEND_POOL` (where the other
containers get addresses from) with them.

### TLS modes

Set `CC_TLS_MODE` in `.env`:

- `acme` (default): a public certificate from Let's Encrypt. The domain must resolve to the host, and the ports must
  be reachable from the internet (with other ports, set `CC_HTTP_PORT`, `CC_HTTPS_PORT` and a matching
  `CC_PUBLIC_URL`).
- `internal`: a certificate from Caddy's own CA, for tests. Clients do not trust it. There is no HSTS in this mode.
- `custom`: your own certificate. Put `fullchain.pem` and `private.key` in `packaging/docker/tls/` (or `CC_TLS_DIR`),
  readable by root only, and restart Caddy to load a renewed one.

Caddy sets a request header timeout of 10 seconds, as the checklist above asks. It writes no access log, because URLs hold invite
codes (`/join/<code>`); its error log leaves out the request's URI and headers for the same reason.

### Postgres

```sh
./install.sh --domain cloud.example.org --db postgres
```

or add `COMPOSE_FILE=compose.yaml:compose.postgres.yaml` to `.env`. This adds a Postgres container and a one-shot
`db-init` that creates the role `craft_cloud`. The server migrates its database when it opens it, so that role owns
the database `craft_cloud` and its schema and may change them. That is less separation than an owner role plus a
runtime role. The role is not a superuser and has no other privileges; `PUBLIC` may not connect to the database or
create in its schema, and the role cannot connect to `postgres` or `template1`. The superuser password exists only
for the Postgres container and `db-init`. Blobs stay on the data volume.

To rotate the database password, change `CC_SECRET_DB_PASSWORD` in `.env` (characters from `A-Za-z0-9._~-`, at least
32), then run `docker compose up -d --force-recreate secrets-init`,
`docker compose up -d --force-recreate db-init` and `docker compose restart server`. The superuser password cannot be changed that way: Postgres applies it only when
the data directory is created. Change it with `ALTER ROLE postgres` first and then in `.env`.

The connection between the server and Postgres is not encrypted (the server's `sqlx` has no TLS feature). It never
leaves the internal `backend` network. An external Postgres over TLS is not covered.

### S3

`compose.s3.yaml` replaces the disk storage with an S3 bucket: fill in the `CC_S3_*` and `CC_SECRET_S3_*` lines of
`.env.example` in `.env` and add `compose.s3.yaml` to `COMPOSE_FILE`. The keys end up in `config.toml` like the
database password, not in the server's environment. The server joins the `edge` network to reach the endpoint. The
image holds the CA certificates the server needs for HTTPS endpoints. The overlay has been tried with a local S3 mock
over plain HTTP (`CC_S3_ALLOW_HTTP=true`), not with a real S3 service or an HTTPS endpoint.

### The first admin and more accounts

`install.sh` creates the first admin. By hand, with the password on standard input:

```sh
printf '%s\n' 'a password' | docker compose run --rm --no-deps -T server user --config /etc/craft-cloud/config.toml add --admin you
```

More accounts: the same line without `--admin`, or `docker compose run --rm --no-deps server user --config
/etc/craft-cloud/config.toml invite name`, which prints the whole join string because `public_url` is set. The
other commands of this guide (`device`, `reset-epoch`, `live`) run the same way.

### Data, upgrading and backups

The data is in the volume `data` (`/var/lib/craft-cloud` in the container): `meta.db` (with SQLite) and `blobs/`.
The image creates the directory for uid 65532 with mode 0700, so a new named volume gets that owner. A bind mount
instead of the volume must be prepared first: `chown 65532:65532 DIR && chmod 0700 DIR`.

To upgrade, update the source, then run `docker compose build server` and `docker compose up -d --wait`. The server
migrates the database when it opens it and refuses a database newer than itself, so back up before an upgrade and do
not go back to an older image without the backup.

`craft-cloud-server backup` (see "Backing up to an SFTP server" in the server guide) copies a SQLite server to an SFTP server while it runs; there is no schedule built in. By hand: stop the server (`docker compose stop server`), copy the volume (SQLite:
`meta.db`, `meta.db-wal`, `meta.db-shm` and `blobs/` together; Postgres: `pg_dump` from the `db` container plus
`blobs/`), and start it again. Restoring is described above, including `reset-epoch`.

`docker compose down` keeps the volumes. `docker compose down -v` deletes all data.

### Troubleshooting

- `secrets-init` fails with a message such as `PUBLIC_URL has a value that is empty or has characters that are not
  allowed`: a setting in `.env` is wrong. The message names the setting and never shows the value.
- The server exits at once with `unable to open database file`: the data directory is not owned by 65532 (see bind
  mounts above).
- `server` is not healthy: `docker compose logs server`. The check is `craft-cloud-server healthcheck`, a
  `GET /api/info` request to `127.0.0.1:8080` inside the container.
- All devices share one sign-in budget: `trusted_proxies` does not contain Caddy's address. `CC_CADDY_IP` must lie in
  `CC_FRONTEND_SUBNET`; run `docker compose up -d --force-recreate secrets-init` and `docker compose up -d` after changing either.

### Not covered

Postgres over TLS, TLS in the server itself (Caddy terminates it), Kubernetes or Helm files, a registry image and
release automation, and backup automation (copy the data as above until the server can create backups itself).
