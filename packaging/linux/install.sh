#!/bin/sh
# Installs Craft Cloud for this user, from the folder this script is in (the unpacked tarball):
#   ~/.local/lib/craft-cloud/      craft-cloud (the app) and craft-cloud-agent
#   ~/.local/bin/                  links to both
#   ~/.local/share/applications/   craft-cloud.desktop (the app menu's entry)
#   ~/.local/share/icons/          the icon
# Run again to upgrade: an agent that runs is stopped first (it finishes the pass under way) and started again after,
# and an agent that starts at login is pointed at the new copy. Nothing goes on the desktop. uninstall.sh removes it.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
lib="$HOME/.local/lib/craft-cloud"
bin="$HOME/.local/bin"
apps="$HOME/.local/share/applications"
icons="$HOME/.local/share/icons/hicolor/256x256/apps"
config="${XDG_CONFIG_HOME:-$HOME/.config}"
unit="$config/systemd/user/craft-cloud-agent.service"
entry="$config/autostart/craft-cloud-agent.desktop"

# An agent that answers is asked to quit until none does (at most a minute).
was_running=0
tries=0
while "$here/craft-cloud-agent" quit >/dev/null 2>&1; do
    was_running=1
    tries=$((tries + 1))
    if [ "$tries" -ge 60 ]; then
        echo "The Craft Cloud agent doesn't stop; try again in a minute." >&2
        exit 1
    fi
    sleep 1
done

mkdir -p "$lib" "$bin" "$apps" "$icons"
for name in craft-cloud craft-cloud-agent; do
    # A new file renamed over the old one: a running copy keeps its own.
    cp "$here/$name" "$lib/.$name.new"
    chmod 755 "$lib/.$name.new"
    mv -f "$lib/.$name.new" "$lib/$name"
    ln -sf "$lib/$name" "$bin/$name"
done
cp "$here/craft-cloud.png" "$icons/craft-cloud.png"

cat >"$apps/craft-cloud.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Craft Cloud
Comment=Join your Craft Cloud and see its sync
Exec="$lib/craft-cloud"
Icon=craft-cloud
Terminal=false
Categories=Network;FileTransfer;
StartupWMClass=craft-cloud
EOF
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$apps" >/dev/null 2>&1 || true
fi

# The agent's start at login (a systemd unit or an autostart entry, made by it or by hand) runs this copy from now on.
if [ -f "$unit" ] || [ -f "$entry" ]; then
    "$lib/craft-cloud-agent" autostart on >/dev/null || echo "Start at login still runs the old agent: run 'craft-cloud-agent autostart on'." >&2
fi

if [ "$was_running" = 1 ]; then
    if [ -f "$unit" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl --user restart craft-cloud-agent.service || true
    else
        nohup "$lib/craft-cloud-agent" run --log >/dev/null 2>&1 &
    fi
fi

echo "Craft Cloud is installed in $lib. Open it from the app menu, or run craft-cloud."
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "($bin isn't on your PATH, so type $bin/craft-cloud to start it from a terminal.)" ;;
esac
