#!/usr/bin/env bash
# Assess captured code in the locked workshop, never by importing it on the host.
set -euo pipefail
if [[ $# != 1 ]]; then
    echo "usage: $0 /absolute/candidate-workspace" >&2
    exit 2
fi
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
candidate=$(realpath -e -- "$1")
[[ -d "$candidate" && -f "$candidate/diffusion.py" ]]
image=sha256:b98a1e67a7170f14fb3452e59b559c17b2bf03eca597e84fdda52a8e491e68cb
# Keep assessment identity explicit. Missing images fail; never pull implicitly.
docker image inspect "$image" >/dev/null
container=$(docker create --pull never --network none --read-only --user 1000:1000 \
    --cap-drop ALL --security-opt no-new-privileges --pids-limit 64 \
    --cpus 1 --memory 256m --memory-swap 256m --log-driver none \
    --tmpfs /tmp:rw,nosuid,nodev,size=16m --workdir /candidate \
    --mount "type=bind,src=$candidate,dst=/candidate,readonly" \
    --mount "type=bind,src=$repo/assessments/simulation-repair-v1/check.py,dst=/assessment/check.py,readonly" \
    --entrypoint python3 "$image" -I /assessment/check.py /candidate/diffusion.py)
trap 'docker rm -f "$container" >/dev/null' EXIT
# Timeout/infrastructure errors are incomplete assessment, not a failed score.
# The caller must require both structured output and a completed container.
set +e
timeout --signal=TERM --kill-after=5 30 docker start --attach "$container"
attach_status=$?
set -e
if [[ $attach_status != 0 ]]; then
    exit "$attach_status"
fi
docker inspect --format '{{.State.ExitCode}}' "$container" | {
    read -r candidate_status
    exit "$candidate_status"
}
