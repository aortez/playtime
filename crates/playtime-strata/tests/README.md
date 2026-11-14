# Strata fixture provenance

`support/mod.rs` generates synthetic Responses events and metadata; `http.rs`
serves them on an ephemeral loopback TCP port. No live output, credentials,
downloaded model or third-party implementation is embedded in these fixtures.

Shapes and event order were checked against the installed
`serve/responses.py` on 2026-11-13:

- Framework terminal/engine 0.1.58, HIP Halo.
- Bird terminal/engine 0.1.57, CUDA 13.
- Both Responses sources: SHA-256
  `aba98b4aba933fe55cf699985add2e1014a66dacb55ed768ca9aa404c3fcb29b`.

Metadata comes from the installed server's `/props`, `/settings`, `/v1/status`
handlers. Fixtures intentionally change identities, sequences, usage, arguments
and transport behavior to check rejection/uncertainty paths. They do not establish
live CUDA/HIP behavior, engine accuracy, remote cancellation or ownership.
See [the transport guide](../../../docs/rust-strata.md) for the separate live gate.
