#!/bin/sh
# Removes Craft Cloud for this user: stops the agent, removes its start at login and the app's, the programs, their
# links, the app menu's entry and the icon. The synced folder and the agent's data folder (the sign-in) stay.
set -eu

lib="$HOME/.local/lib/craft-cloud"
bin="$HOME/.local/bin"
config="${XDG_CONFIG_HOME:-$HOME/.config}"
agent="$lib/craft-cloud-agent"

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
rm -f "$HOME/.local/share/applications/craft-cloud.desktop" "$HOME/.local/share/icons/hicolor/256x256/apps/craft-cloud.png"

echo "Craft Cloud is removed. Your synced folder and its files stay, and so does the sign-in."
