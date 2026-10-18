# Archive layout v1 goldens

These three files add storage expectations to #17's unchanged synthetic
two-session fixture. They are not evidence of a controller execution.

`archive.json` pins the new immutable descriptor. The existing input manifest
is exactly 2,160 bytes with SHA-256
`deaaf65a541b0b247f9e78eafeecb4a0aae63cf16bbefbc72d1cec9e6942d904`.

`empty.commit.json` describes the zero-byte prefix (sequence zero and the
standard SHA-256 empty digest). `complete.commit.json` describes all 16 existing
JSONL records, exactly 5,314 bytes **including every LF**, with digest
`da0b40eed5ac3c07568b89c4eba56f51598633dd8da61dde87111f845da5d5f1`.
These lengths/digests were independently calculated with Python's stdlib
`pathlib.read_bytes` and `hashlib.sha256`; no JSON normalization was applied.

Storage metadata uses compact JSON in declaration order plus LF. Readers hash
the exact stored metadata bytes and accept other valid JSON formatting. Event
records, including unknown kinds and extra envelope fields, retain their exact
supplied bytes. Review any format/identity changes rather than regenerating
these goldens to match implementation output.
