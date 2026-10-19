# Development

## How it fits together

```
 ┌──────────────── each machine ─────────────────────┐        ┌──── self-hosted ──────────────┐
 │ Craft apps ──(plain files)──────────┐             │        │ craft-cloud-server            │
 │ Craft Cloud app, Craft apps         ▼             │ HTTPS  │  ├─ auth (users, devices)     │
 │  └ craft-cloud-app ──local API──► craft-cloud-    │◄──────►│  ├─ metadata (SQLite or PG)   │
 │    (loopback, token)              agent ◄─watch─  │  WSS   │  ├─ chunks (disk or S3)       │
 │                                    ├ state        │◄──────►│  └─ live: documents and their │
 │                                    └ live hub     │        │     update logs, fan-out      │
 │                          ~/Craft Cloud/           │        └───────────────────────────────┘
 └───────────────────────────────────────────────────┘
```

- **The server** stores file contents as chunks, either on local disk or in any S3-compatible object
  store. It keeps the metadata (users, devices, the file tree, versions and a change feed) in SQLite or
  Postgres. Every change arrives as one batch commit, which is applied completely or not at all. Its **live
  service** (`/api/v1/live`, a WebSocket) keeps each live document's log of updates, numbered, and sends
  every new one to the account's other machines at once; it never reads what is in them. Today the live
  documents are each app's recent files.
- **The agent** runs on each machine. It uploads what changed in the synced folder, applies what other
  machines changed, and keeps its own small state file. `craft-cloud-agent run` keeps the folder in step
  on its own (it watches the folder and the server); `craft-cloud-agent sync` runs one pass by hand. While
  it runs it also serves a **local API** on `127.0.0.1` (a random port and token per start, in an
  owner-only `local-api.json`), so the Craft Cloud app and the other apps can see the account's status, join
  and sign out, and share recent files; and its **live hub** keeps each app's recent files, works offline,
  and catches up with the server's live service when it can.
- **The Craft Cloud app** (`craft-cloud`) is a window, and a tray icon where the desktop has a tray. It is
  how a computer joins the cloud (with a code), shows the sync, adds another computer, lists and removes
  computers, pauses and quits. It starts the agent when none runs, and never syncs itself.
- **Apps** link `craft-cloud-app`: a small blocking client of the local API. Every app shows and adds recent
  files, which appear on the account's other machines right away. Apps never talk to the server and hold no
  credentials. The launcher lists Craft Cloud like any app.

## Building and testing

```sh
cargo xtask ci        # rustfmt, dependency licences, the icons, clippy, and every test
cargo xtask package   # the app's package for this system, in target/package/
cargo xtask icons     # draw the app's icons again (after changing xtask/src/icons.rs)
```

The app's window is tested with snapshots of every screen (`apps/craft-cloud-desktop/tests/snapshots/`), drawn
on a CPU renderer where there is one (Mesa's lavapipe on Linux, WARP on Windows); after a deliberate change to a
screen, `UPDATE_SNAPSHOTS=1 cargo test -p craft-cloud-desktop` draws them anew. On Linux the window needs the
X11, Wayland and xkbcommon headers to build (see [`ci.yml`](../.github/workflows/ci.yml)).

The Postgres and S3 tests run when their environment variables are set (see [`ci.yml`](../.github/workflows/ci.yml));
CI runs them against Postgres and SeaweedFS containers. A longer simulation run:
`SIM_SEEDS=1000 cargo test --release -p craft-cloud-sync --test simulation`, and over the simulated network
`SIM_SEEDS=1000 cargo test --release -p craft-cloud-agent --test network`, and for recents
`SIM_SEEDS=10000 cargo test --release -p craft-cloud-agent --test recents_sim`.

Contributors and AI agents: read [`AGENTS.md`](../AGENTS.md) first. It has the workspace map and the rules:
never crash, never lose data, no `unsafe`, and how the clean room works.

## The workspace

| Crate | What it does | State |
|---|---|---|
| `craft-cloud-proto` | Wire types: names and their rules, IDs, hashes, manifests, commit operations, API bodies, limits, the live protocol's frames, the local API's bodies | done |
| `craft-cloud-chunker` | FastCDC chunking (`fastcdc-v1`, pinned by golden test vectors) and BLAKE3 hashing | done |
| `craft-cloud-store` | Chunk storage on disk, S3 or memory; checks every chunk's size and hash on the way in and out | done |
| `craft-cloud-meta` | Metadata on SQLite or Postgres: users, devices, the tree, versions, the batch commit, the change feed, live documents and their update logs | done |
| `craft-cloud-server` | The HTTP API (`/api/v1`), the live service (`/api/v1/live`), authentication, rate limits and the admin CLI | done |
| `craft-cloud-sync` | The sync engine: compares the folder with the server, uploads, downloads, resolves conflicts | done |
| `craft-cloud-agent` | The agent on each machine: real disk, HTTPS transport, saved state, root marker, the local API, the live hub (recents), command line (`login`, `run`, `sync`, `status`, `logout`, `recents`) | runs on its own |
| `craft-cloud-app` | What apps link: status, sign-in and sign-out, joining, computers, pause, recent files, through the running agent | first cut |
| `craft-cloud-desktop` | The Craft Cloud app, `craft-cloud`: the window and the tray icon, joining with a code, adding and removing computers, start at login | first cut |
| `craft-cloud-setup` | The Windows setup (`craft-cloud-setup.exe`): a per-user install with an uninstaller | first cut |

## The sync engine

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
