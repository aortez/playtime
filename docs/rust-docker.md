# Native Docker sandbox

`playtime-docker` owns bounded native subprocess transport and the lifetime of an
owned workshop. [Stopped-workspace capture](rust-workspace-capture.md) produces
snapshot candidates; the runner publishes checkpoints. The
[packaged worker](rust-docker-run-and-view.md) composes those pieces with committed
intent barriers and the daemon. [Live-model execution](rust-live-model.md) uses
the same sandbox with Ollama. This guide owns sandbox contracts, not admission
or archive recovery authority.

## Supported selection and readiness

The supported profile requires a local rootful Linux amd64 Docker Engine, API 1.41+
and cgroup v2. CPU quota, memory, swap and PID enforcement, seccomp and the local
volume driver must be available. Rootless engines, user-namespace remapping,
remote/context-selected engines, Docker Desktop and other platforms fail closed.
The operator supplies absolute executable and Unix socket paths. `Cli::resolve`
canonicalizes them, requires a root-owned socket and a non-group/world-writable
executable, records the CLI digest, and creates a new empty 0700 configuration
directory beneath a private owned root. Perform this preparation outside the
controller loop. Replacement of the selected executable or configuration fails
subsequent transport rather than silently selecting another program/context.

Every CLI invocation clears its environment and explicitly selects `--host` and
`--config`. Docker host/context/TLS/API-version overrides and credential helpers
from the user's configuration are not inherited. Bounded client/server/API,
daemon ID, kernel and host identity are recorded. Every lifecycle query/mutation
rechecks the original daemon. Unavailability and identity change are uncertainty,
never an empty resource inventory.

Readiness checks only version/info and the exact locked image ID/platform. It
requires the reviewed bridge hash in the schema-1 lock and rejects image-declared
volumes/build hooks. The immutable image ID binds the reviewed interpreter/bridge
profile. After the owned holder starts, before tools, a fixed isolated probe also
checks the actual interpreter path, bridge SHA-256 and uid/gid. Preview never
creates a container, runs a probe, pulls/builds/loads an image or refreshes a lock.
A missing image is a distinct failure. Rust does not automatically restore image
archives; explicit image restoration is not implemented in the Rust client.

## Ownership and lifecycle

`Ownership` binds service/run/grant/worker invocation, a 128-bit random nonce and
the discovered daemon. These are trusted caller inputs, never model arguments.
A descriptor is not lifecycle authorization. Before every resource create,
`OwnershipJournal::commit` must durably publish the exact `Proposal`, including
derived name, labels, image, limits and original absolute timer deadline. The
worker supplies a durable private journal; the sandbox gates use explicitly
synthetic descriptors and journals. The adapter itself grants no admission
authority.

The bounded name hashes the full owner. Labels use `io.playtime.*`, include role
and session ordinal, and bind the exact proposal. A preexisting proposed name is
a conflict, even when Docker would reuse that volume. After create, containers
use returned IDs only after verification. A lost acknowledgement retains the
proposed name for exact reconciliation; create/start/tool are never replayed.
An unacknowledged create remains pending until its exact owned resource is
observed. An early name lookup reporting absence is not a completion barrier:
the daemon can publish the create afterward, even if the local CLI was reaped.
Repeated absence checks cannot clear this uncertainty. Cleanup reports a residual,
quiescence stays fenced, and final cleanup retains the volume while any container
create remains pending. A valid create acknowledgement or verified observation of
the exact resource allows removal followed by an absence check; explicit transport
non-dispatch evidence permits `NotDispatched`. Dropped futures preserve pending
state before dispatch, with no new deadline or automatic create retry.
Volumes require exact name/labels/local-driver/options. Cleanup checks identities
again and uses only these resources; it never prunes, deletes images, scans broad
prefixes for deletion or touches an unrelated same-name replacement.

There is one capacity/inode-bounded local-driver tmpfs volume, a trusted holder,
and a disposable agent per session. `volume-nocopy` prevents image-side mount
initialization. Effective configuration is inspected before start and before
model tools: exact image/labels/mounts, explicit entrypoint/argv, no healthcheck or
restart, no network, read-only root, dropped capabilities, no-new-privileges,
disabled Docker logs, private namespaces and bounded CPU/memory/swap/PIDs/tmp/shm.
No host directories or Docker socket are mounted. Holder and agent roots are
read-only; uid/gid 1000 tools can write the bounded workspace and scratch tmpfs.

The root lifetime helper is a fixed literal executed through the locked
`/usr/local/bin/python3.12 -I -S -B`, with workdir `/`. It compares Linux's shared
absolute `CLOCK_MONOTONIC` deadline and kills PID 1 at expiry. `WorkDeadline::at`
accepts the caller's original absolute deadline (the gate's `after` convenience
constructor is used only once). The root timer is distinct from uid-1000 tools.
This preserves the Python root-timer topology while
avoiding a relative timeout restarted by delayed create/start/retry. The holder
uses the original work deadline plus 180 seconds. All helper imports exclude cwd,
PYTHONPATH, user-site and sitecustomize; bytecode writes are disabled. The gate
poisons every bridge import name and user-site hooks inside the locked image.

Methods require exclusive access to the sandbox. An interrupted/dropped mutation
future leaves a proposed or fenced resource. Each session requires confirmed
previous-agent quiescence. `quiesce` force-removes only the exact owned agent,
checks absence on the original daemon, validates the running holder/volume and
requires its holder to be the sole remaining volume mount. It returns a
non-serializable `Quiescent<'_>` exclusive borrow. A new session and holder cleanup
are impossible while that borrow is held. Capture keeps it throughout export,
including exporter failure fencing; it is not a snapshot or writer receipt.

## Transport, tools and uncertainty

`Transport` accepts typed `Operation` values, an absolute deadline and a
`CancellationToken`. The real implementation uses argv directly, never a host
shell. `ScriptedTransport` is an exact-request FIFO fake; lifecycle tests also use
a stateful synthetic daemon. No CPU test accesses Docker or executes model text.

Input encoding rejects JSON over 262,144 bytes before launching any Docker
process. Bridge stdout is capped at 524,288 bytes, query stdout at 65,536 and
stderr at 16,384. Concurrent partial stdin writes and stdout/stderr reads prevent
full-pipe deadlocks. Overflow, cancellation or deadline terminates the owned CLI
process group and retains an asynchronous reaper. Dropping the caller signals
that task; a four-slot bound remains held until its real completion. The leader
is not reaped while descendants hold pipes, preventing process-group reuse.

`Dispatch`, `LocalExit`, CLI acknowledgement, confirmed absence and residual
uncertainty are separate observations. Killing/reaping `docker` proves nothing
about a daemon-side container or exec. Operation diagnostics retain bounded
operation/resource identity, acknowledgement identity, allotted/elapsed time,
local exit/fault and byte counts, without model payloads or raw unrestricted
stderr. Join them to the precommitted run/session proposal, not a broad name scan.

Tools use only `docker exec --user 1000:1000 --workdir / --interactive ID
/usr/local/bin/python3.12 -I -S -B /opt/playtime/tool.py`. `exec`, `read_file` and
`write_file` travel as encoded JSON stdin. A model command never appears in host
argv. Image-file reads remain allowed and bridge writes stay inside `/work`.
`end_session` remains a controller memento write, not a fourth bridge operation.
UTF-8 and response shape are validated; zero/false, output truncation, binary-read
errors and an ordinary `timed_out: true` result are preserved.

Transport timeout/disconnect/cancellation or malformed output fences the agent.
The caller must invoke `quiesce` or final `cleanup`; another tool, new session or
snapshot cannot proceed on that uncertain agent. Quiescence must be proven even
if a local child exited successfully. Do not map this error to a successful tool
timeout, user cancellation or non-dispatch proof in the runner.

Start/initialize use at most twenty seconds within the frozen work deadline.
Quiescence uses at most twenty seconds of the caller's total stop/capture bound
(capped at eighty seconds). The first quiescence/capture deadlines are retained
for that session, and the first final-cleanup deadline is retained for the
sandbox; a retry cannot renew them. Final cleanup has one aggregate twenty-second budget
for agent, holder and volume in that order, not a fresh timeout per resource.
Per-resource residuals survive independent successes. A lost remove reply can be
resolved by a subsequent successful absence check; an unavailable/changed daemon
cannot supply that proof. The holder is not removed during quiescence. Final
cleanup is an explicit operation after the checkpoint borrow ends.

## Gates and evidence

```sh
cargo test --locked -p playtime-docker
./scripts/test-rust-docker-e2e.sh --case sandbox
```

The real wrapper uses only the already-present `environments/general.lock.json`
image and synthetic tool calls. It defaults to `/usr/bin/docker` and
`/var/run/docker.sock`; explicit `PLAYTIME_DOCKER_EXECUTABLE` and
`PLAYTIME_DOCKER_SOCKET` select another supported local installation. It never
loads an image or calls a model. An invoked gate fails when unsupported/unavailable.
Each invocation creates a private temporary root and random resource namespace,
commits proposed ownership, and retains bounded diagnostics plus cleanup results.
The wrapper has a 240-second outer bound; a hard harness kill retains proposals
for inspection and does not pretend it proved cleanup. Internal absolute timers
still bound containers, and no broad automatic deletion is attempted.

The gate verifies uid/gid, network/rootfs/socket isolation, actual cgroup settings,
output/workspace/inode/tmp/PID limits, poisoned imports, binary/image-file reads,
valid command timeout, persistence, fresh `/tmp`, old background-writer cessation,
cancellation during a running command, independent lifetime expiry, retained
holder and exact absence after cleanup. It does not capture/publish a checkpoint,
inspect a daemon run in the UI, or establish production run-and-view. The
checkpoint and service run-and-view gates test those separate pieces; see
[development](development.md). Gates preserve workshop sources, identity locks,
and existing run archives.

The container inspection fixture is a normalized capture from this gate's own
synthetic Docker Engine 28.4.0 holder, with host/run identifiers and nonessential
environment fields replaced. It is not an experiment record. Fault fixtures
exercise lost acknowledgements (including daemon publication delayed until after
cleanup/absence queries), dropped create futures, non-dispatch proofs, full pipes,
malformed responses, same-name and
effective-config conflicts, changed/unavailable daemons and partial cleanup.

Docker references: [create](https://docs.docker.com/reference/cli/docker/container/create/),
[exec](https://docs.docker.com/reference/cli/docker/container/exec/),
[force removal](https://docs.docker.com/reference/cli/docker/container/rm/), and
[volume creation and existing-name reuse](https://docs.docker.com/reference/cli/docker/volume/create/).
