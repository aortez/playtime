"""Independent stdlib oracle. Explicit fixture authoring only; never run by Cargo.

Inputs are synthetic except the already committed public model definition.
Review changes to these bytes; do not regenerate to satisfy Rust failures.
"""
import hashlib
import json
import math
from pathlib import Path
import random
import struct

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True, allow_nan=False).encode()

def freeze(name, data):
    (HERE / name).write_bytes(data)

lock = json.loads((ROOT / "models/gpt-oss-20b.lock.json").read_bytes())
definition = canonical(lock["definition"])
assert len(definition) == 8845
assert hashlib.sha256(definition).hexdigest() == lock["definition_sha256"]
freeze("definition.canonical.json", definition)
vectors = [
    {"epsilon": 1e-5, "false": False, "float": 1.0, "integer": 1, "unicode": "é😀", "zero": 0},
    {"a": [-0.0, 1e20, 1e-7], "z": None},
    {"capabilities": ["completion", "tools", "thinking"], "parameters": None, "template": "line\nline"},
    {"escapes": '"\\\b\f\n\r\t\x00\x1f\x7f\u2028\uffff😀', "\ue000": 1, "😀": 2,
     "ints": [-9223372036854775808, 18446744073709551615],
     "numbers": [0.0, -0.0, 1e-4, 1e-5, 1e15, 1e16, 1e23, 5e-324, 1.7976931348623157e308,
                 1.2345678901234567, 9007199254740992.0]},
]
rng = random.Random(55)
numbers = []
while len(numbers) < 256:
    n = struct.unpack('>d', rng.getrandbits(64).to_bytes(8, 'big'))[0]
    if math.isfinite(n):
        numbers.append(n)
vectors.append(numbers)
for i, value in enumerate(vectors):
    freeze(f"canonical-{i}.input.json", json.dumps(value, ensure_ascii=False, indent=2).encode())
    freeze(f"canonical-{i}.expected.json", canonical(value))
freeze("canonical-manifest.json", json.dumps([
    {"length": len(canonical(v)), "sha256": hashlib.sha256(canonical(v)).hexdigest()}
    for v in vectors
], indent=2).encode() + b'\n')

# Expected request is authored from the contract, not the Rust encoder.
request = {"messages": [{"role": "user", "content": "fixture é"}],
           "tools": [{"type": "function", "function": {"name": "fixture", "parameters": {"type": "object"}}}],
           "options": {"num_predict": 512, "seed": 17, "num_ctx": 32768, "temperature": 0.0,
                       "top_p": 0.0, "top_k": 0, "repeat_penalty": 0.0, "use_mmap": False},
           "model": "gpt-oss:20b", "stream": True, "keep_alive": "5m", "think": "low"}
freeze("request.expected.json", canonical(request))
call = {"id": "fixture-call", "function": {"index": 0, "name": "fixture", "arguments": {"text": "é😀"}}, "extra": "retained"}
frames = [
    {"model": "gpt-oss:20b", "message": {"role": "assistant", "thinking": "first é"}, "done": False, "eval_count": 999},
    {"message": {"thinking": "😀", "content": "", "tool_calls": [call]}, "done": False},
    {"message": {"content": "final", "tool_calls": [call]}, "done": True, "eval_count": 32, "prompt_eval_count": 48},
]
wire = b'\r\n'.join(json.dumps(v, ensure_ascii=False, separators=(',', ':')).encode() for v in frames)
freeze("stream.ndjson", wire)  # Deliberately no last LF.
freeze("stream.expected.json", canonical({"role": "assistant", "thinking": "first é😀", "content": "final", "tool_calls": [call, call]}))
freeze("http-parse-error.json", b'{"error":"error parsing tool call: synthetic HTTP failure"}')
freeze("stream-parse-error.ndjson", b'{"message":{"thinking":"partial"},"done":false}\n{"error":"error parsing tool call: synthetic stream failure","done":true,"eval_count":0}\n')
freeze("success.ndjson", b'{"message":{"content":"synthetic"},"done":true,"eval_count":32}\n')
