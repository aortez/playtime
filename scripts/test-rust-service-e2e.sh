#!/usr/bin/env bash
set -euo pipefail
playtime_service_case=normal
if [[ $# == 2 && $1 == --case && $2 == normal ]]; then
    playtime_service_case=normal
elif [[ $# == 4 && $1 == --case && $2 == run-and-view && $3 == --backend && $4 == docker-scripted ]]; then
    playtime_service_case=run-and-view
else
    echo 'usage: ./scripts/test-rust-service-e2e.sh --case normal | --case run-and-view --backend docker-scripted' >&2
    echo 'Guarded live inference and restart/recovery remain later integration gates.' >&2
    exit 2
fi
cd "$(dirname "${BASH_SOURCE[0]}")/.."
playtime_service_root=$(mktemp -d "${TMPDIR:-/tmp}/playtime-service.XXXXXXXX")
if [[ $playtime_service_case == normal ]]; then
    trap 'rm -rf -- "$playtime_service_root"' EXIT
else
    echo "Run-and-view evidence: $playtime_service_root"
fi
# Explicit build stage. Neither daemon admission nor worker execution runs Cargo.
cargo run --locked -p xtask -- bundle-worker --output "$playtime_service_root/bundle"
playtime_service_features=()
if [[ $playtime_service_case == run-and-view ]]; then
    playtime_service_features=(--features playtime-ui/ui-snapshots)
fi
cargo build --locked -p playtime-server -p playtime-cli -p playtime-ui --bin playtimed --bin playtime --example service-e2e "${playtime_service_features[@]}" --message-format=json-render-diagnostics > "$playtime_service_root/build.jsonl"
python3 - "$playtime_service_root" "$playtime_service_case" <<'PLAYTIME_SERVICE_GATE'
import json
import pathlib
import subprocess
import sys
root = pathlib.Path(sys.argv[1])
case = sys.argv[2]
artifacts = {}
for line in (root / 'build.jsonl').read_text().splitlines():
    item = json.loads(line)
    if item.get('reason') == 'compiler-artifact' and item.get('executable'):
        artifacts[item['target']['name']] = item['executable']
required = ['service-e2e', 'playtimed', 'playtime']
if any(name not in artifacts for name in required):
    sys.exit('Missing required service gate executables')
args = [artifacts['service-e2e'], '--daemon', artifacts['playtimed'], '--cli', artifacts['playtime'], '--bundle', str(root / 'bundle')]
if case == 'run-and-view':
    args += ['--docker-scripted', '--screenshot', str(root / 'workbench.png'), '--evidence', str(root / 'evidence')]
try:
    result = subprocess.run(args, timeout=540 if case == 'run-and-view' else 120)
    sys.exit(result.returncode)
except subprocess.TimeoutExpired:
    sys.exit(f'{case} service gate exceeded its total deadline; ownership evidence is retained')
PLAYTIME_SERVICE_GATE
