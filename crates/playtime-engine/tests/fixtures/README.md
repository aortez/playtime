# Controller fixtures

Unless identified as captured evidence below, files here are invented,
explicitly simulated inputs. No workshop images or archived worker executables
are included.

`two-session.json` is executable by the in-memory simulator. Four model requests
spend 26 measured tokens over 400 virtual milliseconds. A workspace write and
scripted exec mutations persist into session two; temporary/process state and
conversation/context state reset. The second session receives the first memento
and a separate deterministic evidence appendix. `two-session.trace.json` fixes
the observation order, monotonic times, phases, and final decision. Tests also
compare complete executions, including payloads and artifact bytes.

`evidence.json` stresses UTF-8 filename/excerpt clipping, file allocation,
uncertain/failed/recent command priority, omitted records, upstream truncation,
and continuing selection after an oversized record. Expected text comes from
the pinned Python `build_evidence` implementation, independently rechecked by
`tests/test_rust_controller.py`. Selected command dispatches are 35 (no result),
32 (failed, truncated output), and 2 (small successful exit). Success is not a
correctness judgment. File identities are hashes of invented text bytes;
`changed.txt` changes permissions with identical content. Index comparison also
retains entry types and link targets, so metadata changes cannot disappear.

Tests also read the unchanged #17 `legacy/recovery.json`, `context-seed.json`,
and `evidence.json` fixtures. The old empty evidence fixture uses a Python fake
snapshot with no indexed files; the Rust workshop indexes real simulated files,
including MEMENTO.md, so that empty-index appendix is compared directly against
the evidence builder. Full workshop evidence is tested separately.

`canonical.json` fixes Python-compatible context byte accounting for Unicode,
control characters, negative zero, and scientific-notation boundaries. The
Python test independently reconstructs its expected text. This is context
sizing, not the serializer used to identify archived Python inputs.

`failure-history-v3.json` fixes the controller-generated notice and structured
history for an invented failed `exec` call. `failure-history-v2.json` and
`tool-failure-history-v1.json` retain the earlier notices. Their source numbers
are controller observation sequences, not archive sequences or publication receipts. The
compaction regression separately checks preservation across false repair
summaries, later successful calls and both summary request formats.

`framework-parse-failure-v1.json` retains the exact parser diagnostic from live
Framework run `continuous-8b1bdb91689e`, with source provenance. A CPU controller
regression replays it as failed work and failed summary requests, checks that no
malformed tool text dispatches, and follows usage and source references through
recovery and two later compactions. It covers the original unknown-usage
reservation and simulated measured counts of zero and 405 tokens; those added
counts do not replace the captured request's unknown accounting. It executes no
command text and makes no model call.

The simulator is not a prediction of model behavior or a replacement for real
workshop integration tests. Do not regenerate expected outputs blindly when a
test fails; review the transition or baseline difference first.
