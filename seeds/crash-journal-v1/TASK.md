# Crash recovery for an interleaved journal, version 1

Implement `recover(data)` in `journal.py`, using Python 3.12's standard library.
`data` must be bytes (otherwise raise TypeError). Return exactly
`{"state": dict[str, str], "committed": list[int], "valid_bytes": int}`.
The initial state is empty. Do not perform file or network I/O.

A frame is `b"PTJ1" + N.to_bytes(4, "big") + payload + CRC.to_bytes(4, "big")`.
N is the payload byte length, at most 16,384. CRC is
`zlib.crc32(payload) & 0xffffffff`. Payload is UTF-8 JSON. The supplied `wire.py`
encodes example frames correctly; it is a helper, not a decoder or test oracle.

Read the contiguous valid prefix. Stop at the first incomplete frame, wrong
magic, oversized length, wrong CRC, invalid UTF-8/JSON, or invalid record.
Never search for another magic marker after an error. `valid_bytes` is the
offset immediately after the last accepted frame, including valid frames of
transactions that have not committed. Do not count any part of the bad frame.
An invalid/torn suffix does not roll back earlier commits.

Payload must be one of these JSON arrays, with exactly the shown elements:

- `["begin", tx]`: open a previously unused transaction ID.
- `["put", tx, key, value]`: queue a string value for a string key.
- `["del", tx, key]`: queue deletion of a string key; absent keys are harmless.
- `["commit", tx]`: atomically apply this transaction's queued operations in
  their record order to the current committed state, then close it and append
  its ID to `committed`.
- `["abort", tx]`: discard the queued operations and close the transaction.

IDs are JSON integers from 0 through 2**31-1; booleans/floats are invalid.
Every operation except begin requires an active transaction. IDs cannot be
reused after commit or abort. Keys/values are strings, including empty strings
and Unicode. Transactions can interleave; commits apply in commit-record order,
not begin order. Pending/aborted writes never become visible. A deletion applies
to the committed state at commit time, not a snapshot taken at begin time.
Malformed record shapes, types, operations or transaction lifecycles end the
valid prefix just like corruption. An empty committed transaction is valid.

Write runnable tests and README.md explaining the recovery invariant. Include
crash points inside headers, payloads and CRCs, an interleaved commit-order
example, and corruption followed by an otherwise valid frame. Report checks
actually run. Automated scoring checks behavior; prose is reviewed separately.
