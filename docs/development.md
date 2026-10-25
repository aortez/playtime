# Development and test gates

Use the existing standalone checkout. Rust is the active implementation; the
Python controller under `src/playtime/` and `tests/*.py` are frozen. Do not run
Python experiments. See [AGENTS.md](../AGENTS.md) for working constraints and
[the architecture guide](rust-mvp.md) for component ownership and operation.

## Build

Cargo selects Rust 1.94.1 from `rust-toolchain.toml`. Commit `Cargo.lock`; build
with `--locked`. The workspace forbids unsafe Rust. Core values stay independent
of I/O and runtimes; serialization belongs in protocol, external effects in their
owning crates, and presentation in clients.

```sh
cargo build --locked -p playtime-server --bin playtimed -p playtime-cli --bin playtime \
  -p playtime-ui --bin playtime-ui
```

A production worker is a verified static Linux x86-64 musl bundle. Building it
requires the musl target and `/usr/bin/cc` with a linker (`build-essential` on
Ubuntu). Choose an output directory that does not exist:

```sh
rustup target add x86_64-unknown-linux-musl --toolchain 1.94.1
cargo run --locked -p xtask -- bundle-worker --output /absolute/new-bundle
```

The builder compiles a captured source snapshot. Admission and worker execution
never invoke Cargo. See [worker packaging](rust-worker-package.md) for supported
Cargo configuration, provenance, and validation.

## Required checks for Rust changes

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 -m unittest discover -s tests -v
```

The Python suite stays in CI and still reads some shared Rust fixtures. A deliberate
Rust change that would break those frozen tests adds a new versioned file; see
[compatibility and goldens](rust-compatibility.md). Review intentional behavior or
schema changes, update the affected Rust goldens, and add a short compatibility
note. Never regenerate fixtures or screenshot baselines just to make tests pass.

For documentation-only edits, check the diff, relative links and anchors, and any
commands or behavior claims against the implementation. Runtime gates are needed
when behavior changes, not merely because a guide mentions them.

## Choose the additional gate by the changed boundary

These gates test different properties. Passing a fixture gate does not establish
real worker, Docker, model-host, or recovery behavior.

| Change | Command | What it exercises |
| --- | --- | --- |
| CLI/shared client | `./scripts/test-rust-cli-e2e.sh --backend fixture` | Actual CLI subprocesses over scripted authenticated HTTP; receipts, reconnect, output and downloads |
| Workbench | `./scripts/test-rust-ui-e2e.sh --backend fixture` | Workbench backend and launch recovery over scripted HTTP; also review pinned screenshots for visual changes |
| Worker/package | `./scripts/test-rust-worker-package.sh` | Frozen static bundle, detached controller, prepared-writer handoff and package verification on CPU |
| Daemon integration | `./scripts/test-rust-service-e2e.sh --case normal` | Real daemon, packaged controller, archives and clients with scripted effects and an explicit CPU test supervisor |
| Backend guard | `./scripts/test-rust-backend-guard.sh` | Private ownership/journal, dispatch fencing and independent stop/crash behavior with owned child processes |
| Docker adapter | `./scripts/test-rust-docker-e2e.sh --case sandbox` | Real locked Docker sandbox, isolation, deadlines and exact owned cleanup |
| Workspace capture | `./scripts/test-rust-docker-e2e.sh --case checkpoint` | Real stopped-workspace capture; additionally run the sandbox gate |
| Native effects/Docker profile/run-and-view | `./scripts/test-rust-service-e2e.sh --case run-and-view --backend docker-scripted` | Real systemd worker and Docker, scripted model, archive and client review |
| Live model path | `./scripts/test-rust-service-e2e.sh --case run-and-view --backend live` | The live Ollama path, streaming, persistence, review and cleanup |
| Registered-model execution | `./scripts/test-rust-registered-model.sh` | Additional real Qwen API/workbench smoke, exact selection and replay; does not replace the original live gate |

The Docker gates require the already-present locked image and supported local
Docker environment. Run-and-view gates additionally require user systemd and the
pinned software Vulkan renderer. They fail when prerequisites are missing; do
not substitute fixtures. See [Docker](rust-docker.md),
[capture](rust-workspace-capture.md), and [run-and-view](rust-docker-run-and-view.md).

Run one live-model test at a time. First check that no worker is active:

```sh
systemctl --user list-units 'playtime-*' --state=active
```

The owner stays off the model host during a live run. Identity inspection is
read-only; do not change the host or refresh identities. Keep failed attempts as
records. `PLAYTIME_LIVE_ENDPOINT` selects the gate's endpoint. See
[live-model evidence and gate requirements](rust-live-model.md#evidence).

The frozen Python suite also has opt-in Docker checks for its shared helpers and
fixtures. With the locked Docker environment available:

```sh
PLAYTIME_DOCKER_TESTS=1 python3 -m unittest discover -s tests -v
```

These checks do not run Python experiments or make Python a Rust parity oracle.

The guard's process suites run only in its dedicated gate, outside default CI;
its CPU evidence does not establish production model-host access control.
Real systemd lifecycle tests are separately opt-in; their commands and ownership
bounds are in [worker lifecycle](rust-worker-lifecycle.md#real-systemd-supervision).

## Gates that are not implemented

The dedicated CLI/UI `--backend simulation` wrappers intentionally fail. The
service `--case normal` gate works, but the broader client simulation acceptance
still requires restart with a surviving worker, worker-loss recovery, and the
associated failure matrix. There is no fixture fallback. Service `--case lifecycle`
and `--case recovery` are not supported by the wrapper.

Do not interpret this as an absence of Rust simulation or live execution. Those
paths are covered by the implemented gates above. Automatic daemon reattachment
and archive recovery remain unimplemented; the read-only
[recovery planner](rust-recovery-contract.md) grants no mutation authority.

## Workbench development

For a private headless display, scripted daemon and workbench:

```sh
./scripts/workbench-sandbox.sh start
./scripts/workbench-sandbox.sh status
./scripts/workbench-sandbox.sh stop
```

`scripts/workbench-drive.py` drives controls and captures screenshots. The
[workbench playbook](../.claude/skills/run-workbench/SKILL.md) explains its use,
including attachment to an existing daemon. The sandbox keeps display, D-Bus and
accessibility settings separate from the desktop and retains its directory for
inspection.

The [UI guide](rust-ui.md#reviewed-screenshots) owns the exact pinned renderer,
screenshot commands and review procedure. Its [performance section](rust-ui.md#performance-measurement)
explains the bounded headless measurement and links the recorded result.

## CI

[Foundation checks](../.github/workflows/foundation.yml) defines four jobs:

- `foundation`: formatting, locked Clippy/workspace tests, CLI/UI fixture wrappers,
  and the frozen Python suite.
- `worker-package`: the static worker package gate.
- `service-normal`: the real daemon/controller gate with CPU test supervision.
- `ui-snapshots`: reviewed screenshots using the pinned CPU Vulkan renderer.

Docker, live-model, backend-guard, and opt-in real-systemd lifecycle gates run
separately on suitable hosts. Historical test counts in dated evidence describe
those runs, not today's suite size or current test results.
