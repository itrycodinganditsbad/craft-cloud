#!/bin/sh
# Removes Craft Cloud for this user: stops the agent, removes its start at login and the app's, the programs, their
# links, the app menu's entry, the icon and the app's single-instance files. The synced folder and the agent's data folder (the sign-in) stay.
set -eu

lib="$HOME/.local/lib/craft-cloud"
bin="$HOME/.local/bin"
config="${XDG_CONFIG_HOME:-$HOME/.config}"
agent="$lib/craft-cloud-agent"
# The app's single-instance files, next to the agent's data folder, as the app finds it (a relative XDG_DATA_HOME is
# ignored).
case "${XDG_DATA_HOME:-}" in
    /*) data_home=$XDG_DATA_HOME ;;
    *) data_home=$HOME/.local/share ;;
esac
data="${CRAFT_CLOUD_AGENT_DIR:-$data_home/craft-cloud-agent}"
data_root=$(dirname "$data")
app_files="$data_root/craft-cloud-app"

if [ -x "$agent" ]; then
    # Stopped first (it finishes the pass under way), so the entry is removed by this copy, not the running one.
    tries=0
    while "$agent" quit >/dev/null 2>&1 && [ "$tries" -lt 60 ]; do
        tries=$((tries + 1))
        sleep 1
    done
    "$agent" autostart off >/dev/null 2>&1 || true
fi
rm -f "$config/autostart/craft-cloud.desktop"

for name in craft-cloud craft-cloud-agent; do
    if [ -L "$bin/$name" ] && [ "$(readlink "$bin/$name")" = "$lib/$name" ]; then
        rm -f "$bin/$name"
    fi
done
rm -rf "$lib"
# Only the app's own names, and the folder once empty: anything else there stays. Older versions kept them loose.
for name in window.lock window.port window.port.tmp tray.lock tray.port tray.port.tmp; do
    rm -f "$app_files/$name"
done
rmdir "$app_files" 2>/dev/null || true
for name in craft-cloud-app.lock craft-cloud-app.port craft-cloud-tray.lock craft-cloud-tray.port; do
    rm -f "$data_root/$name"
done
rm -f "$HOME/.local/share/applications/craft-cloud.desktop" "$HOME/.local/share/icons/hicolor/256x256/apps/craft-cloud.png"

echo "Craft Cloud is removed. Your synced folder and its files stay, and so does the sign-in."
