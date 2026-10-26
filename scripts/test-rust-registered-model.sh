#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 0 && ( $# != 1 || $1 != --continuous ) ]]; then
    echo 'usage: PLAYTIME_LIVE_ENDPOINT=http://127.0.0.1:11435 ./scripts/test-rust-registered-model.sh [--continuous]' >&2
    exit 2
fi
if systemctl --user list-units 'playtime-*.service' --state=active --no-legend --plain \
    | grep -Eq '^playtime-[0-9a-f]{32}\.service'; then
    echo 'A Playtime worker is active; run one live-model check at a time.' >&2
    exit 1
fi
cd "$(dirname "${BASH_SOURCE[0]}")/.."
playtime_registered_root=$(mktemp -d "${TMPDIR:-/tmp}/pr.XXXXXXXX")
echo "Registered-model evidence (including failed attempts): $playtime_registered_root"
cargo run --locked -p xtask -- bundle-worker --output "$playtime_registered_root/bundle"
cargo build --locked -p playtime-server --bin playtimed -p playtime-ui \
    --example registered-model-e2e --features ui-snapshots --message-format=json-render-diagnostics \
    > "$playtime_registered_root/build.jsonl"
python3 - "$playtime_registered_root" "${PLAYTIME_LIVE_ENDPOINT:-http://127.0.0.1:11435}" "${1:-}" <<'PLAYTIME_REGISTERED_GATE'
import json
import os
import pathlib
import subprocess
import sys
import time

root = pathlib.Path(sys.argv[1])
artifacts = {}
for line in (root / 'build.jsonl').read_text().splitlines():
    item = json.loads(line)
    if item.get('reason') == 'compiler-artifact' and item.get('executable'):
        artifacts[item['target']['name']] = item['executable']

# Reuse the existing reviewed live smoke's limits unchanged. This small prompt
# tests the selected-model plumbing, not the GPT-OSS persistence acceptance task.
scenario = json.loads(pathlib.Path('crates/playtime-runner/src/docker/live.json').read_text())
recipe = {
    'id': 'dev-smoke', 'name': 'Development smoke source', 'workspace_entries': 512,
    'prompt': 'Use the controller session number. In Session 1, call exec with cwd /work and command "printf local-qwen-ok > probe.txt", then call end_session with finish_run=false and a memento asking the next session to read probe.txt. In Session 2, call read_file for /work/probe.txt, then call end_session with finish_run=true and report what you read. Act directly using tools.',
    'protocol': scenario['protocol'], 'inventory': scenario['inventory'],
}
continuous = sys.argv[3] == '--continuous'
if continuous:
    # Same common allocations as the reviewed step-B native compaction probe.
    scenario['parameters']['limits'].update(wall_seconds=300, generated_tokens=16384,
        context_tokens=16384, model_seconds=45, output_bytes=2048, log_bytes=16 << 20)
    recipe['prompt'] = 'Repeatedly call exec with exactly these arguments: {"command":"bash /work/seeds/continuous-probe.sh","cwd":"/work","timeout_seconds":5}. The working directory is /work, never the script filename. This task requires 48 separate successful exec calls, one per response. Each call advances one stage. Continue until its output contains continuity-ok, then call finish_run alone with a short summary. Do not inspect or modify the script or other files. After compaction continue calling exec with those same arguments.'
    seed_dir = root / 'seeds'
    seed_dir.mkdir()
    (seed_dir / 'continuous-probe.sh').write_bytes(pathlib.Path('crates/playtime-runner/src/docker/continuous-probe.sh').read_bytes())
    recipe['seeds'] = 'seeds'
lines = [f'{key} = {json.dumps(value)}' for key, value in recipe.items()]
for name, table in [('parameters', {k: v for k, v in scenario['parameters'].items() if not isinstance(v, dict)}),
                    ('parameters.limits', scenario['parameters']['limits']),
                    ('parameters.workshop', scenario['parameters']['workshop']),
                    ('model', dict(thinking='high', temperature=0.8, top_p=0.95, top_k=40, repeat_penalty=1.0))]:
    lines.append(f'\n[{name}]')
    lines.extend(f'{key} = {json.dumps(value)}' for key, value in table.items())
catalog = root / 'recipes'
catalog.mkdir()
(catalog / 'dev-smoke.toml').write_text('\n'.join(lines) + '\n')
service = root / 'service'
service.mkdir(mode=0o700)
connection = service / '.playtime/connection.json'
with (root / 'daemon.log').open('w') as log:
    daemon = subprocess.Popen([artifacts['playtimed'], '--root', str(service), '--bundle', str(root / 'bundle'),
                               '--listen', '127.0.0.1:0', '--docker-scripted', '--ollama-endpoint', sys.argv[2],
                               '--recipes', str(catalog)], stdout=log, stderr=subprocess.STDOUT,
                              env={**os.environ, 'PATH': '/no-runtime-cargo'})
    try:
        deadline = time.monotonic() + 30
        while not connection.exists():
            if daemon.poll() is not None or time.monotonic() >= deadline:
                sys.exit('daemon readiness failed; see retained daemon.log')
            time.sleep(0.05)
        result = subprocess.run([artifacts['registered-model-e2e'], str(connection), str(root / 'evidence')] + (['--continuous'] if continuous else []), timeout=900)
        sys.exit(result.returncode)
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=15)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait()
PLAYTIME_REGISTERED_GATE
