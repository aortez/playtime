#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 2 || $1 != --backend || ( $2 != docker-scripted && $2 != live ) ]]; then
    echo 'usage: ./scripts/test-rust-continuous.sh --backend docker-scripted|live' >&2
    exit 2
fi
if systemctl --user list-units 'playtime-*.service' --state=active --no-legend --plain \
    | grep -Eq '^playtime-[0-9a-f]{32}\.service'; then
    echo 'A Playtime worker is active; run native checks sequentially.' >&2
    exit 1
fi
cd "$(dirname "${BASH_SOURCE[0]}")/.."
playtime_continuous_root=$(mktemp -d "${TMPDIR:-/tmp}/pc.XXXXXXXX")
echo "Continuous native evidence (including failed attempts): $playtime_continuous_root"
cargo run --locked -p xtask -- bundle-worker --output "$playtime_continuous_root/bundle"
playtime_continuous_args=("$playtime_continuous_root/bundle" "$playtime_continuous_root/run")
if [[ $2 == live ]]; then
    playtime_continuous_args+=("${PLAYTIME_LIVE_ENDPOINT:-http://127.0.0.1:11435}")
fi
cargo run --locked -p playtime-server --example continuous-native-e2e -- "${playtime_continuous_args[@]}"
