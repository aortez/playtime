#!/usr/bin/env bash
# Same-UID namespace qualification; privileged installation/account tests are separate.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
/usr/bin/bwrap --version
PLAYTIME_NATIVE_TESTS=1 python3 -m unittest discover -s scripts/chat -p test_native_tool.py -v
cargo test -p playtime-chat --lib native_project::tests --locked -- --include-ignored
