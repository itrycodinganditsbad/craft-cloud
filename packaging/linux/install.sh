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

# The agent's data folder, as the agent finds it (it ignores a relative XDG_DATA_HOME), where a running agent's
# local-api.json names its pid.
case "${XDG_DATA_HOME:-}" in
    /*) data_home=$XDG_DATA_HOME ;;
    *) data_home=$HOME/.local/share ;;
esac
data="${CRAFT_CLOUD_AGENT_DIR:-$data_home/craft-cloud-agent}"

agent_pid() {
    sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$data/local-api.json" 2>/dev/null | head -n 1
}

has_unit() {
    [ -f "$unit" ] && command -v systemctl >/dev/null 2>&1
}

# Starts the agent again once the process $1 (or, with no pid known, the one whose local-api.json is in the data
# folder) has ended and so let go of the data folder: an agent started before that finds the folder taken and exits.
# Through systemd when $2 is "systemd", else `$3 run --log`. In the background: the wait may be long.
start_after_exit() {
    nohup sh -c '
        n=0
        if [ -n "$1" ]; then
            while kill -0 "$1" 2>/dev/null && [ "$n" -lt 86400 ]; do
                n=$((n + 1))
                sleep 1
            done
        else
            while [ -e "$4/local-api.json" ] && [ "$n" -lt 86400 ]; do
                n=$((n + 1))
                sleep 1
            done
            sleep 5
        fi
        if [ "$2" = systemd ]; then
            systemctl --user start craft-cloud-agent.service
        else
            "$3" run --log
        fi
    ' sh "$1" "$2" "$3" "$data" >/dev/null 2>&1 &
}

# The agent that ran and was stopped is started again (once): this computer keeps syncing.
restarted=0
start_again() {
    restarted=1
    if has_unit; then
        # `stop` waits for whatever the unit still runs to end; then the unit starts the agent.
        systemctl --user stop craft-cloud-agent.service || true
        systemctl --user start craft-cloud-agent.service || true
    elif [ -x "$lib/craft-cloud-agent" ]; then
        nohup "$lib/craft-cloud-agent" run --log >/dev/null 2>&1 &
    fi
}

# Gives up waiting: nothing has been changed. The agent has accepted the quit and stops after its pass, so it is
# started again once it has: through systemd, else the copy installed already or the one on PATH (never this unpacked
# one, which may be moved or deleted).
give_up() {
    again=", and it starts again once done"
    if has_unit; then
        start_after_exit "$pid" systemd ""
    elif [ -x "$lib/craft-cloud-agent" ]; then
        start_after_exit "$pid" run "$lib/craft-cloud-agent"
    elif command -v craft-cloud-agent >/dev/null 2>&1; then
        start_after_exit "$pid" run "$(command -v craft-cloud-agent)"
    else
        again=" (it stops once done; start it again with 'craft-cloud-agent run')"
    fi
    echo "The Craft Cloud agent is still finishing its sync; nothing was changed$again. Try again in a while." >&2
    exit 1
}

# An agent that answers is asked to quit until none does: it finishes the sync under way first, which may take a
# while for big files. Then its process is waited for (it lets go of the data folder last). Both together take at most
# CRAFT_CLOUD_INSTALL_WAIT seconds, 10 minutes by default. Nothing is changed before.
wait_for="${CRAFT_CLOUD_INSTALL_WAIT:-600}"
was_running=0
stopped=0
tries=0
# Read before the quit: the agent removes the file as it stops.
pid=$(agent_pid)
while "$here/craft-cloud-agent" quit >/dev/null 2>&1; do
    was_running=1
    [ -n "$pid" ] || pid=$(agent_pid)
    tries=$((tries + 1))
    if [ "$tries" = 3 ]; then
        echo "Waiting for the Craft Cloud agent to finish its sync…"
    fi
    if [ "$tries" -ge "$wait_for" ]; then
        give_up
    fi
    sleep 1
done
if [ "$was_running" = 1 ]; then
    if [ -n "$pid" ]; then
        while kill -0 "$pid" 2>/dev/null; do
            tries=$((tries + 1))
            if [ "$tries" -ge "$wait_for" ]; then
                give_up
            fi
            sleep 1
        done
    else
        # An agent that doesn't name its pid: it lets go of the data folder a moment after its API.
        sleep 5
    fi
    stopped=1
fi

# From here on the agent is stopped: whatever ends the script (a failed copy too) starts it again.
trap 'if [ "$stopped" = 1 ] && [ "$restarted" = 0 ]; then start_again; fi' EXIT

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

if [ "$stopped" = 1 ]; then
    start_again
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
