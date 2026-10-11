# Craft Cloud

Self-hosted sync and storage for the Craft App Suite, in pure Rust.

Run the server on your own computer, NAS or VPS. Your project files, app settings, brushes, fonts,
presets and other asset libraries then follow you between machines, with version history. It works
for one person with a laptop and a desktop, and for a small studio with many accounts.

There is no hosted service and no account with anyone but you. Nothing phones home.

> **Status: early development.** The server runs; the sync agent is next. The design is in
> [`docs/design/sync.md`](docs/design/sync.md).

## Running the server

```sh
cargo build --release -p craft-cloud-server
# Create a data directory (SQLite + local disk) and the first admin. The password is read from stdin.
printf '%s\n' 'choose a password' | ./target/release/craft-cloud-server init --dir ./cc-data --admin you
./target/release/craft-cloud-server serve --config ./cc-data/config.toml
```

The server speaks plain HTTP on `127.0.0.1:8080`. To reach it from other machines, put a TLS reverse proxy
(Caddy, nginx, Traefik …) in front, set `listen` to the proxy-facing address, `behind_proxy = true`, and the
proxy's address in `trusted_proxies`. The proxy must also set request header and body timeouts: the server relies on it to cut off slow clients.
For a non-loopback `--listen`, `init` takes `--behind-proxy` alongside it, for example
`init --dir ./cc-data --admin you --listen 0.0.0.0:8080 --behind-proxy`; `trusted_proxies` must then list the proxy's address.
For Postgres or S3, edit `[database]` and `[storage]` in `config.toml`.
More accounts: `printf '%s\n' 'their password' | craft-cloud-server user add --config cc-data/config.toml name`.

## Planned

- **Sync**: a synced folder on each machine; large files are split into content-defined chunks, so
  only changed parts are uploaded and transfers can resume. Selective sync per folder. When two
  machines change the same file, both copies are kept. Every version is kept for a set time.
- **Collaboration** (after Sync): sharing, review comments and team libraries.

Craft Cloud is an independent project built for the Craft App Suite. It is not made or endorsed by
the ArtCraft Team, and it is not compatible with any vendor's cloud service.

## Building

```sh
cargo xtask ci
```

## License

MIT OR Apache-2.0, at your option. See [`LICENSE-MIT`](LICENSE-MIT), [`LICENSE-APACHE`](LICENSE-APACHE)
and [`NOTICE`](NOTICE).
