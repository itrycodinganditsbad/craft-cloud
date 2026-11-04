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

For Postgres or S3, edit `[database]` and `[storage]` in `config.toml`.

`init` also makes `two-factor.key` next to `config.toml` and names it in the config (`[two_factor] key_file`). It is the key
for two-factor sign-in (below). **Back it up, apart from your database backups.**

To cap what one account may store, set `stored_bytes_per_user` (in bytes; there is no cap by default) in
`config.toml`. It counts every chunk and manifest an account uploaded once, and nothing frees space until
garbage collection arrives, including what a refused file stored before it was refused. A file that would go
over it is skipped by the agent, which says so; the rest of the folder keeps syncing. Nearly full counts as
full: with less than 4 MiB (one chunk) left, the agent pauses and says the account's storage is full.
A cap under 4 MiB is refused when the server starts, since not even one chunk would fit. Per-user quotas on
file sizes come later, with history.

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
still made by `init`). They are an API for a tool or a later admin app: the server brings no web page. Every change asks
for **a fresh confirmation in its body**, the acting admin's own current password (and, for an admin who has two-factor
sign-in, a `totp_code` or `recovery_code` too), because a device's token never expires and must not be enough on its own:

```sh
curl -s -X POST https://cloud.example.org/api/v1/admin/users/USER_ID/disable \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"password":"your password"}'
```

- `GET /admin/server` shows counters (accounts, signed-in devices, stored bytes, live-document bytes) and the kinds of
  database and storage, nothing else. `GET /admin/users` lists accounts 50 at a time (`limit` up to 200, `cursor` from
  the last answer's `next`, `query` to find a username).
- `POST /admin/users` makes an account (role `admin` or `member`) with no password and gives an **invite**, shown once:
  the person chooses their password when they join with it (a whole join link is included when `public_url` is set).
  `POST /admin/users/{id}/invite` makes a new invite, but only for an account that still has no password; for one with a
  password, make a **password reset code** (`POST …/password-reset`, good for an hour). Whoever redeems
  it in Craft Cloud chooses a new password, **every device of the account is signed out, and the computer that redeems
  it is signed in**. If the account has two-factor sign-in, redeeming the code needs its second factor too, unless the
  admin asked for `"reset_two_factor": true`, which turns it off with the redeem. Codes are only shown in the answer,
  never in the log.
- `PATCH /admin/users/{id}` changes the role and/or `quota_bytes`, a limit on what that account may store that replaces
  `stored_bytes_per_user` for it (at least 4 MiB; `null` removes it; it also holds when there is no server-wide cap; one
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

## The security audit log

The server keeps a log of the events that matter for who may do what, in its database: sign-ins and failed sign-ins (a
row per account and minute, with a count), devices signed out, password changes, an administrator's changes to accounts,
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
which kind of client and which platform out of a short list). It has **no IP addresses, no file names, no device names, no
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
removal mark, `0009`, the audit log, and `0010`, two-factor sign-in: the secrets, recovery codes and sign-in challenges).
The server applies them when it starts. A database it
has migrated cannot be opened by an older server: that one refuses to start ("the database was created by a newer Craft
Cloud"). Rolling back therefore means restoring a backup taken before the upgrade, so take one first. Upgrade the
server first: agents and apps keep working unchanged, **except that an account that turns on two-factor sign-in can't sign in
with an agent from before this release** (see "Two-factor sign-in"). Then update the desktop package with its matching agent,
then agents installed on their own. The `init` of a data directory made before this release did not make a key: add one
with `[two_factor] key_file` in `config.toml` (32 random bytes, owner-only) before anyone sets up two-factor sign-in. A config
that has the new `[audit]` section is refused by an older server (it rejects unknown settings), so remove the section before
going back to one.

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
