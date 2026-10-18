#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
playtime_guard_root="$(mktemp -d "${TMPDIR:-/tmp}/pt-guard.XXXXXXXX")"
trap 'rm -rf "$playtime_guard_root"' EXIT
export TMPDIR="$playtime_guard_root"
cargo test --locked -p playtime-protocol --test backend_guard
cargo test --locked -p playtime-backend-guard -- --nocapture
