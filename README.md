# Craft Cloud

Self-hosted sync and storage for the Craft App Suite, in pure Rust.

Run the server on your own computer, NAS or VPS. Your project files, app settings, brushes, fonts,
presets and other asset libraries then follow you between machines, with version history. It works
for one person with a laptop and a desktop, and for a small studio with many accounts.

There is no hosted service and no account with anyone but you. Nothing phones home.

> **Status: design.** There is no working code yet. The design is in [`docs/design/`](docs/design/).

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
