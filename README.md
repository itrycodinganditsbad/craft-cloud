# Craft Cloud

Self-hosted sync for the Craft App Suite, in Rust. You run the server on your own computer, NAS or VPS,
and sign in once on each machine. From then on, every Craft app knows your account, its recent files are on
all your machines, and the folder `~/Craft Cloud` stays in sync, with version history.

There is no hosted service and no account with anyone but you. Nothing phones home.

> **Status: early development.** File sync, shared recent files and the Craft Cloud app work. Live
> documents (an edit shows on your other machines as you type) are next.

- **Big files sync quickly.** Files are split into content-defined chunks, so a change to a 20 GB project
  uploads only what changed, and an interrupted upload resumes.
- **Nothing is lost.** Every version is kept for a set time. When two machines change the same file, both
  copies are kept, and a sync that would delete many files asks first.
- **Still plain files.** The apps save normal files into the synced folder, so backups and other tools see
  real files. Everything keeps working offline.

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

## Documentation

- [Installing the Craft Cloud app](docs/install.md) on Windows, macOS and Linux
- [Running the server](docs/server.md): reverse proxy, Postgres and S3, accounts, invites, backups
- [The agent on the command line](docs/agent.md): `login`, `run`, `sync`, `join`, start at login
- [Development](docs/development.md): how it fits together, building, testing, the sync engine's rules
- Design: [`sync.md`](docs/design/sync.md) and [`live.md`](docs/design/live.md). Contributors and AI agents
  start with [`AGENTS.md`](AGENTS.md).

## License

MIT OR Apache-2.0, at your option ([`LICENSE-MIT`](LICENSE-MIT), [`LICENSE-APACHE`](LICENSE-APACHE),
[`NOTICE`](NOTICE)). Craft Cloud is an independent project built for the Craft App Suite, not made or
endorsed by the ArtCraft Team, and not compatible with any vendor's cloud service.
