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
| `apps/craft-cloud-desktop/assets/fonts/Archivo-Variable.ttf`, `OFL-Archivo.txt` | Archivo (variable), the app's headlines and wordmark | The Archivo Project Authors | https://github.com/Omnibus-Type/Archivo | OFL-1.1 (`OFL-Archivo.txt` next to it) |
| `apps/craft-cloud-desktop/assets/fonts/Inter-Variable.ttf`, `OFL-Inter.txt` | Inter (variable), the app's text | The Inter Project Authors | https://github.com/rsms/inter | OFL-1.1 (`OFL-Inter.txt` next to it) |
| `apps/craft-cloud-desktop/assets/fonts/GeistMono-Variable.ttf`, `OFL-GeistMono.txt` | Geist Mono (variable), the app's labels and buttons | The Geist Project Authors | https://github.com/vercel/geist-font | OFL-1.1 (`OFL-GeistMono.txt` next to it) |
| `apps/craft-cloud-desktop/tests/autostart/*` | Expected start-at-login entries of the app per OS (golden files) | Craft Cloud contributors | Written for this repository | MIT OR Apache-2.0 |
| `apps/craft-cloud-desktop/tests/snapshots/*.png` | The Craft Cloud app's screens as its snapshot tests expect them (the text in the bundled Archivo, Inter and Geist Mono, with egui's default fonts behind them for symbols) | Craft Cloud contributors | Rendered by the tests (`UPDATE_SNAPSHOTS=1`) | MIT OR Apache-2.0 |
| `apps/craft-cloud-server/src/web/style.css` | The admin pages' stylesheet: system fonts, no images, no external files | Craft Cloud contributors | Written for this repository | MIT OR Apache-2.0 |
