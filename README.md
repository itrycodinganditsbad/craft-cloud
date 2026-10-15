# Craft Cloud

Self-hosted sync and storage for the Craft App Suite (PhotoCraft, LightCraft, FilmCraft, DesignCraft,
VectorCraft, PdfCraft, EffectCraft, WordCraft, DeckCraft, GridCraft, CADCraft, SoundCraft), in pure Rust.

You run the server on your own computer, NAS or VPS. A folder on each of your machines then stays in sync
through it: project files, app settings, brushes, fonts, presets and other asset libraries, with version
history. It works for one person with a laptop and a desktop, and for a small studio with many accounts.

There is no hosted service and no account with anyone but you. Nothing phones home.

> **Status: early development.** The server, the sync engine and the agent work: two machines keep a
> folder in step with `craft-cloud-agent run` (or one pass at a time with `sync`). See
> [Where things stand](#where-things-stand). The full design is in [`docs/design/sync.md`](docs/design/sync.md).

## What it will do

- **One synced folder per machine.** The Craft apps keep saving plain files to disk, as they do today. A
  small background agent watches the folder and syncs it with your server. No app needs a plugin, and
  everything keeps working offline.
- **Big files sync quickly.** Files are split into content-defined chunks (FastCDC, about 1 MiB on
  average). When you change part of a 20 GB video project, only the chunks that changed are uploaded.
  Identical chunks are stored once, and an interrupted upload resumes where it stopped.
- **Nothing is lost.** Every version of every file is kept for a set time. When two machines change the
  same file, both versions are kept: one under its name and one as
  `name (conflict, <device>, <date>).ext`. Nothing is locked.
- **Moves are cheap.** Files and folders have stable IDs, so renaming or moving a 10,000-file folder is a
  single change, and history follows the file.
- **Safe with backups.** After the server is restored from a backup, every machine notices and reconciles
  with the restored state instead of trusting its own record of what happened.
- **A guard against mistakes.** If one sync would delete more than 500 files or more than 20 % of the
  folder, it pauses and asks first.
- **Selective sync.** Big folders can stay on the server without taking space on every machine.
- **Later: collaboration.** Sharing, review comments and team libraries, built on top of sync.

## How it fits together

```
 ┌──────────────── each machine ────────────────┐        ┌──── self-hosted ─────────────┐
 │ Craft apps ──(plain files)──┐                │        │ craft-cloud-server           │
 │                             ▼                │ HTTPS  │  ├─ auth (users, devices)    │
 │ craft-launcher ──local API──► craft-cloud-   │◄──────►│  ├─ metadata (SQLite or PG)  │
 │                               agent ◄─watch─ │        │  └─ chunks (disk or S3)      │
 │                                 └ state DB   │        └──────────────────────────────┘
 │                          ~/Craft Cloud/      │
 └──────────────────────────────────────────────┘
```

- **The server** stores file contents as chunks, either on local disk or in any S3-compatible object
  store. It keeps the metadata (users, devices, the file tree, versions and a change feed) in SQLite or
  Postgres. Every change arrives as one batch commit, which is applied completely or not at all.
- **The agent** runs on each machine. It uploads what changed in the synced folder, applies what other
  machines changed, and keeps its own small state file. `craft-cloud-agent run` keeps the folder in step
  on its own (it watches the folder and the server); `craft-cloud-agent sync` runs one pass by hand.
- **The launcher** will be the user interface: sign-in, sync status, history and restore, the conflict
  list. It talks to the agent over a documented local API and doesn't link any Craft Cloud code.

### The workspace

| Crate | What it does | State |
|---|---|---|
| `craft-cloud-proto` | Wire types: names and their rules, IDs, hashes, manifests, commit operations, API bodies, limits | done |
| `craft-cloud-chunker` | FastCDC chunking (`fastcdc-v1`, pinned by golden test vectors) and BLAKE3 hashing | done |
| `craft-cloud-store` | Chunk storage on disk, S3 or memory; checks every chunk's size and hash on the way in and out | done |
| `craft-cloud-meta` | Metadata on SQLite or Postgres: users, devices, the tree, versions, the batch commit, the change feed | done |
| `craft-cloud-server` | The HTTP API (`/api/v1`), authentication, rate limits and the admin CLI | done |
| `craft-cloud-sync` | The sync engine: compares the folder with the server, uploads, downloads, resolves conflicts | done |
| `craft-cloud-agent` | The agent on each machine: real disk, HTTPS transport, saved state, root marker, command line (`login`, `run`, `sync`, `status`, `logout`) | runs on its own |

## Where things stand

| Milestone | What | State |
|---|---|---|
| M0 | Repository, conventions, CI on Ubuntu and Windows | done |
| M1 | Protocol types and the chunker | done |
| M2 | The server: login, devices, chunk upload, batch commit, change feed, backup epochs; SQLite or Postgres, disk or S3 | done |
| M3a | The sync engine and its simulation | done |
| M3b1 | The agent, first cut: syncs a real folder over HTTPS when you run `craft-cloud-agent sync`; two machines sync one folder | done |
| M3b2a | Hardening: downloads land atomically and never over a save you just made, crash recovery, files held open or read-only on Windows | done |
| M3b2b1 | The agent on its own: `run` watches the folder and the server, waits for files to settle, retries with backoff | done |
| M3b2b2 | A simulated network (partitions, delays, dropped connections) in the simulation | done |
| M3c | Fixes from a full review: per-account upload limits and storage cap, big libraries sync in linear time, a file too big is skipped instead of stopping the agent | done |
| M4 | History, restore, trash, cleanup of old versions, garbage collection, per-user quotas, server backup and restore | next |
| M5 | Selective sync, large-media polish, `.craftignore` | planned |
| M6 | Packaging: release binaries, Docker image, systemd unit, built-in TLS, a self-hosting guide | planned |
| M7 | The local control API and launcher integration | planned |

### The sync engine

The engine is the part that decides what to upload, download, move aside or delete. It's the code most
likely to lose data, so it's built and tested on its own. It only reaches the disk through an `Fs` trait
and the server through a `Transport` trait. That lets the tests run several machines and a real server
in one process, with an in-memory disk.

The rules it follows, among others:

- A local file is never overwritten or deleted unless its content has just been checked against the last
  synced version. Size and modification time alone are not enough.
- A download lands atomically on exactly what was checked. On Linux and macOS it swaps places with the
  file and swaps back if the file changed in between; on Windows the file is moved aside first, checked,
  and put back if needed. A save you make while a download arrives is never overwritten.
- A file another program holds open is skipped and retried later; on Windows the download is kept and
  lands once the file is free. A read-only file is never replaced or removed.
- Links (symlinks, junctions) are never followed, moved or written through, and a synced file or folder
  that became a link is not treated as deleted. One rare sequence (a folder turned into a link and back,
  a failed pass, then a move) is a known gap being fixed next; the server's history keeps the file.
- When an edit and a delete meet, the edit wins. Whether a file was edited is decided by its content, so a
  file saved again with the same bytes is not an edit.
- A conflict copy is only made when the content really differs.
- A crash at any point costs at most some bookkeeping, never a file: the state is saved at safe points
  during a pass, and an interrupted download is finished or undone on the next run.
- No file is ever held in memory as a whole. Uploads and downloads stream chunk by chunk, and every chunk
  is checked against its hash.

How it's tested:

- Scenario tests cover each rule.
- A deterministic simulation runs three machines that share one account. They create, edit, rename, move
  and delete files and folders at random, hold files locked, replace files with links, and sync at random
  moments while the connection fails mid-pass and machines crash, also between two steps on disk. At the
  end every machine must have the same files, and every file version a user didn't overwrite or delete
  must still exist on some disk.
- Seeds run in parallel: 64,000 seeds pass with and without crashes in about ten minutes, and reviews ran
  more than 100,000. A failing seed can be replayed exactly:
  `SIM_SEED=17 cargo test -p craft-cloud-sync --test simulation`.
- The same random work also runs over a simulated network: three agents and the real server talk HTTP
  through the agent's own request code while links are cut, held and slowed, and the server loses or cuts
  answers after doing the work. Each seed replays exactly:
  `SIM_SEED=17 cargo test -p craft-cloud-agent --test network`.

## Running the server

Set up the server first; the agent (below) syncs folders with it.

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

To add accounts: `printf '%s\n' 'their password' | craft-cloud-server user --config cc-data/config.toml add name`.
`--config` can come before or after `add`.

### Restoring a backup

After restoring the database and the chunk storage from a backup, run
`craft-cloud-server reset-epoch --config cc-data/config.toml --all`. Every account then starts a new epoch,
so each synced machine reconciles with the restored state instead of trusting its own change counter.
`reset-epoch --config … name` resets one account.

## Syncing a folder

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
- Start `run` from your desktop's autostart for now; installers that do it for you come with packaging.
- To sync a folder that already has files, add `--adopt` to `login`.
- `sync` exits 0 when done; 1 when it failed and can simply run again (offline, server busy); 2 when
  deletions wait for approval (`sync --approve-deletes` releases exactly the ones it listed); 3 when it
  needs you (the folder or its `.craft-cloud` marker is missing, another agent is running, the device was
  signed out, and so on). The message says what to do.
- HTTPS is required, except to a server on the same machine. Proxy settings are ignored for now.
- `logout` signs the device out on the server and forgets the account here; your files stay. Use
  `logout --local` if the server is gone.

## Building and testing

```sh
cargo xtask ci        # rustfmt, dependency licences, clippy, and every test
```

The Postgres and S3 tests run when their environment variables are set (see `.github/workflows/ci.yml`);
CI runs them against Postgres and SeaweedFS containers. A longer simulation run:
`SIM_SEEDS=1000 cargo test --release -p craft-cloud-sync --test simulation`, and over the simulated network
`SIM_SEEDS=1000 cargo test --release -p craft-cloud-agent --test network`.

Contributors and AI agents: read [`AGENTS.md`](AGENTS.md) first. It has the workspace map and the rules:
never crash, never lose data, no `unsafe`, and how the clean room works.

## Independence

Craft Cloud is an independent project built for the Craft App Suite. It is not made or endorsed by the
ArtCraft Team, and it is not compatible with any vendor's cloud service. Its protocol is its own and is
documented in [`docs/design/sync.md`](docs/design/sync.md).

## License

MIT OR Apache-2.0, at your option. See [`LICENSE-MIT`](LICENSE-MIT), [`LICENSE-APACHE`](LICENSE-APACHE)
and [`NOTICE`](NOTICE).
