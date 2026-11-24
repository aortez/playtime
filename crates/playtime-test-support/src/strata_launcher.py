# The chat launcher's hold protocol as a scripted fixture plays it: `fixture.json`
# says what each command reports, and every hold appends what its caller did
# to `calls.jsonl`. Like the real hold, it reads stdin to EOF; `linger` then
# delays its ending, as stopping a server does. It never reaches a host.
import json, os, sys, time
from pathlib import Path
here = Path(__file__).resolve().parent
script = json.loads((here / "fixture.json").read_text())
def record(entry):
    with (here / "calls.jsonl").open("a") as calls:
        calls.write(json.dumps(entry) + "\n")
args = sys.argv[1:]
if args[:1] == ["--status"]:
    print(json.dumps(script["status"]))
    sys.exit(0)
profile, command = args[0], args[1:]
if command == ["--describe-hold"]:
    print(json.dumps(script["describe"]))
    sys.exit(0)
assert command == ["--hold"], command
def emit(event, **fields):
    print(json.dumps({"schema_version": 1, "protocol": "hold_v1", "event": event,
                      "profile": profile, **fields}), flush=True)
hold = script["hold"]
record({"call": "hold", "path": os.environ.get("PATH"), "python": sys.executable})
if hold["first"] == "refused":
    emit("refused", reason=hold["reason"], message="fixture refusal", inference_owner="not_acquired")
    sys.exit(1)
if hold["first"] == "wait":
    data = sys.stdin.buffer.read()
    record({"call": "abandoned", "input": data.decode()})
    emit("refused", reason="abandoned", message="ended before held", inference_owner="confirmed")
    sys.exit(1)
emit("held", endpoint=hold["endpoint"], model_lock=hold["model_lock"],
     model_lock_sha256=hold["model_lock_sha256"], ready={}, report=hold["report"],
     helper_logs="/fixture", evidence=str(here), timings={})
data = sys.stdin.buffer.read()
ending = {b"release\n": "released", b"": "closed"}.get(data, "protocol_error")
time.sleep(hold.get("linger", 0))
record({"call": "ended", "ending": ending})
emit("ended", ending=ending, released=ending == "released", inference_owner="confirmed", owner_exit=0)
