# Playtime

This repository is a Rust local-model experiment controller and a disposable
Docker workshop. Work in the existing standalone checkout; do not create linked
Git worktrees unless the user explicitly requests an exception.

- `crates/` and `xtask/`: Rust controller, effects, service, clients, and packaging.
- `recipes/`: reviewed Rust recipe catalog.
- `src/playtime/` and `tests/*.py`: frozen Python controller and tests; keep the
  controller independent of third-party Python packages.
- `workshop/`: image definition, locked packages, fixed container tool bridge.
- `experiments/`: historical Python recipes, study plans, and results.
- `prompts/` and `seeds/`: versioned experimental inputs.
- `models/` and `environments/`: small identity lock files; commit these.
- `.playtime/` and `runs/`: local image archives and run records; never commit.

Rust is the target. Its MVP (#16) is released as v0.1.0; `docs/rust-mvp.md` says
how its parts fit and how to run it. `docs/development.md` maps checks to changes.
The Python controller is frozen: don't modify
`src/playtime/` or `tests/*.py`, and don't run Python experiments. It is no
longer a parity oracle, the legacy compatibility matrix is retired, and legacy
archives and fixtures no longer constrain Rust. The committed Rust goldens are
the spec. The Python suite stays in CI for now; some of its tests read Rust
files, so a deliberate Rust change that would break one adds a new versioned
file instead (see `docs/rust-compatibility.md`).

The Rust foundation is a root Cargo workspace under `crates/`. Keep
`playtime-core` independent of I/O, async runtimes, HTTP, databases, and UI;
`playtime-protocol` owns wire types and serialization/schema dependencies.
`playtime-engine` owns the bounded controller and replaceable external effects.
Its simulator must drive the same controller and never execute command text on
the host. See `docs/rust-controller.md` for dispatch, deadline, and accounting
invariants. Changes to the session-neutral contract also run
`cargo test -p playtime-runner --features test-harness --test workspace_probe --locked`.
`playtime-storage` owns synchronous portable archive I/O. Preserve its distinction
between writer-confirmed live frontiers and offline recovered markers. Never
retry a stopped writer or promote uncommitted bytes. See `docs/rust-storage.md`
for publication, file/link policy, limits, and failure-test contracts.
`playtime-runner` integrates the controller and storage through incremental
scripted or worker-supplied native effects. Preserve committed-intent dispatch
barriers, independent
timer/cancel servicing, late checkpoint receipts, actual log-byte accounting,
and incomplete evidence on uncertain writes. See `docs/rust-archive-driver.md`.
Its demo must explicitly capture the actual executing image through `/proc/self/exe`;
preparation accepts an explicit worker bundle. Latest-receipt observers never
promote a discovered marker or wait on filesystem work.
`playtime-backend-guard` owns private backend-host leases, durable dispatch fencing
and independent stop servicing. Keep its journal outside experiment archives and
retain replay tombstones. Wire observations are not held termination authority.
See `docs/rust-backend-guard.md`; run `./scripts/test-rust-backend-guard.sh` for
guard changes. Its owned-child CPU gate does not establish production host access
control, Ollama ownership or real systemd enforcement.
`playtime-docker` owns bounded native Docker transport and the owned sandbox.
Keep Docker/process dependencies outside core/engine. Preserve precommitted
resource ownership, exact identity checks, absolute timers, uncertain-exec fencing
and aggregate cleanup bounds. See `docs/rust-docker.md`; explicitly run
`./scripts/test-rust-docker-e2e.sh --case sandbox` for adapter changes when the
locked local environment is available. The
sandbox gate is not checkpoint publication or production run-and-view evidence.
Stopped-workspace capture owns bounded unextracted candidates, not writer receipts.
See `docs/rust-workspace-capture.md`; run the additional Docker `--case checkpoint`
gate for capture changes. Keep embedded helper digests tied to the frozen worker
build and preserve pending-exporter fencing through final cleanup.
`playtime-worker` composes the real runner and private worker control; `xtask`
builds verified static bundles from a staged source snapshot. Preserve the same
owned prepared writer through Begin, and never invoke Cargo during admission or
worker execution. See `docs/rust-worker-package.md`; run
`./scripts/test-rust-worker-package.sh` for worker/package changes. This CPU gate
does not replace real systemd, Docker/live run-and-view, or restart/recovery gates.
`playtime-server` owns HTTP/auth in modules separate from lifecycle coordination;
`playtime-client` owns native transport and acknowledged event reconnect.
`playtime-local` shares credential/discovery file policy. Keep HTTP fixture
provenance explicit, and do not treat serializable API frontiers as writer
receipts. See `docs/rust-http-api.md` for the application boundary and conformance
tests. HTTP fixture tests use temporary roots and port zero, and must not launch
experimental workers. `playtimed` supplies normal production simulation through
the same handlers; see `docs/rust-daemon.md` and run
`./scripts/test-rust-service-e2e.sh --case normal` for daemon integration changes.
Its CPU test supervisor is explicitly separate from real systemd enforcement;
restart/recovery and the full CLI/UI simulation gates remain incomplete.
`playtimed --docker-scripted` runs the frozen `docker-two-session` recipe in a
packaged systemd worker with real Docker. See `docs/rust-docker-run-and-view.md`;
run `./scripts/test-rust-service-e2e.sh --case run-and-view --backend docker-scripted`
for native-effect, Docker-profile or run-and-view changes when user systemd and the
locked image are available. `--ollama-endpoint` adds the live model, called
directly; see `docs/rust-live-model.md`. Run `--backend live` for live-path changes,
one live-model test at a time, after checking that no Playtime worker is active.
Long live runs come from the reviewed `recipes/*.toml` catalog (`--recipes`). The
runner's bounds check them, and each archive freezes its recipe and seeds; see
`docs/rust-recipes.md`. Recipe limits change only with the owner's review.
`playtime-cli` owns clap parsing/presentation. Shared connection resolution,
durable launch receipts, cursor files and verified downloads belong in
`playtime-client`; keep these independent of clap/egui. Local I/O stays outside
UI render loops. `playtime-test-support` owns native-client subprocess fixtures,
isolated roots, readiness deadlines and owned-process cleanup. See
`docs/rust-cli.md`; run `./scripts/test-rust-cli-e2e.sh --backend fixture` for CLI
changes. The dedicated CLI simulation gate still fails pending full restart/recovery
integration; the service-normal gate is separate. Never substitute fixtures
or claim they establish the missing recovery behavior.
`playtime-ui` owns egui presentation and a separate view model. Keep network,
private files, JSON parsing, hashing, image decoding and memory diffs outside
render callbacks. Use shared client receipts/downloads; closing or detaching the
UI never cancels a run. Preserve generation/run identity checks and bounded
history/image work. See `docs/rust-ui.md`; run the UI fixture wrapper for UI
changes. Review screenshot differences using the pinned software Vulkan renderer;
never silently skip or regenerate baselines. The dedicated UI simulation gate
still fails pending full restart/recovery
integration; service-normal and Docker/live gates exercise the implemented paths.
Do not treat fixture tests as recovery evidence.
Commit `Cargo.lock`; ignore
`target/`. Validate Rust changes with `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`, and
`cargo test --workspace --locked`, plus the Python suite below. See
`docs/rust-foundation.md` for the scope and versioning rules. Keep runtime,
service, database, and GUI dependencies in their owning crates. When Rust
behavior changes on purpose, update the affected behavioral or schema goldens and
add a short reviewed note to `docs/rust-compatibility.md`; do not automatically
regenerate them just to make a test pass.

Run the frozen Python suite with `python3 -m unittest discover -s tests -v`.

Preserve these properties:

- Model-controlled commands execute only inside the workshop as uid 1000.
- Logs, input archives, controller code, and Docker access stay outside it.
- `memento_turns_v1` starts a new conversation each session; only workspace files
  persist. Other internal policies must use explicit request purposes/scopes and
  versioned archive records, never fabricated memento sessions. The run-scoped
  probe is test-only and establishes no native process-continuity behavior.
- Limits and cleanup are enforced by the controller and container lifetime.
- Keep partial inference visible; never report unknown token usage as zero.
- Archived model text and filenames must be escaped in generated HTML.
- Reruns verify identities and archived inputs; do not silently update locks.

Changing workshop sources requires a deliberate `prepare --refresh` and a new
locked image. Preserve existing run records and image archives.
