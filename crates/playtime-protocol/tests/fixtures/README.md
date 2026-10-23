# Foundation fixtures

All records here are synthetic. No private run logs, model output, credentials,
real model/workshop locks, or actual worker binaries are included.

- `two-session/events.jsonl` is a contract trace, explicitly labeled `simulation`.
  It carries workspace/memento and separate evidence into a fresh conversation,
  retains an unknown 100-token request alongside two known 5-token requests,
  and ends with a recorded result and cleanup. It is not an executed simulation.
- Files under `two-session/inputs/` and `sessions/` supply the exact bytes named
  by content references. `.fixture` files stand in for executable/source/workspace
  artifacts and cannot be executed or extracted. Their SHA-256 identities are
  real hashes of those synthetic bytes. `inputs/worker/Cargo.lock` is deliberately
  synthetic, independent of the repository's buildable Cargo.lock.
- `legacy/recovery.json`, `legacy/context-seed.json`, and `legacy/evidence.json`
  are selected fields from the Python runner using the existing fake model and
  sandbox fixtures. Timestamps, runtime durations, and incidental paths are
  excluded. Their provenance is the pinned `ad7a2f2` baseline; the Python suite
  checks the expectations against that implementation. They are baseline
  examples for #18/#19, not a general legacy reader or a second controller.
- `usage-unknown.json` and `error-unsupported-version.json` fix initial read-model
  shapes. `schema.json` fixes OpenAPI envelope, reference, and read-model schemas;
  its empty HTTP endpoint collection is preserved as part of the original
  foundation fixture. The current HTTP v1 schema and transport fixtures live in
  [playtime-server's fixture directory](../../../playtime-server/tests/fixtures/README.md).

Validate with the normal Rust and Python suites. When deliberately changing a
contract, review the JSON diff and regenerate the schema with:

```sh
cargo run -q -p playtime-protocol --example schema --locked > crates/playtime-protocol/tests/fixtures/schema.json
```

Do not automatically overwrite behavioral goldens when a test fails. The
committed goldens are the spec; record a deliberate change with a short reviewed
note in [rust-compatibility.md](../../../../docs/rust-compatibility.md). Editing any frozen input requires
updating its reference, the stored manifest digest, and the affected trace
references; the Python fixture-integrity tests detect stale hashes.
