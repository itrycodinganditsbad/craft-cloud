# Attribution

Every non-code asset in this repository, with its author, source and license. Craft Cloud's own
code and original assets are MIT OR Apache-2.0 (see [`LICENSE-MIT`](LICENSE-MIT),
[`LICENSE-APACHE`](LICENSE-APACHE) and [`NOTICE`](NOTICE)). When you add an asset, add a row here
in the same change; third-party assets must be permissively licensed and keep their license file
next to them.

| Path | Title | Author | Source | License |
|---|---|---|---|---|
| `apps/craft-cloud-agent/tests/tls/localhost.cert.der`, `localhost.key.der` | Self-signed test certificate for `localhost` and its key (tests only, never trusted outside the TLS test) | Craft Cloud contributors | Made with `openssl req -x509` for this repository | MIT OR Apache-2.0 |
| `apps/craft-cloud-agent/tests/autostart/*` | Expected start-at-login entries per OS (golden files for the autostart tests) | Craft Cloud contributors | Written for this repository | MIT OR Apache-2.0 |
| `apps/craft-cloud-desktop/assets/icon.png`, `icon.ico`, `icon.icns` | Craft Cloud's app icon: a white cloud on a blue rounded square | Craft Cloud contributors | Drawn by `cargo xtask icons` (`xtask/src/icons.rs`) for this repository | MIT OR Apache-2.0 |
| `apps/craft-cloud-desktop/assets/tray-*.png` | Craft Cloud's tray icon in each state (synced, syncing, offline, needs attention, paused): a cloud with a badge | Craft Cloud contributors | Drawn by `cargo xtask icons` (`xtask/src/icons.rs`) for this repository | MIT OR Apache-2.0 |
| `apps/craft-cloud-desktop/tests/autostart/*` | Expected start-at-login entries of the app per OS (golden files) | Craft Cloud contributors | Written for this repository | MIT OR Apache-2.0 |
| `apps/craft-cloud-desktop/tests/snapshots/*.png` | The Craft Cloud app's screens as its snapshot tests expect them (the text in egui's default fonts, see `epaint_default_fonts`) | Craft Cloud contributors | Rendered by the tests (`UPDATE_SNAPSHOTS=1`) | MIT OR Apache-2.0 |
