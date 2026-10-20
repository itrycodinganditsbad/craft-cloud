# AGENTS.md: guide for AI agents and contributors

Craft Cloud is a **self-hosted** sync and storage server for the **Craft App Suite** (PhotoCraft,
LightCraft, FilmCraft, DesignCraft, VectorCraft, PdfCraft, EffectCraft, WordCraft, DeckCraft,
GridCraft, CADCraft, SoundCraft). People run it on their own machine, NAS or VPS; there is no
hosted service, no account with us, and nothing phones home. Your files, settings and asset
libraries follow you across machines, and (later) can be shared with a team.

It is written in **Rust only**. Siblings with the same conventions: `../craft-launcher`,
`../photocraft`, `../vectorcraft`, `../filmcraft` and the other Craft apps. Standards shared across
the Craft apps live in `craftrules` (read its `README.md` and `AGENTS.md` when you have it checked
out). Contribute reusable learnings there, never code: repos don't share code.

## Naming

The product name is **Craft Cloud** in user-facing text. Machine names are lowercase:
`craft-cloud-*` crates and binaries. This repository is not an ArtCraft Team project: never use the
ArtCraft name, logo or marks in the product, its icon, its binaries or its docs, beyond saying in
plain text that it is built for the Craft App Suite. Every Craft app's `docs/brand/LICENSE-brand.txt`
explains why.

## 1. Workspace map

The design lives in `docs/design/sync.md`; the live protocols (the server's live endpoint, the agent's local API) in
`docs/design/live.md`.

```text
crates/
  api       craft-cloud-api       what apps need of the protocol, with no postcard: IDs, names, limits, the agent's local API (`local.rs`, `/v1/blobs` too), error codes and the account's devices (`api.rs`)
  proto     craft-cloud-proto     wire types: hashes, manifests, commit ops, API bodies, join codes and join strings (`join.rs`), the live protocol (`live.rs`); re-exports `craft-cloud-api` under the same paths
  chunker   craft-cloud-chunker   FastCDC `fastcdc-v1` + BLAKE3, frozen by golden vectors
  store     craft-cloud-store     blob store: disk, S3, memory; checks every blob in and out
  meta      craft-cloud-meta      metadata on SQLite or Postgres; the batch commit; the change feed; invites and pairings (`join.rs`); live documents' logs and snapshots (`live.rs`)
  sync      craft-cloud-sync      the sync engine over `Fs`/`Transport` traits; `MemFs`; scenario tests and the simulation
  kit       craft-cloud-kit       the app kit (`docs/kit.md`): an app's model live through the agent; the `LiveDocument` trait and `Session` (`session.rs`: open, the `local_change` hook, `poll`, undo through Loro's undo manager, reconnect and the 64 MiB queue, presence, saves), the change writer that cuts commits under 512 KiB (`writer.rs`), the mappings (`tree.rs`, `text.rs`, `table.rs`, `timeline.rs`, `opaque.rs`, `list.rs`; stories and tables nest in a node's or a cell's map: `props.rs`), values, chunk references and blob stores (`value.rs`)
  egui      craft-cloud-egui      Craft Cloud's piece of an app's window: the account line, the shared recents, a live document's badge, and `CloudWatch` that keeps them current
  app       craft-cloud-app       what apps link: a blocking client of the agent's local API (status, sign-in, joining, computers, pause, autostart, recents, live documents at the level of bytes: `live.rs`)
apps/
  craft-cloud-server              HTTP API (`/api/v1`), the live service (`/api/v1/live`, `live.rs`: logs, documents by import, snapshots and compaction, presence), join codes (`join.rs`: pairings, join, the `/join/{code}` page), auth, admin CLI (`init`, `serve`, `user` with `invite`, `device`, `reset-epoch`, `live`)
  craft-cloud-agent               the sync agent: real disk `Fs`, HTTP `Transport` over a `Connection`, saved account and state, root marker, CLI (`login`, `join`, `run`, `sync`, `status`, `logout`, `recents`, `doc` (`doc.rs`), `add-computer`, `devices`, `pause`, `resume`, `quit`, `autostart`), the `run` loop, the local API (`local_api.rs`), the live hub (`live/`: a document's Loro copy, recents document, replica (recents and documents of files), store (with the documents' journals), link, hub, the local API's docs socket), start at login (`autostart/`: Windows, macOS, Linux), the background build (`src/bin/bg.rs`, logging to `runlog.rs`)
    tests/network/                the simulation over a simulated network (turmoil): `main.rs` histories and scenarios, `harness.rs` server and agent hosts, `live.rs` live documents over WebSockets on it
    tests/recents_sim.rs          the recents convergence simulation: replicas against the server's live sessions
    tests/documents_sim.rs        the documents convergence simulation (shares `tests/docs_common/` with `documents.rs` and `live_speed.rs`)
  craft-cloud-desktop             the Craft Cloud app (`craft-cloud`, eframe/egui): the pure model (`model.rs`), the agent behind a trait and calls on threads (`agent.rs`), starting the agent (`start_agent.rs`), one app per user (`single_instance.rs`), the window (`ui/`, `app.rs`), the tray (`tray/`: tray-icon in the window's process on Windows and macOS; on Linux a StatusNotifierItem over zbus in a process of its own, `craft-cloud --tray`, `tray/process.rs`, since Wayland can't hide a window), the app's own start at login (`autostart.rs`), its icons (`assets/`, drawn by `cargo xtask icons`)
    tests/snapshots/              every screen as kittest draws it (`UPDATE_SNAPSHOTS=1` redraws them)
  craft-cloud-setup               the Windows setup (`craft-cloud-setup.exe`): install, upgrade, move from a hand-made install, `--uninstall`; the exes built in by `cargo xtask package`
  craft-cloud-kit-demo            the app kit's demo (never packaged or installed): a story and a layer tree, live through the kit; its model and adapter (`model.rs`), its window (`ui.rs`); tests drive two windows on one fake hub, and time the kit on a big document
packaging/linux/                  the Linux tarball's `install.sh` and `uninstall.sh` (into `~/.local`)
xtask/                            cargo xtask ci | licenses | icons | package | version
docs/                             design documents and the self-hosting guide
```

Layering: `api` and `chunker` depend on nothing else in the workspace; `proto` depends on `api` only. What apps link
(`app`, `kit`, `egui`) depends on `api`, never on `proto`, so it brings no crate a Craft app doesn't already lock but
Loro's own (`cargo xtask suite-deps <the apps' Cargo.lock files>` checks). `store` and `meta` depend on `proto`.
The server depends on all three; nothing depends on the server outside tests. `sync` depends on `proto` and `chunker`
(its tests also use `meta` and `store` as an in-process server). `app` depends on `api` only and has no async
runtime. The agent depends on `proto`, `sync` and `app` (its `recents` commands are clients like an app); its
tests also run the server on a loopback port, and `tests/network/` runs the server's router and agents over
`turmoil` (a dev-dependency: the shipped binaries never link it). That test target includes sync's test harness
(`crates/sync/tests/common/`) by `#[path]`, so the agent's dev-dependencies mirror sync's test ones, and a change
there can break the agent's tests: run both (`cargo xtask ci` does). The app (`craft-cloud-desktop`) depends on
`app` and `proto` only, never on the agent: it reaches the agent through the local API and starts its binary. The
setup (`craft-cloud-setup`) depends on nothing in the workspace: it stops and moves the agent through the agent's own
local API (`POST /v1/quit`, then its process is waited for and, after a minute, ended) and command line
(`autostart on`).

Backend tests (Postgres, S3) skip unless their environment variables are set; see `.github/workflows/ci.yml`
for the variables and the test containers. Set `CRAFT_CLOUD_REQUIRE_BACKENDS=1` to make a skip a failure.

## 2. Clean room

Craft Cloud reimplements the *idea* of desktop cloud sync. It is not compatible with, and never
talks to, any vendor's cloud.

- Behaviour comes from public documentation, published papers (for example the FastCDC paper for
  content-defined chunking) and black-box use of products with synthetic test files and throwaway
  accounts. Never capture real personal data, account details or someone else's files.
- **Never reverse engineer another sync service's private protocol or API.** No packet captures of
  their clients, no decompiling their apps, no copying their wire formats. Our protocol is our own and documented in `docs/design/`.
- Never copy, port or paraphrase code from GPL, AGPL or LGPL projects (Nextcloud, ownCloud,
  Seafile, and similar). Don't read their source while working on the matching feature here.
  MPL-2.0 code (Syncthing) is not copied either.
- Dependencies must be permissively licensed (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode, CC0, CDLA-Permissive-2.0 (data only, e.g. root certificates)).
  Named exceptions, for no other crate: for what Loro (the live documents' CRDT) needs, MPL-2.0 for `im`, `bitmaps`
  and `sized-chunks`, used as published and never modified or copied from, and BSL-1.0 (the Boost licence) for
  `xxhash-rust`; for the Craft Cloud app's window (eframe), BSL-1.0 for `clipboard-win` and `error-code` (the
  Windows clipboard), and OFL-1.1 and the Ubuntu Font Licence (data only) for the fonts in `epaint_default_fonts`.
  Adding one means checking its license and every transitive license. `cargo xtask licenses` (part of
  `cargo xtask ci` and CI) checks every crate in the build against this list; a data-only licence is allowed
  only for the crates named in `xtask/src/licenses.rs`.
- Never copy vendor icons, artwork, UI text or documentation wording. Feature names are fine.

## 3. Golden rules

### Never crash (outranks feature work)

A bad request, a truncated upload, a corrupt chunk, a full disk or a broken config file must
produce an error the user or admin can act on, never a panic. A crashed server is a whole studio
that can't save. Shared standard: `craftrules/standards/never-crash.md`.

- **Non-test code never panics.** No `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!` or
  `unimplemented!`. Return the crate's error type and propagate with `?`.
- Every production crate root (`lib.rs`, each binary's `main.rs`, and `xtask`) carries
  `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]`.
- **No `unsafe`.** The workspace sets `unsafe_code = "forbid"`.
- **Everything from the network is hostile**, including requests from signed-in users. Use `get()`
  instead of indexing, checked arithmetic for sizes and offsets, and cap request bodies, chunk
  sizes, JSON sizes, path lengths and list lengths.

### Never lose data

- Nothing a user uploaded is deleted except by an explicit user action or the admin's retention
  policy. Garbage collection only removes chunks that no version references, and is tested for it.
- Every chunk is verified against its hash when written and when read. A mismatch is an error, never
  silently served.
- Writes are atomic: write to a temporary file, flush, then rename. A crash at any point leaves the
  old state or the new state, never a mix.
- A sync conflict keeps both copies. The client never overwrites a local change it hasn't uploaded.

### Security

- Passwords are hashed with Argon2id. Access tokens are random, stored only as hashes, and can be
  revoked per device.
- Never log passwords, tokens, file contents or file names at info level or above.
- Every request is checked against the signed-in user's permissions on the server; never trust a
  path, ID or flag from the client to decide access.
- Paths from clients are normalised and confined; `..`, absolute paths, drive letters, NUL bytes and
  reserved Windows names are rejected.
- No telemetry, analytics or update checks that send anything about the user or their server.

### Other rules

1. **Tests are the gate.** Protocol, chunking, storage and sync logic get unit tests. The sync
   engine gets tests for conflicts, interrupted transfers and offline edits.
2. **Assets need attribution.** Every bundled non-code asset gets a row in `ATTRIBUTION.md` (path,
   title, author, source, license) in the same change.
3. When fetching anything, use a generic `Craft-Cloud/<version>` User-Agent and never put a
   person's name, email or other personal details in requests.

## 4. Before you finish a task

```sh
cargo xtask ci              # fmt --check, licenses, clippy -D warnings, test
```

Or piecemeal: `cargo test -p <crate>`, `cargo clippy -p <crate> --all-targets -- -D warnings`.
The app's Linux tray tests run a private D-Bus and need `dbus-daemon`; without it they skip, and CI
(`CRAFT_CLOUD_REQUIRE_BACKENDS=1`) fails them.

## 5. Versions

The version lives only in `[workspace.package] version` in the root `Cargo.toml`
(`cargo xtask version`, `cargo xtask version set X.Y.Z[-pre]`).

## 6. Parallel agents

- Use your own target dir (`CARGO_TARGET_DIR=target/agent-<name>`) and edit only the files you own.
- Keep every `Cargo.toml` valid at all times: the `crates/*` and `apps/*` globs mean one broken
  manifest breaks everyone's build. Write manifests to a temp file and `mv` them into place.
- If someone else's in-progress edit breaks the build, wait and retry; don't fix their files.
