#!/usr/bin/env bash
# Runs the workbench on a private headless display against a sandbox daemon.
# The display, D-Bus session, accessibility bus and settings all live in one
# private directory, so nothing appears on or changes the desktop session.
set -euo pipefail

usage() {
    cat >&2 <<'USAGE'
usage: ./scripts/workbench-sandbox.sh [--dir DIR] start [--bundle DIR] [--state-dir DIR]
                                      [--connection-file FILE] [--size WxH]
       ./scripts/workbench-sandbox.sh [--dir DIR] ui | stop | status
       ./scripts/workbench-sandbox.sh [--dir DIR] cli ARGS...

start   Builds the binaries, then starts Xvfb, a private D-Bus session with its
        own accessibility bus, a --cpu-test-supervisor daemon (the scripted
        simulation: no Docker, model or GPU) and the workbench. The first start
        also builds a worker bundle unless --bundle names one.
        --connection-file attaches the workbench to an existing daemon instead.
        The driver then refuses Launch, Retry and Cancel unless given --force.
ui      Closes and reopens the workbench. Runs and the daemon are unaffected.
stop    Stops everything it started and keeps DIR for inspection.
cli     Runs the playtime CLI against the sandbox, for example `cli runs list`.

Drive the window with scripts/workbench-drive.py. DIR defaults to
/tmp/playtime-workbench-$UID; keep it short because of Unix socket path limits.
USAGE
    exit 2
}

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
dir=${PLAYTIME_SANDBOX:-/tmp/playtime-workbench-$(id -u)}
if [[ ${1-} == --dir ]]; then
    [[ $# -ge 2 ]] || usage
    dir=$2
    shift 2
fi
[[ $# -ge 1 ]] || usage
command=$1
shift
dir=$(realpath -m -- "$dir")
icd=/usr/share/vulkan/icd.d/lvp_icd.json

fail() {
    echo "workbench-sandbox: $*" >&2
    exit 1
}
pid_of() {
    if [[ -f $dir/pids/$1 ]]; then cat "$dir/pids/$1"; fi
}
# Signals only a recorded process that still runs the expected program.
owned() {
    local pid=$2
    [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null \
        && tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q -- "$1"
}
program() {
    case $1 in
        xvfb) echo Xvfb ;;
        bus) echo dbus-daemon ;;
        daemon) echo playtimed ;;
        ui) echo playtime-ui ;;
    esac
}
running() {
    owned "$(program "$1")" "$(pid_of "$1")"
}
# Waits for a condition while the named process stays alive.
await() {
    local seconds=$1 name=$2 what=$3
    shift 3
    local deadline=$((SECONDS + seconds))
    until "$@"; do
        running "$name" || fail "$name exited while starting; see $dir/logs/$name.log"
        ((SECONDS < deadline)) || fail "timed out waiting for $what; see $dir/logs/$name.log"
        sleep 0.1
    done
}
load() {
    [[ -f $dir/env ]] || fail "no sandbox in $dir; run start first"
    # shellcheck source=/dev/null
    source "$dir/env"
}
window_mapped() {
    python3 - "$DISPLAY" <<'PY'
import sys
from Xlib import display
d = display.Display(sys.argv[1])
windows = d.screen().root.query_tree().children
sys.exit(0 if any(w.get_wm_name() == 'Playtime' and w.get_attributes().map_state == 2 for w in windows) else 1)
PY
}
# at-spi-bus-launcher persists accessibility properties through GSettings. With
# a real backend and the shared HOME, setting one changes the desktop's dconf.
isolated() {
    local pid line ours memory
    for pid in $(pgrep -u "$(id -u)" -f at-spi-bus-launcher || true); do
        ours=0
        memory=0
        while IFS= read -r -d '' line; do
            if [[ $line == "DBUS_SESSION_BUS_ADDRESS=unix:path=$dir/bus"* ]]; then ours=1; fi
            if [[ $line == GSETTINGS_BACKEND=memory ]]; then memory=1; fi
        done < <(cat "/proc/$pid/environ" 2>/dev/null)
        if ((ours)); then
            ((memory))
            return
        fi
    done
    return 1
}
# Processes the private bus activated: the accessibility bus and its registry.
helpers() {
    local pid
    for pid in $(pgrep -u "$(id -u)" -f 'at-spi|dconf-service|dbus-daemon' || true); do
        if tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | grep -qF "unix:path=$dir/"; then
            echo "$pid"
        fi
    done
}

start_ui() {
    load
    nohup env -i HOME="$HOME" PATH="$PATH" DISPLAY="$DISPLAY" \
        DBUS_SESSION_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS" XDG_RUNTIME_DIR="$dir/xdg-runtime" \
        XDG_CONFIG_HOME="$dir/xdg-config" XDG_STATE_HOME="$dir/xdg-state" GSETTINGS_BACKEND=memory \
        VK_DRIVER_FILES="$icd" WGPU_BACKEND=vulkan RUST_LOG=warn \
        "$UI" --connection-file "$CONNECTION_FILE" --state-dir "$STATE_DIR" \
        >>"$dir/logs/ui.log" 2>&1 &
    echo $! >"$dir/pids/ui"
    await 60 ui "the workbench window" window_mapped
    # AccessKit registers the window's tree shortly after the window maps.
    await 30 ui "the accessibility tree" tree_ready
}
tree_ready() {
    python3 "$repo/scripts/workbench-drive.py" --dir "$dir" wait PLAYTIME --timeout 1 >/dev/null 2>&1
}

start() {
    local bundle='' state='' connection='' size=1440x900
    while [[ $# -gt 0 ]]; do
        case $1 in
            --bundle | --state-dir | --connection-file | --size)
                [[ $# -ge 2 ]] || usage
                case $1 in
                    --bundle) bundle=$(realpath -- "$2") ;;
                    --state-dir) state=$(realpath -m -- "$2") ;;
                    --connection-file) connection=$(realpath -- "$2") ;;
                    --size) size=$2 ;;
                esac
                shift 2
                ;;
            *) usage ;;
        esac
    done
    [[ $size =~ ^[0-9]+x[0-9]+$ ]] || usage
    for name in xvfb bus daemon ui; do
        if running "$name"; then fail "already running in $dir; stop it first"; fi
    done
    for tool in Xvfb dbus-daemon gdbus import python3 cargo; do
        command -v "$tool" >/dev/null \
            || fail "missing $tool (Ubuntu: xvfb dbus libglib2.0-bin imagemagick python3)"
    done
    python3 -c 'import Xlib, gi; gi.require_version("Atspi", "2.0")' 2>/dev/null \
        || fail "missing python3-xlib or gir1.2-atspi-2.0"
    [[ -f $icd ]] || fail "missing $icd (Ubuntu: mesa-vulkan-drivers)"
    install -d -m 700 "$dir" "$dir/pids" "$dir/logs" "$dir/shots" "$dir/xdg-config" \
        "$dir/xdg-state" "$dir/xdg-runtime"
    state=${state:-$dir/state}

    (cd "$repo" && cargo build --locked -q -p playtime-server --bin playtimed \
        -p playtime-cli --bin playtime -p playtime-ui --bin playtime-ui)
    local bin
    bin=$(cd "$repo" && cargo metadata --format-version 1 --no-deps \
        | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')/debug
    if [[ -z $connection && -z $bundle ]]; then
        bundle=$dir/bundle
        if [[ ! -e $bundle ]]; then
            (cd "$repo" && cargo run --locked -q -p xtask -- bundle-worker --output "$bundle")
        fi
    fi

    # Xvfb picks a free display and writes its number once it accepts clients.
    : >"$dir/display"
    nohup Xvfb -displayfd 3 -screen 0 "${size}x24" -nolisten tcp -noreset +extension XTEST \
        3>"$dir/display" >"$dir/logs/xvfb.log" 2>&1 &
    echo $! >"$dir/pids/xvfb"
    await 10 xvfb "the display" grep -q '[0-9]' "$dir/display"
    local display
    display=:$(tr -dc 0-9 <"$dir/display")

    rm -f "$dir/bus"
    nohup env -i HOME="$HOME" PATH="$PATH" XDG_RUNTIME_DIR="$dir/xdg-runtime" \
        XDG_CONFIG_HOME="$dir/xdg-config" GSETTINGS_BACKEND=memory \
        dbus-daemon --session --nofork --address="unix:path=$dir/bus" >"$dir/logs/bus.log" 2>&1 &
    echo $! >"$dir/pids/bus"
    await 10 bus "the session bus" test -S "$dir/bus"
    local bus=unix:path=$dir/bus
    DBUS_SESSION_BUS_ADDRESS=$bus gdbus call --session --dest org.a11y.Bus \
        --object-path /org/a11y/bus --method org.a11y.Bus.GetAddress >/dev/null
    if ! isolated; then
        stop
        fail "the accessibility launcher would use real settings; stopped before changing any"
    fi
    # accesskit_unix publishes the window's tree only while a screen reader is on.
    DBUS_SESSION_BUS_ADDRESS=$bus gdbus call --session --dest org.a11y.Bus \
        --object-path /org/a11y/bus --method org.freedesktop.DBus.Properties.Set \
        org.a11y.Status ScreenReaderEnabled '<true>' >/dev/null

    local external=1
    if [[ -z $connection ]]; then
        external=0
        install -d -m 700 "$dir/root"
        (cd "$dir" && exec nohup "$bin/playtimed" --root "$dir/root" --bundle "$bundle" \
            --cpu-test-supervisor --listen 127.0.0.1:0) >"$dir/logs/daemon.log" 2>&1 &
        echo $! >"$dir/pids/daemon"
        await 60 daemon "the sandbox daemon" grep -q '^playtimed http' "$dir/logs/daemon.log"
        connection=$dir/root/.playtime/connection.json
    fi
    {
        printf 'PLAYTIME_SANDBOX=%q\n' "$dir"
        printf 'DISPLAY=%q\n' "$display"
        printf 'DBUS_SESSION_BUS_ADDRESS=%q\n' "$bus"
        printf 'SHOTS=%q\n' "$dir/shots"
        printf 'CONNECTION_FILE=%q\n' "$connection"
        printf 'STATE_DIR=%q\n' "$state"
        printf 'EXTERNAL=%q\n' "$external"
        printf 'UI=%q\n' "$bin/playtime-ui"
        printf 'CLI=%q\n' "$bin/playtime"
    } >"$dir/env"
    start_ui
    echo "Workbench on $display in $dir"
    if ((external)); then
        echo "Attached to $connection: Launch, Retry and Cancel need --force in the driver."
    else
        sed -n 's/^playtimed /Sandbox daemon: /p' "$dir/logs/daemon.log"
    fi
    echo "Drive it with: python3 scripts/workbench-drive.py --dir $dir tree"
}

stop() {
    local name pid
    for name in ui daemon bus xvfb; do
        pid=$(pid_of "$name")
        if owned "$(program "$name")" "$pid"; then kill -TERM "$pid"; fi
    done
    for name in ui daemon bus xvfb; do
        pid=$(pid_of "$name")
        for _ in $(seq 100); do
            owned "$(program "$name")" "$pid" || break
            sleep 0.1
        done
        if owned "$(program "$name")" "$pid"; then
            echo "workbench-sandbox: $name ($pid) is still running" >&2
        else
            rm -f "$dir/pids/$name"
        fi
    done
    for pid in $(helpers); do
        kill -TERM "$pid" 2>/dev/null || true
    done
}

status() {
    load
    local name pid
    for name in xvfb bus daemon ui; do
        pid=$(pid_of "$name")
        if running "$name"; then echo "$name: running ($pid)"; else echo "$name: stopped"; fi
    done
    echo "display: $DISPLAY"
    echo "connection: $CONNECTION_FILE"
    echo "screenshots: $SHOTS"
    # shellcheck disable=SC2153 # EXTERNAL comes from the sourced env file.
    if [[ $EXTERNAL == 1 ]]; then
        echo "external daemon: Launch, Retry and Cancel need --force"
    fi
}

restart_ui() {
    load
    if running ui; then
        python3 "$repo/scripts/workbench-drive.py" --dir "$dir" close
        for _ in $(seq 100); do
            running ui || break
            sleep 0.1
        done
        running ui && fail "the workbench did not close"
    fi
    start_ui
}

case $command in
    start) start "$@" ;;
    stop) [[ $# -eq 0 ]] || usage; stop ;;
    status) [[ $# -eq 0 ]] || usage; status ;;
    ui) [[ $# -eq 0 ]] || usage; restart_ui ;;
    cli)
        load
        XDG_STATE_HOME=$dir/xdg-state XDG_CONFIG_HOME=$dir/xdg-config \
            "$CLI" --connection-file "$CONNECTION_FILE" "$@"
        ;;
    *) usage ;;
esac
