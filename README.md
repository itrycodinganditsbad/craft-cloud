# Craft Cloud

Self-hosted sync and storage for the Craft App Suite (PhotoCraft, LightCraft, FilmCraft, DesignCraft,
VectorCraft, PdfCraft, EffectCraft, WordCraft, DeckCraft, GridCraft, CADCraft, SoundCraft), in pure Rust.

You run the server on your own computer, NAS or VPS. A folder on each of your machines then stays in sync
through it: project files, app settings, brushes, fonts, presets and other asset libraries, with version
history. It works for one person with a laptop and a desktop, and for a small studio with many accounts.

There is no hosted service and no account with anyone but you. Nothing phones home.

> **Status: early development.** The server, the sync engine and a first agent work: two machines can
> sync a folder by running `craft-cloud-agent sync`. Automatic syncing in the background is next. See
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
- **The agent** (coming next) runs on each machine. It watches the synced folder, uploads what changed,
  applies what other machines changed, and keeps its own small state database.
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
| `craft-cloud-agent` | The agent on each machine: real disk, HTTPS transport, saved state, root marker, command line (`login`, `sync`, `status`, `logout`) | first cut done; runs by hand |

## Where things stand

| Milestone | What | State |
|---|---|---|
| M0 | Repository, conventions, CI on Ubuntu and Windows | done |
| M1 | Protocol types and the chunker | done |
| M2 | The server: login, devices, chunk upload, batch commit, change feed, backup epochs; SQLite or Postgres, disk or S3 | done |
| M3a | The sync engine and its simulation | done |
| M3b1 | The agent, first cut: syncs a real folder over HTTPS when you run `craft-cloud-agent sync`; two machines sync one folder | done |
| M3b2 | The agent on its own: watches the folder, waits for files to settle, retries with backoff | next |
| M4 | History, restore, trash, cleanup of old versions, garbage collection, server backup and restore | planned |
| M5 | Selective sync, large-media polish, `.craftignore` | planned |
| M6 | Packaging: release binaries, Docker image, systemd unit, built-in TLS, a self-hosting guide | planned |
| M7 | The local control API and launcher integration | planned |

### The sync engine (M3a)

The engine is the part that decides what to upload, download, move aside or delete. It's the code most
likely to lose data, so it's built and tested on its own. It only reaches the disk through an `Fs` trait
and the server through a `Transport` trait. That lets the tests run several machines and a real server
in one process, with an in-memory disk.

The rules it follows, among others:

- A local file is never overwritten or deleted unless its content has just been checked against the last
  synced version. Size and modification time alone are not enough.
- Before a download replaces a file, the file is checked again. If the user saved it in the meantime, the
  download waits and the two versions are handled as a conflict.
- A file another program holds locked is skipped and retried later. A remote change never overwrites it.
- When an edit and a delete meet, the edit wins.
- A conflict copy is only made when the content really differs.
- No file is ever held in memory as a whole. Uploads and downloads stream chunk by chunk, and every chunk
  is checked against its hash.

How it's tested:

- Scenario tests cover each rule.
- A deterministic simulation runs three machines that share one account. They create, edit, rename, move
  and delete files and folders at random, hold files locked, and sync at random moments while the
  connection fails mid-pass. At the end every machine must have the same files, and every file version a
  user didn't overwrite or delete must still exist on some disk.
- The simulation passes 4,000 seeds. A failing seed can be replayed exactly:
  `SIM_SEED=17 cargo test -p craft-cloud-sync --test simulation`.

## Running the server

The server is usable today, although there is no agent to sync with yet.

```sh
cargo build --release -p craft-cloud-server
# Create a data directory (SQLite + local disk) and the first admin. The password is read from stdin.
printf '%s\n' 'choose a password' | ./target/release/craft-cloud-server init --dir ./cc-data --admin you
./target/release/craft-cloud-server serve --config ./cc-data/config.toml
```

The server speaks plain HTTP on `127.0.0.1:8080`. To reach it from other machines, put a TLS reverse proxy
(Caddy, nginx, Traefik …) in front of it:

1. Set `listen` to the proxy-facing address, and set `behind_proxy = true`.
2. Put the proxy's address in `trusted_proxies`. Edit these in `config.toml`; `init` writes
   `trusted_proxies = []`.
3. Make the proxy set request header and body timeouts. The server relies on the proxy to cut off slow
   clients.

For a non-loopback `--listen`, `init` also needs `--behind-proxy`, for example
`init --dir ./cc-data --admin you --listen 0.0.0.0:8080 --behind-proxy`. `trusted_proxies` must then list
the proxy's address.

For Postgres or S3, edit `[database]` and `[storage]` in `config.toml`.

To add accounts: `printf '%s\n' 'their password' | craft-cloud-server user --config cc-data/config.toml add name`.
`--config` can come before or after `add`.

### Restoring a backup

After restoring the database and the chunk storage from a backup, run
`craft-cloud-server reset-epoch --config cc-data/config.toml --all`. Every account then starts a new epoch,
so each synced machine reconciles with the restored state instead of trusting its own change counter.
`reset-epoch --config … name` resets one account.

## Building and testing

```sh
cargo xtask ci        # rustfmt, clippy, and every test
```

The Postgres and S3 tests run when their environment variables are set (see `.github/workflows/ci.yml`);
CI runs them against Postgres and SeaweedFS containers. A longer simulation run:
`SIM_SEEDS=1000 cargo test --release -p craft-cloud-sync --test simulation`.

Contributors and AI agents: read [`AGENTS.md`](AGENTS.md) first. It has the workspace map and the rules:
never crash, never lose data, no `unsafe`, and how the clean room works.

## Independence

Craft Cloud is an independent project built for the Craft App Suite. It is not made or endorsed by the
ArtCraft Team, and it is not compatible with any vendor's cloud service. Its protocol is its own and is
documented in [`docs/design/sync.md`](docs/design/sync.md).

## License

MIT OR Apache-2.0, at your option. See [`LICENSE-MIT`](LICENSE-MIT), [`LICENSE-APACHE`](LICENSE-APACHE)
and [`NOTICE`](NOTICE).
