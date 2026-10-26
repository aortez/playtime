set -euo pipefail
cd /work
if [[ ! -f stage ]]; then
    python3 -c 'import time
from pathlib import Path
n=0
while True:
 n+=1
 Path("counter.next").write_text(str(n))
 Path("counter.next").replace("counter")
 time.sleep(.05)' </dev/null >/dev/null 2>&1 &
    echo "$!" > counter.pid
    echo 0 > stage
    sleep .1
    cat counter > counter.before
fi
stage=$(cat stage)
stage=$((stage+1))
echo "$stage" > stage
kill -0 "$(cat counter.pid)"
if [[ "$stage" -ge 48 ]]; then
    python3 -c 'from pathlib import Path
assert int(Path("counter").read_text()) > int(Path("counter.before").read_text())
Path("result.txt").write_text("continuity-ok\n")'
    cat result.txt
else
    python3 -c 'print("probe-stage=" + open("stage").read().strip() + " of 48. TASK INCOMPLETE. Call exec again with command bash /work/seeds/continuous-probe.sh, cwd /work, timeout_seconds 5. Do not finish yet."); print("checked workspace evidence " * 100)'
fi
