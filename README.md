# Craft Cloud

Self-hosted sync for the Craft App Suite, in Rust. You run the server on your own computer, NAS or VPS,
and sign in once on each machine. From then on, every Craft app knows your account, its recent files are on
all your machines, and the folder `~/Craft Cloud` stays in sync, with version history.

There is no hosted service and no account with anyone but you. Nothing phones home.

> **Status: early development.** File sync, shared recent files and the Craft Cloud app work. Team spaces
> (a synced folder shared between accounts, with viewer, editor and owner roles) work in the server, the agent and the
> app. Live documents (an edit shows on your other machines as you type) work in the agent and the server, and the app
> kit lets a Craft app take them up with one adapter; the apps take them up next. The server also has admin pages in the
> browser, blob storage on disk, S3 or an SFTP server, backups to an SFTP server, and a Docker install with an update
> notice.

- **Big files sync quickly.** Files are split into content-defined chunks, so a change to a 20 GB project
  uploads only what changed, and an interrupted upload resumes.
- **Nothing is lost.** Every version is kept for a set time. When two machines change the same file, both
  copies are kept, and a sync that would delete many files asks first.
- **Still plain files.** The apps save normal files into the synced folder, so backups and other tools see
  real files. Everything keeps working offline.
- **Shared with a team, if you want.** An administrator makes a team space and names its owner; owners invite other
  accounts. An administrator has no access to a team space's files unless they are a member.
- **Looked after from a browser.** The server can serve admin pages for accounts, invites, team spaces, storage and the
  audit log, in light and dark and on a phone. They are off until you turn them on.
- **Yours to host.** Run the server from a binary or with the Docker setup (Caddy, TLS, SQLite or Postgres). Blobs can live
  on disk, S3 or an SFTP server, and the server can back itself up to an SFTP server. In Docker the admin pages can tell you
  when a newer version exists and show the command that updates it, if you turn the update check on (it is off by
  default); the server never updates itself.

## Quick start

Start a server (plain HTTP on `127.0.0.1:8080`; put a TLS reverse proxy in front of it for other machines):

```sh
cargo build --release -p craft-cloud-server
printf '%s\n' 'choose a password' | ./target/release/craft-cloud-server init --dir ./cc-data --admin you
./target/release/craft-cloud-server serve --config ./cc-data/config.toml
./target/release/craft-cloud-server user --config ./cc-data/config.toml invite you   # a join string (set public_url in config.toml)
```

Then install the Craft Cloud app on each machine (`cargo xtask package` builds the installer for the
system it runs on) and enter the join string. To add another computer, click **Add a computer** on one
that is signed in, and enter its code on the new one.

Or with Docker (Compose 2.32 or newer, on Linux): `cd packaging/docker && ./install.sh --domain cloud.example.org --admin you`.
See [Running the server](docs/server.md#docker).

## The admin pages in the browser

Light and dark, from a test server with made-up accounts. [The admin pages](docs/web-admin.md) shows each page.

| Overview, with the update notice | A team space, dark | Phone width |
|---|---|---|
| ![Overview, light](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/overview-1280-light.png) | ![A team space, dark](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/space-1280-dark.png) | ![Team spaces on a phone, dark](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/web-admin/spaces-390-dark.png) |

## The app

The Craft Cloud app, from a test setup with a made-up account. The overview shows the sync state, the storage in use and the computers on the account; Files lists the recent files of every app and computer; Shared lists invitations and team spaces.

| Overview | Files | Shared |
|---|---|---|
| ![The app's overview](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/app/overview-folders.png) | ![The app's recent files](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/app/files-loaded.png) | ![The app's team spaces and invitations](https://raw.githubusercontent.com/Skyfuller1303/craft-cloud/screenshots/app/shared-list.png) |

[Installing the Craft Cloud app](docs/install.md) shows how to add a computer.

## Documentation

- [Installing the Craft Cloud app](docs/install.md) on Windows, macOS and Linux
- [Running the server](docs/server.md): reverse proxy, Postgres, S3 and SFTP, accounts, invites, team spaces, backups, Docker
- [The admin pages in the browser](docs/web-admin.md): what each page does, with screenshots
- [The agent on the command line](docs/agent.md): `login`, `run`, `sync`, `join`, start at login
- [Development](docs/development.md): how it fits together, building, testing, the sync engine's rules
- [The app kit](docs/kit.md): how a Craft app makes its documents live
- [Rust app kit integration guide](docs/kit-integration.md): adding the kit to a Rust app
- [Local API protocol reference](docs/protocol-reference.md): the agent's local API, for apps in other languages
- Design: [`sync.md`](docs/design/sync.md) and [`live.md`](docs/design/live.md). Contributors and AI agents
  start with [`AGENTS.md`](AGENTS.md).

## License

MIT OR Apache-2.0, at your option ([`LICENSE-MIT`](LICENSE-MIT), [`LICENSE-APACHE`](LICENSE-APACHE),
[`NOTICE`](NOTICE)). Craft Cloud is an independent project built for the Craft App Suite, not made or
endorsed by the ArtCraft Team, and not compatible with any vendor's cloud service.
