# Streaming JSON Lines, version 1

Implement `JSONLinesParser(max_record_bytes)` in `jsonlines.py`, using Python
3.12's standard library. The limit must be a positive integer (not bool); invalid
limits raise ValueError. Methods are `feed(chunk: bytes) -> list[object]` and
`finish() -> list[object]`. Valid feed arguments are bytes.

Parse newline-delimited UTF-8 JSON, preserving record order and JSON value types.
Chunks may split anywhere, including inside UTF-8 characters. Accept LF and CRLF.
Ignore lines containing only ASCII JSON whitespace (space, tab, CR). Finish parses
an unterminated final record. A single record may be any JSON value, including null.
Reject malformed UTF-8, malformed JSON, and nonstandard NaN/Infinity constants.

Count record bytes before LF, including any CR and whitespace. Equality with the
limit is allowed. Enforce the cap for blank lines too, and immediately during feed
when an unfinished record exceeds it. Do not first copy/split/decode an arbitrarily
large input chunk: storage for an unfinished record must be bounded by the limit.
The caller-owned chunk and returned parsed values are excluded from that bound.
An oversized 16 MiB record with a 32-byte limit must use less than 1 MiB of extra
Python allocations before rejecting it.

Malformed input or an oversized record raises ValueError and permanently closes
the parser. After an error or successful finish, both methods raise RuntimeError.
Feed may return multiple completed records; finish returns the final record or [].
No partial-result guarantee is required for a feed that encounters an error.

Write runnable tests and README.md explaining usage, memory bounds, and checks
actually run. Do not access the network or use external packages.

Adapted from Q21 of the Reddit Brutal Model Test. Names, errors, strict JSON and
allocation ceiling are pinned here for automated tool-assisted assessment.
