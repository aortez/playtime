#!/usr/bin/env bash
# Install a self-contained chat launcher outside project tool roots. With
# --hold-only, install only what a daemon's Strata holds use: the wrapper,
# verifier, presets and reviewed locks, without building the chat binaries.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
hold_only=0
if [[ $# == 1 && $1 == --hold-only ]]; then
  hold_only=1
elif [[ $# != 0 ]]; then
  echo 'usage: scripts/install-chat-launcher.sh [--hold-only]' >&2
  exit 2
fi
destination="${PLAYTIME_CHAT_INSTALL_DIR:-$HOME/.local/lib/playtime-chat}"
case "$destination" in
  /*) ;;
  *) echo 'PLAYTIME_CHAT_INSTALL_DIR must be absolute' >&2; exit 1 ;;
esac
if (( ! hold_only )); then
  cargo build --release --locked -p playtime-chat
fi
install -d -m 700 "$destination"
if (( ! hold_only )); then
  install -m 755 target/release/playtime-chat "$destination/playtime-chat"
  install -m 755 target/release/playtime-task "$destination/playtime-task"
fi
install -m 755 scripts/chat/run-strata.py scripts/chat/verify-strata-host.py scripts/chat/project_environment.py \
    scripts/chat/llama-session.py "$destination/"
install -m 644 scripts/chat/launch_protocol.py "$destination/"
install -m 600 scripts/chat/presets.json models/framework-strata-gyro-m-fast-v2.lock.json \
    models/bird-strata-gyro-m-fast-v2.lock.json models/bird-basalt-iq4-v1.lock.json \
    models/bird-basalt-nvfp4-v1.lock.json models/local-llama-lfm2.5-350m-v1.lock.json \
    models/local-llama-qwen3-1.7b-v1.lock.json models/local-llama-qwen3.5-0.8b-v1.lock.json \
    environments/general.lock.json "$destination/"
install -m 600 workshop/tool.py "$destination/tool.py"
if (( hold_only )); then
  printf 'Installed the Playtime launcher for Strata holds in %s\n' "$destination"
else
  printf 'Installed Playtime chat in %s\n' "$destination"
  printf 'Add the [[harness]] entry from docs/rust-chat-launcher.md to codex-launcher.toml.\n'
fi
