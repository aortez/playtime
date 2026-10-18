"""Independent stdlib oracle for PR #61's shortest-decimal tie regressions.

Explicit fixture authoring only, never invoked by Cargo. Existing goldens and
model locks are not touched. Each input is frozen by its exact binary64 bits.
"""
import hashlib
import json
import math
from pathlib import Path
import struct

cases = []
# These are exact halfway values between shortest decimal spellings. Cover
# rounding both toward and away from zero, both signs, and adjacent binary64s.
for label, value in [
    ("review_positive", 132647377983681.62),
    ("review_negative", -2192903396094659.2),
    ("halfway_up", 132647377983681.88),
    ("halfway_down", 2192903396094659.2),
]:
    for sign in (1, -1):
        number = sign * value
        for direction, candidate in [
            ("lower", math.nextafter(number, -math.inf)),
            ("exact", number),
            ("upper", math.nextafter(number, math.inf)),
        ]:
            cases.append((f"{label}_{sign}_{direction}", candidate))
# Style transitions must not accidentally depend on Ryū's own pretty layout.
for value in [1e-5, 1e-4, 1e15, 1e16, 0.0, -0.0, 5e-324, 1.7976931348623157e308]:
    cases.append((f"notation_{value!r}", value))

rows = []
for label, value in cases:
    canonical = json.dumps({"x": value}, sort_keys=True, ensure_ascii=True,
                           separators=(",", ":"), allow_nan=False)
    rows.append(json.dumps({"label": label,
                            "bits": struct.pack(">d", value).hex(),
                            "canonical": canonical,
                            "sha256": hashlib.sha256(canonical.encode()).hexdigest()},
                           sort_keys=True, separators=(",", ":")))
Path(__file__).with_name("float-ties.jsonl").write_text("\n".join(rows) + "\n")
