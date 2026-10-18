#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 2 || $1 != --backend || ( $2 != fixture && $2 != simulation ) ]]; then
    echo 'usage: ./scripts/test-rust-ui-e2e.sh --backend fixture|simulation' >&2
    exit 2
fi
playtime_backend=$2
playtime_repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$playtime_repo"
playtime_artifacts=$(mktemp)
trap 'rm -f -- "$playtime_artifacts"' EXIT
# One build before the driver; tests never invoke Cargo recursively.
cargo build --locked -p playtime-ui -p playtime-cli -p playtime-test-support --bins --examples --message-format=json-render-diagnostics > "$playtime_artifacts"
python3 - "$playtime_artifacts" "$playtime_backend" <<'PY'
import json
import subprocess
import sys
artifacts = {}
with open(sys.argv[1]) as messages:
    for line in messages:
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get('reason') == 'compiler-artifact' and message.get('executable'):
            artifacts[message['target']['name']] = message['executable']
required = ['playtime', 'ui-e2e', 'cli_fixture']
missing = [name for name in required if name not in artifacts]
if missing:
    sys.exit('Required Cargo executables absent: ' + ', '.join(missing))
sys.exit(subprocess.call([artifacts['ui-e2e'], '--backend', sys.argv[2], '--cli', artifacts['playtime'], '--fixture', artifacts['cli_fixture']]))
PY
