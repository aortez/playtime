#!/usr/bin/env bash
# Install a self-contained chat launcher outside project tool roots.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
destination="${PLAYTIME_CHAT_INSTALL_DIR:-$HOME/.local/lib/playtime-chat}"
case "$destination" in
  /*) ;;
  *) echo 'PLAYTIME_CHAT_INSTALL_DIR must be absolute' >&2; exit 1 ;;
esac
cargo build --release --locked -p playtime-chat
install -d -m 700 "$destination"
install -m 755 target/release/playtime-chat "$destination/playtime-chat"
install -m 755 scripts/chat/run-strata.py scripts/chat/verify-strata-host.py "$destination/"
install -m 600 scripts/chat/presets.json models/framework-strata-gyro-m-fast-v1.lock.json \
    models/bird-strata-gyro-m-fast-v1.lock.json environments/general.lock.json "$destination/"
printf 'Installed Playtime chat in %s\n' "$destination"
printf 'Add the [[harness]] entry from docs/rust-chat-launcher.md to codex-launcher.toml.\n'
