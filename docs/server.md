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
connection ends at its next ping (within about 30 seconds), and a change-feed long-poll within 60 seconds; every
other request is refused at once (`401 unauthorized`). Devices signed out through the API end their live connections
immediately.

## Upgrading

This release adds a migration (`0007`, device sessions). The server applies it when it starts. A database it
has migrated cannot be opened by an older server: that one refuses to start ("the database was created by a newer Craft
Cloud"). Rolling back therefore means restoring a backup taken before the upgrade, so take one first. Upgrade the
server first: agents and apps keep working unchanged.

## Restoring a backup

With the server stopped, restore the database and the chunk storage from the backup, then run
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
