#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
case "${1:-cpu}" in
    cpu)
        cargo test --locked -p playtime-backend-host
        ;;
    systemd)
        export PLAYTIME_BACKEND_SYSTEMD_TESTS=1
        cargo test --locked -p playtime-backend-host --test systemd -- --ignored --skip owner_entry --nocapture --test-threads=1
        ;;
    *) echo 'usage: test-rust-backend-host.sh [cpu|systemd]' >&2; exit 2 ;;
esac
