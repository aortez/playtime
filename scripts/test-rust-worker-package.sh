#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
playtime_package_root="$(mktemp -d "${TMPDIR:-/tmp}/playtime-package.XXXXXXXX")"
trap 'rm -rf "$playtime_package_root"' EXIT
cargo run --locked -p xtask -- bundle-worker --output "$playtime_package_root/bundle"
cargo run --locked -p playtime-worker --example package-e2e -- --bundle "$playtime_package_root/bundle"
