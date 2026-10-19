# Installing the Craft Cloud app

The app comes with the agent, and installs for you alone: no admin rights, and nothing on the desktop.
`cargo xtask package` builds the package for the system it runs on into `target/package/`.

- **Windows:** run `craft-cloud-setup.exe`. It installs into `%LOCALAPPDATA%\Programs\Craft Cloud`, adds Craft
  Cloud to the Start menu (`--no-start-menu` leaves that out) and to "Apps & features", which uninstalls it.
  `--quiet` installs without asking anything. Running it again upgrades: the agent finishes what it is doing,
  stops, and starts again with the new files. An earlier install made by hand in
  `%LOCALAPPDATA%\Programs\craft-cloud` is taken over (its files are left alone).
- **macOS:** unzip and move `Craft Cloud.app` to Applications. The build isn't signed, so the first time, open
  it from its context menu (Open) to tell macOS it's fine.
- **Linux:** unpack `craft-cloud-<version>-linux-x86_64.tar.gz` and run `./install.sh` in it. It installs into
  `~/.local/lib/craft-cloud`, links `craft-cloud` and `craft-cloud-agent` into `~/.local/bin` and adds Craft
  Cloud to the app menu. Running it again upgrades. `./uninstall.sh` removes it.

Uninstalling keeps your synced folder and the agent's data folder (the sign-in).

Open Craft Cloud and enter a join string: an invite from the server's admin (`craft-cloud-server user invite`),
or a code from a computer that is signed in (Add a computer there). Then this computer starts syncing by itself
at every login. A computer that was signed in before (with `login`) shows its account at once.

The tray icon says whether everything is synced, syncing, offline, paused or needs you, and has Open, Open
folder, Pause and Quit (which stops syncing until the next login). On Windows and macOS, closing the window leaves
it in the tray. On Linux the tray icon is a small process of its own (`craft-cloud --tray`, what starts at login),
because Wayland can't hide a window: closing the window ends it, and the tray's Open brings it back. The icon needs a
StatusNotifierItem host (waybar's tray, KDE, GNOME with the AppIndicator extension), and appears whenever one is
there; without one the window is the way in, and the agent keeps syncing either way.
