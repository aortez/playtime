#!/usr/bin/env bash
# Build Playtime from this checkout and run it.
#
# The daemon runs as a systemd user service on a persistent data directory, so
# it outlives this script and the workbench, and a running experiment is never
# interrupted. The script builds the worker bundle and release builds of the playtimed, playtime
# and playtime-ui binaries, starts or restarts the daemon with the recipe
# catalog and the live model endpoint, prints the useful commands and opens the
# workbench.
set -euo pipefail

usage() {
    cat <<'HELP'
Usage: ./go.sh [--no-build] [--stop] [--help]

  --no-build   Skip the build and reuse the last bundle and binaries.
  --stop       Stop the daemon. Refused while an experiment is running.

Environment:
  PLAYTIME_DATA    Data directory: bundles, and the daemon root with its runs.
                   Default: /home/data2/playtime when /home/data2 exists,
                   otherwise $XDG_DATA_HOME/playtime. The daemon refuses a root
                   with a group- or world-writable parent.
  PLAYTIME_OLLAMA  Ollama endpoint. Default: http://oldman-bird.local:11434.
  PLAYTIME_UNIT    systemd user unit name. Default: playtimed-go.
  PLAYTIME_NO_UI   When set, don't open the workbench.
HELP
}

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
unit=${PLAYTIME_UNIT:-playtimed-go}
ollama=${PLAYTIME_OLLAMA:-http://oldman-bird.local:11434}
if [[ -n ${PLAYTIME_DATA:-} ]]; then
    data=$PLAYTIME_DATA
elif [[ -d /home/data2 && -w /home/data2 ]]; then
    data=/home/data2/playtime
else
    data=${XDG_DATA_HOME:-$HOME/.local/share}/playtime
fi
[[ $data = /* ]] || data=$PWD/$data
root=$data/root
build=1
stop=0

for arg in "$@"; do
    case $arg in
        --no-build) build=0 ;;
        --stop) stop=1 ;;
        -h | --help) usage; exit 0 ;;
        *) echo "go.sh: unknown option: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

say() { printf '\033[1m%s\033[0m\n' "$*"; }
die() { echo "go.sh: $*" >&2; exit 1; }

# A playtime-<32 hex>.service unit is a worker, so an experiment is running.
worker_active() {
    systemctl --user list-units 'playtime-*' --state=active --no-legend 2>/dev/null \
        | grep -E -q 'playtime-[0-9a-f]{32}\.service'
}
daemon_active() { systemctl --user is-active --quiet "$unit"; }

if (( stop )); then
    if worker_active; then
        die "an experiment is running; cancel it first, then stop the daemon"
    fi
    if daemon_active; then
        systemctl --user stop "$unit"
        say "Stopped $unit."
    else
        say "$unit is not running."
    fi
    exit 0
fi

cd "$here"
[[ -f Cargo.toml && -d recipes ]] || die "$here is not a Playtime checkout"

# The daemon refuses a root with a group- or world-writable parent, so create
# directories private and check every parent before building anything.
(umask 077 && mkdir -p "$data" "$data/bundles" "$root")
chmod 700 "$data" "$data/bundles" "$root"
dir=$data
while [[ $dir != / ]]; do
    dir=$(dirname "$dir")
    if (( (8#$(stat -c %a "$dir") & 8#022) != 0 )); then
        die "$dir is group- or world-writable and the daemon refuses such a parent; use another PLAYTIME_DATA or run: chmod g-w,o-w $dir"
    fi
done
avail_kb=$(df -Pk "$data" | awk 'NR == 2 { print $4 }')
if (( avail_kb < 20 * 1024 * 1024 )); then
    say "Warning: $((avail_kb / 1024 / 1024)) GiB free under $data. A ten-session run can take about 400 MB."
fi

if (( build )); then
    if daemon_active && worker_active; then
        die "an experiment is running under $unit, so the daemon can't be restarted with a new build; use --no-build to open the workbench on it"
    fi
    if command -v rustup > /dev/null && ! rustup target list --installed 2> /dev/null | grep -q '^x86_64-unknown-linux-musl$'; then
        die "the worker bundle needs the musl target: rustup target add x86_64-unknown-linux-musl"
    fi
    bundle=$data/bundles/$(date +%Y%m%dT%H%M%S)-$(git rev-parse --short HEAD 2> /dev/null || echo nogit)
    say "Building the worker bundle: $bundle"
    cargo run --quiet --locked -p xtask -- bundle-worker --output "$bundle"
    say "Building playtimed, playtime and playtime-ui"
    cargo build --quiet --locked --release -p playtime-server --bin playtimed -p playtime-cli --bin playtime \
        -p playtime-ui --bin playtime-ui
    printf '%s\n' "$bundle" > "$data/bundle.latest"
    # Keep the three newest bundles. Runs keep their own frozen copies.
    find "$data/bundles" -mindepth 1 -maxdepth 1 -type d | sort | head -n -3 | xargs -r rm -rf
    if daemon_active; then
        # An experiment may have started while the build ran.
        if worker_active; then
            die "an experiment started during the build, so $unit keeps its old build; run $0 again after it ends"
        fi
        say "Restarting $unit with the new build."
        systemctl --user stop "$unit"
    fi
elif ! daemon_active; then
    bundle=$(cat "$data/bundle.latest" 2> /dev/null || true)
    [[ -n $bundle && -d $bundle ]] || die "no bundle recorded under $data; run without --no-build first"
    [[ -x target/release/playtimed ]] || die "no target/release/playtimed; run without --no-build first"
fi

conn=$root/.playtime/connection.json
if ! daemon_active; then
    systemctl --user reset-failed "$unit" 2> /dev/null || true
    rm -f "$conn"
    say "Starting $unit on $root"
    systemd-run --user --quiet --unit "$unit" --description "Playtime daemon from $here" \
        --working-directory "$here" \
        "$here/target/release/playtimed" --listen 127.0.0.1:0 --root "$root" --bundle "$bundle" \
        --docker-scripted --ollama-endpoint "$ollama" --recipes "$here/recipes"
    for _ in $(seq 240); do
        [[ -f $conn ]] && break
        if ! daemon_active; then
            journalctl --user -u "$unit" --no-pager -n 20 -o cat >&2
            die "$unit exited during startup; its log is above"
        fi
        sleep 0.5
    done
    [[ -f $conn ]] || die "$unit published no connection file in 120 s; see: journalctl --user -u $unit"
fi

endpoint=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["endpoint"])' "$conn")
cli=("$here/target/release/playtime" --connection-file "$conn")
say "Daemon $unit at $endpoint, root $root"
if worker_active; then
    say "An experiment is running. A new launch gets run_busy until it ends."
fi
others=$(systemctl --user list-units 'playtimed-*' --state=active --no-legend 2> /dev/null \
    | awk '{ print $1 }' | grep -v "^$unit.service$" || true)
[[ -z $others ]] || say "Other Playtime daemons are active: $others"
say "Recipes:"
"${cli[@]}" recipes list || true
cat <<INFO

Workbench   $here/target/release/playtime-ui --connection-file $conn
Runs        ${cli[*]} runs list
Cancel      ${cli[*]} runs cancel RUN_ID --wait
Release     ${cli[*]} runs release RUN_ID --reason TEXT
Daemon log  journalctl --user -u $unit -f
Stop        $0 --stop

INFO
if [[ -n ${PLAYTIME_NO_UI:-} ]]; then
    exit 0
fi
say "Opening the workbench. Closing it leaves the daemon and any experiment running."
exec "$here/target/release/playtime-ui" --connection-file "$conn"
