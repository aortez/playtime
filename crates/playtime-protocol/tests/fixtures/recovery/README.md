# Synthetic recovery v1 goldens

These are deliberately synthetic schema/recovery evidence, not a runnable
package or a completed filesystem recovery transaction. The worker source,
executable and toolchain labels explicitly say synthetic. All referenced files
exist and have genuine SHA-256/byte lengths. `hashes.json` records their exact
bytes, including JSONL line feeds; no placeholder digests are used.

`build_synthetic.py` authors expectations with Python's JSON encoder and hashlib,
independently of the Rust planner and query facade. Tests read the checked-in
bytes and do not invoke it. Regeneration requires review of the intended schema
change, not merely a failing Rust comparison. The scenario is the existing
known-zero/rejected-command fixture; the evidence stream is independently
authored and is not claimed to be that scenario's execution.

Deliberate additions compared with the existing archive protocol:

- The execution contract freezes the v1 profile, exact worker/config references,
  original nonzero allocations and all storage limits. The advertised optional
  bundle capability is explicit; old capability encodings still omit it.
- Binding/attempt/provenance and maintenance limits are separate from acquired
  generation. The attempt timestamp is persisted, not chosen by encoding.
- Worker and public bounds name checkpoint 1 at sequence 4. The current marker
  reaches checkpoint 2 at sequence 7, representing a lost acknowledgment.
- The plan charges r1's measured 10 and adds r2's original 100 allowance exactly
  once. It preserves agent_finished and checkpoint 2. Base cost is 2,272 bytes.
- The 1,425-byte patch contains only request_finished and run_finished, with
  unknown cleanup. Both records refer to the exact 5,064-byte plan and use the
  attempt time. Original events omit recovery_origin and retain their old shape.
- Selection names the exact plan, private inventory, patch and target marker.
  It counts 14 unique ordinary references, including the plan. The plan never
  references its own digest, patch, target marker or encoded patch size.
- Resolution fixtures describe all three wire variants. Their evidence digests
  name actual synthetic bytes; they do not grant authority to abandon a run.

`review-vectors.json` is the issue's 27 semantic vectors, preserved separately
from archive goldens. Its older issue baseline is provenance, not this branch's
implementation base. Reentry tests classify evidence only; no test helper is a
production recovery writer, retry or takeover API.
