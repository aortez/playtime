#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 2 || $1 != --case || $2 != normal ]]; then
    echo 'usage: ./scripts/test-rust-service-e2e.sh --case normal (lifecycle/recovery remain later #20 slices)' >&2
    exit 2
fi
cd "$(dirname "${BASH_SOURCE[0]}")/.."
playtime_service_root=$(mktemp -d "${TMPDIR:-/tmp}/playtime-service.XXXXXXXX")
trap 'rm -rf -- "$playtime_service_root"' EXIT
# Explicit build stage. Neither daemon admission nor worker execution runs Cargo.
cargo run --locked -p xtask -- bundle-worker --output "$playtime_service_root/bundle"
cargo build --locked -p playtime-server -p playtime-cli -p playtime-ui --bin playtimed --bin playtime --example service-e2e --message-format=json-render-diagnostics > "$playtime_service_root/build.jsonl"
python3 - "$playtime_service_root" <<'PY'
import json
import pathlib
import subprocess
import sys
root = pathlib.Path(sys.argv[1])
artifacts = {}
for line in (root / 'build.jsonl').read_text().splitlines():
    item = json.loads(line)
    if item.get('reason') == 'compiler-artifact' and item.get('executable'):
        artifacts[item['target']['name']] = item['executable']
required = ['service-e2e', 'playtimed', 'playtime']
if any(name not in artifacts for name in required):
    sys.exit('Missing required service gate executables')
try:
    result = subprocess.run([artifacts['service-e2e'], '--daemon', artifacts['playtimed'], '--cli', artifacts['playtime'], '--bundle', str(root / 'bundle')], timeout=120)
    sys.exit(result.returncode)
except subprocess.TimeoutExpired:
    sys.exit('Normal service gate exceeded 120 seconds')
PY
