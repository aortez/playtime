#!/usr/bin/env bash
set -euo pipefail
if [[ $# -ne 2 || $1 != --case || ( $2 != sandbox && $2 != checkpoint ) ]]; then
    echo 'usage: ./scripts/test-rust-docker-e2e.sh --case sandbox|checkpoint' >&2
    echo 'Production composition (#24 D3) remains a separate gate.' >&2
    exit 2
fi
case_name=$2
cd "$(dirname "$0")/.."
task_root=$(mktemp -d "${TMPDIR:-/tmp}/playtime-docker.XXXXXXXX")
chmod 700 "$task_root"
echo "Real Docker $case_name evidence: $task_root"
cargo build --locked -p playtime-docker --example "$case_name-e2e"
# Explicit invocation never skips missing/unsupported Docker or restores images.
# The normal/failure paths perform exact owned cleanup. A hard harness deadline
# retains proposals for operator inspection; it never broadly deletes resources.
timeout --signal=TERM --kill-after=5s 240s "${CARGO_TARGET_DIR:-target}/debug/examples/$case_name-e2e" \
    "$task_root" "${PLAYTIME_DOCKER_EXECUTABLE:-/usr/bin/docker}" \
    "${PLAYTIME_DOCKER_SOCKET:-/var/run/docker.sock}" environments/general.lock.json
