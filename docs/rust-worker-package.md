# Frozen worker packaging and execution

`playtime-worker` composes the runner/archive writer with authenticated private
control. It supports controller simulation, Docker with a scripted model, and
Docker with live Ollama through `playtime-ollama`. It has no public API server,
SQLite ledger, or UI. Native tool text travels only to the fixed in-container
bridge; it never reaches a host shell.

`xtask` builds and verifies the static worker bundle. The
[daemon](rust-daemon.md) owns public admission and the
[lifecycle coordinator](rust-worker-lifecycle.md) owns systemd supervision. This
guide covers packaging and the prepared-writer handoff; automatic reattachment
and archive recovery remain unimplemented. See [development](development.md) for
how package, service, Docker/live, and recovery gates differ.

## Build and validate

Development/CI prerequisites are Linux x86-64, the repository's pinned Rust
toolchain, its musl standard library, `/usr/bin/cc` and its linker (on Ubuntu,
`build-essential`). Rust's target supplies the self-contained musl runtime; a
GNU/dynamic fallback is forbidden. Install the target explicitly:

```sh
rustup target add x86_64-unknown-linux-musl
cargo run --locked -p xtask -- bundle-worker --output /existing/parent/new-bundle
./scripts/test-rust-worker-package.sh
```

The output directory must not exist. The builder captures the workspace
manifests, lockfile, toolchain, first-party crates (including build scripts and
included data), xtask and supported project `.cargo` configuration. It compiles only
the captured files in a private staging directory, with a separate target and
Cargo home, cleared build environment, explicit compiler/linker and locked
release build. Registry downloads are allowed during this explicit operation.
Admission and the worker never invoke Cargo.

Both `.cargo/config` and `.cargo/config.toml` are validated before staging. The
supported configuration is deliberately limited to `build.jobs`, terminal
presentation settings, `net.offline`/`net.retry`, and captured local `paths` or
`patch` entries. Local source paths must be relative, stay within the captured
tree, and name a captured package. They follow Cargo's
[configuration-relative path rules](https://doc.rust-lang.org/cargo/reference/config.html#config-relative-paths),
relative to the parent of `.cargo`. Other configuration, including source
replacement directories/local registries, includes, environment injection,
compiler wrappers and target overrides, is rejected explicitly. These forms
require a reviewed capture/validation policy before packaging supports them.

Before compilation, the builder runs full `cargo metadata --locked` with the
same isolated environment and target selection as the build. Every resolved
local package manifest and target source must canonicalize inside staging,
belong to the snapshot, and match its captured bytes. Registry/git dependencies
must reside within the isolated Cargo cache; a registry source ID alone cannot
authorize reading a host directory. This second check does not rely on the
configuration parser catching every dependency path. Metadata resolution may
fetch dependencies but does not compile them or execute their build scripts.

`source.bundle` is versioned JSON mapping paths to exact UTF-8 or byte contents.
Dirty status compares those captured bytes and paths with the recorded revision,
not a post-build checkout status. This is provenance, not a bit-identical or
offline rebuild guarantee. The final executable bytes are execution identity.
The kernel, model host, and Docker workshop retain separate requirements.

`bundle.json` names the executable, source snapshot, lockfile and build record
using `ContentRef` and `WorkerIdentity`. SHA-256 of the exact descriptor bytes is
the bundle ID. The build record captures source/lock digests, compiler, Cargo and
linker versions, profile, flags, features, revision, dirty status and supported
protocol/schema/capability versions. The record is embedded before compilation;
neither executable nor bundle ID is embedded in itself.

The package validator checks all lengths/hashes and ordinary storage limits,
platform/capability compatibility, and the ELF headers. It requires Linux
x86-64, no `PT_INTERP`, no `DT_NEEDED`, and accepts static PIE. A native development
worker with no embedded production record refuses production startup. Validation
precedes an atomic, no-replace directory publication. Successful builds remove
their uniquely owned staging directory; failed builds retain it for diagnosis.

## Preparation and invocation

`VerifiedBundle::open` holds validated bytes rather than a path to mutable build
output. `prepare` passes that explicit worker to the runner, retaining exact
descriptor/build-record bytes in `InputManifest.files`. `prepare_recoverable`
additionally freezes the supported execution contract only when the validated
bundle advertises `controller_archive_v1`. The worker independently checks it
before controller creation. Ordinary preparation does not opt in. See
[recovery contracts](rust-recovery-contract.md) for the read-only #50 scope and
the remaining #43 transaction work. The demo self-capture helper is never used here.

`VerifiedBundle::from_archive` uses storage-verified input references.
`materialize(parent, run_id)` creates a fresh per-run directory and executable
copy with modes 0700 and 0500. Its intended service parent is
`.playtime/executables`; the caller owns creation of that private parent. All
components use descriptor-relative no-follow operations; files are single-link
regular files. The copy is synced, reread, and checked for execution permission
and a noexec mount. Archived inputs remain non-executable. Retain the disposable
copy until its original invocation is confirmed gone; no drop-time deletion or
shared mutable symlink is provided.

The worker accepts only typed argv:

```text
/absolute/private/run/playtime-worker --bootstrap /absolute/private/bootstrap.json
```

Bootstrap credentials stay in the private file. The existing control library
hashes an open `/proc/self/exe` file, not a reopened pathname. The worker verifies
reservation, kind, run/manifest/executable identities, frozen wall allocation and
embedded build identity before reporting Prepared. Missing proc support,
incompatible builds or corrupt inputs fail startup with no fabricated event.

`playtime_runner::PreparedExecution::acquire` opens and retains the verified
empty writer first, then reads through its actual zero-prefix receipt. It keeps
the frozen scenario, image identity and writer together without creating a
controller/work clock or dispatching an adapter. The value is non-Clone and
nonserializable. It is ownership, not permission to execute.

The private-control `Worker` owns durable grant consumption/revocation and the run lock.
Its Begin callback consumes the runner value into a controller thread using the
same writer. Work time starts at the consumed Begin timestamp, including thread
scheduling delay. Duplicate Begin cannot create a second execution. Cancellation
before Begin retains the empty writer until the invocation ends.

Status refresh has one backward-compatible default hook in worker-control. The
production implementation samples only coalesced in-memory receipts and flags;
it never discovers markers or blocks on disk. Begin waits briefly for committed
initial records; timeout leaves the original thread and grant alive in
Reconciling, and later Status can report its returned receipt. The wire receipt
still requires service verification and persistence before publication through
the public API; see [daemon confirmation](rust-daemon.md#confirmation-authority-and-publication).

The worker retains a pending-publication owner and polls until it settles.
`publication_pending` conservatively remains true while its driver can publish,
including late storage operations. The process remains available for receipt
inspection after terminal publication, retaining its run lock until supervision
ends the original invocation. This does not implement independent hard deadlines
or claim that a returned report proves invocation termination.

## Executable checks

The package gate builds and starts the static worker in an isolated temporary
root with an empty environment. It removes the source bundle before launch,
materializes from archived inputs, then unlinks/replaces the launched pathname
before Begin. It verifies two actual controller sessions, one `run_started`,
returned receipt progression, duplicate Begin, control-client reattachment while
waiting, cancellation/revocation before Begin, embedded build mismatch, copied
archive inspection, object tampering, platform/capability and size rejection.
The compiler/staged source directory has already been removed by the builder.

Native tests cover ELF truncation/overflow, dynamic dependencies/interpreter,
static PIE, directory/file links, no-replace copying, permissions, noexec where
available, empty-prefix ownership and competing writers at every commit through
handoff. All roots are isolated and owned child processes are reaped. Reconnecting
this private client tests the worker seam; it is not an API restart gate.
Real-Cargo packaging regressions also exercise a captured configuration patch,
an external patch whose source changes without changing the snapshot/lockfile,
and a symlink escape. Rejected resolutions never reach a fixture build script.
