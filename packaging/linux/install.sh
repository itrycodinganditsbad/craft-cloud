#!/bin/sh
# Installs Craft Cloud for this user, from the folder this script is in (the unpacked tarball):
#   ~/.local/lib/craft-cloud/      craft-cloud (the app), craft-cloud-agent and LICENSES.txt
#   ~/.local/bin/                  links to both
#   ~/.local/share/applications/   craft-cloud.desktop (the app menu's entry), unless --no-menu
#   ~/.local/share/icons/          the icon
# Run again to upgrade: an agent that runs is stopped first (it finishes the pass under way) and started again after,
# and an agent that starts at login is pointed at the new copy. Nothing goes on the desktop. uninstall.sh removes it.
# --no-menu leaves the app menu's entry out (and removes one an earlier install made).
set -eu

menu=1
for arg in "$@"; do
    case "$arg" in
        --no-menu) menu=0 ;;
        *)
            echo "unknown option $arg: use --no-menu" >&2
            exit 2
            ;;
    esac
done

here=$(cd "$(dirname "$0")" && pwd)
lib="$HOME/.local/lib/craft-cloud"
bin="$HOME/.local/bin"
apps="$HOME/.local/share/applications"
icons="$HOME/.local/share/icons/hicolor/256x256/apps"
config="${XDG_CONFIG_HOME:-$HOME/.config}"
unit="$config/systemd/user/craft-cloud-agent.service"
entry="$config/autostart/craft-cloud-agent.desktop"

# Starts the agent ($1) again once the one running has stopped: it stops answering, and lets go of its data folder a
# moment later (an agent started before that finds it taken and exits). Through systemd when $2 is "systemd", else
# `run --log`. In the background: the wait may be long.
start_when_stopped() {
    nohup sh -c '
        n=0
        while "$1" quit >/dev/null 2>&1 && [ "$n" -lt 86400 ]; do
            n=$((n + 1))
            sleep 1
        done
        sleep 3
        if [ "$2" = systemd ]; then
            systemctl --user start craft-cloud-agent.service
        else
            "$1" run --log
        fi
    ' sh "$1" "$2" >/dev/null 2>&1 &
}

has_unit() {
    [ -f "$unit" ] && command -v systemctl >/dev/null 2>&1
}

# An agent that answers is asked to quit until none does: it finishes the sync under way first, which may take a
# while for big files (at most CRAFT_CLOUD_INSTALL_WAIT seconds, 10 minutes by default). Nothing is changed before.
wait_for="${CRAFT_CLOUD_INSTALL_WAIT:-600}"
was_running=0
tries=0
while "$here/craft-cloud-agent" quit >/dev/null 2>&1; do
    was_running=1
    tries=$((tries + 1))
    if [ "$tries" = 3 ]; then
        echo "Waiting for the Craft Cloud agent to finish its sync…"
    fi
    if [ "$tries" -ge "$wait_for" ]; then
        # It has accepted the quit and stops after its pass: it is started again then, so this computer keeps
        # syncing. The copy installed already, else the one on PATH, else this one.
        if has_unit; then
            start_when_stopped "$here/craft-cloud-agent" systemd
        elif [ -x "$lib/craft-cloud-agent" ]; then
            start_when_stopped "$lib/craft-cloud-agent" run
        elif command -v craft-cloud-agent >/dev/null 2>&1; then
            start_when_stopped "$(command -v craft-cloud-agent)" run
        else
            start_when_stopped "$here/craft-cloud-agent" run
        fi
        echo "The Craft Cloud agent is still finishing its sync; nothing was changed, and it starts again once done. Try again in a while." >&2
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
cp "$here/LICENSES.txt" "$lib/LICENSES.txt"
cp "$here/craft-cloud.png" "$icons/craft-cloud.png"

if [ "$menu" = 1 ]; then
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
else
    rm -f "$apps/craft-cloud.desktop"
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$apps" >/dev/null 2>&1 || true
fi

# The agent's start at login (a systemd unit or an autostart entry, made by it or by hand) runs this copy from now on.
if [ -f "$unit" ] || [ -f "$entry" ]; then
    "$lib/craft-cloud-agent" autostart on >/dev/null || echo "Start at login still runs the old agent: run 'craft-cloud-agent autostart on'." >&2
fi

if [ "$was_running" = 1 ]; then
    if has_unit; then
        systemctl --user restart craft-cloud-agent.service || true
    else
        start_when_stopped "$lib/craft-cloud-agent" run
    fi
fi

if [ "$menu" = 1 ]; then
    echo "Craft Cloud is installed in $lib. Open it from the app menu, or run craft-cloud."
else
    echo "Craft Cloud is installed in $lib. Run craft-cloud to open it."
fi
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "($bin isn't on your PATH, so type $bin/craft-cloud to start it from a terminal.)" ;;
esac
