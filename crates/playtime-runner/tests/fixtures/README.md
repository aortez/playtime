# Durable mapping fixture

`known-zero-rejected.json` is a synthetic CPU-only scenario for the real
incremental archive driver. It delivers a draft, then a replacing final response
with measured zero generation, an unsupported tool, an exec with missing exit
status, a successful handoff, and a tool rejected after the session ended. It
uses self-written memory without an evidence appendix.

`known-zero-rejected-mapping.json` pins selected actual archive records, archive
sequences, original controller sequences and bindings, and exact LF-inclusive
SHA-256. The executable/manifest event is deliberately excluded because the
worker identity truthfully changes with the compiling executable. No selected
record depends on that binary hash. Unknown command status remains unknown;
rejected tools have original request identities but no external effect IDs.

The fixture was authored from a release `archive_simulation` invocation and
reviewed as a new contract, not by a test regeneration mode. The test executes
the controller and writer again and compares these fields and hashes. Existing
engine/protocol fixtures independently cover multi-session self-plus-evidence,
non-dispatched intents, missing usage/partial failure, and final text replacing
stream previews. Do not rewrite these goldens to conceal a behavior change.
