# Playtime

This repository is a local-model experiment controller and a disposable Docker
workshop. Keep the controller independent of third-party Python packages.

- `src/playtime/`: host controller, Ollama adapter, snapshotting, reports, CLI.
- `workshop/`: image definition, locked packages, fixed container tool bridge.
- `experiments/`: complete TOML recipes. Paths are relative to the project root.
- `prompts/` and `seeds/`: versioned experimental inputs.
- `models/` and `environments/`: small identity lock files; commit these.
- `.playtime/` and `runs/`: local image archives and run records; never commit.

The Rust foundation is a root Cargo workspace under `crates/`. Keep
`playtime-core` independent of I/O, async runtimes, HTTP, databases, and UI;
`playtime-protocol` owns wire types and serialization/schema dependencies.
`playtime-engine` owns the bounded controller and replaceable external effects.
Its simulator must drive the same controller and never execute command text on
the host. See `docs/rust-controller.md` for dispatch, deadline, and accounting
invariants. Keep the Python parity fixtures and independent oracle tests intact.
`playtime-storage` owns synchronous portable archive I/O. Preserve its distinction
between writer-confirmed live frontiers and offline recovered markers. Never
retry a stopped writer or promote uncommitted bytes. See `docs/rust-storage.md`
for publication, file/link policy, limits, and failure-test contracts.
Retain the Python launcher during migration.
`playtime-server` owns HTTP/auth in modules separate from future lifecycle work;
`playtime-client` owns native transport and acknowledged event reconnect.
`playtime-local` shares credential/discovery file policy. Keep HTTP fixture
provenance explicit, and do not treat serializable API frontiers as writer
receipts. See `docs/rust-http-api.md` for the application boundary and conformance
tests. HTTP fixture tests use temporary roots and port zero, and must not launch
experimental workers.
Commit `Cargo.lock`; ignore
`target/`. Validate Rust changes with `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`, and
`cargo test --workspace --locked`, plus the Python suite below. See
`docs/rust-foundation.md` and `docs/rust-compatibility.md` for the scope, pinned
baseline, versioning rules, and synthetic fixture provenance. Keep runtime,
service, database, and GUI dependencies in their later owning crates. Changing
behavioral or schema goldens requires reviewing the contract difference; do not
automatically regenerate them just to make a test pass.

Use `python3 -m unittest discover -s tests -v` for controller tests. Use
`PLAYTIME_DOCKER_TESTS=1 python3 -m unittest discover -s tests -v` when changing
the Docker integration or workspace limits. `./playtime smoke
experiments/pilot.toml` exercises the real runner without a model; `--live`
adds a small real-model check.

Preserve these properties:

- Model-controlled commands execute only inside the workshop as uid 1000.
- Logs, input archives, controller code, and Docker access stay outside it.
- Each session starts a new conversation. Only workspace files persist.
- Limits and cleanup are enforced by the controller and container lifetime.
- Keep partial inference visible; never report unknown token usage as zero.
- Archived model text and filenames must be escaped in generated HTML.
- Reruns verify identities and archived inputs; do not silently update locks.

Changing workshop sources requires a deliberate `prepare --refresh` and a new
locked image. Preserve existing run records and image archives.
