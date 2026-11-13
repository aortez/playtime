# Project chat console

`playtime-chat` is a standalone terminal client over `playtime-harness::Session`.
It supplies an asynchronous effect driver, Strata model calls and a separate
Docker adapter for editing a selected local project. The controller, tool
admission, token accounting, context planning and compaction policy are shared
with Playtime experiments. It requires no Playtime daemon or workbench.

This is the initial implementation of
[#210](https://github.com/aortez/playtime/issues/210), with the project execution
portion of [#208](https://github.com/aortez/playtime/issues/208). It is an opt-in
developer client. The [launcher guide](rust-chat-launcher.md) describes the first
Framework fast preset for [codex-launcher #14](https://github.com/aortez/codex-launcher/issues/14).
Use its separate pilot entry alongside working Codex profiles.

## Running

First acquire an idle inference host through its existing exclusive session
helper, keep its heartbeat alive, and verify actual artifacts/configuration
against the selected [Strata model lock](rust-strata.md#identity-and-settings).
Use `framework-gyro-m-fast` or `bird-gyro-m-fast`. The console connects to the
owner's loopback endpoint; it does not start SSH, stop another session, change
power limits, hash remote weights or acquire inference ownership itself. HTTP
busy checks do not replace that ownership.

```sh
cargo build -p playtime-chat --locked
target/debug/playtime-chat \
  --endpoint http://127.0.0.1:18080/v1 \
  --model-lock models/framework-strata-gyro-m-fast-v1.lock.json \
  --workspace /absolute/project
```

The default state root is `~/.local/state/playtime-chat`, outside the selected
project. `--state-dir` changes it; use one consistent state root for all consoles
so their project leases contend. A new private session directory is printed on
startup. To reopen a completed session, pass `--session /absolute/session` and
the same model, workspace and allocation settings. A changed config is rejected.
The executable, image/model locks, session and private state must be outside the
project. When editing Playtime itself, install the binary and copy its reviewed
locks outside the source checkout first, passing both `--model-lock` and
`--image-lock` explicitly.

The basic interface accepts one message per line. `/paste` starts multiline input;
`/end` on its own line submits it. Text, reasoning, tool activity, input bounds,
compaction requests and final accounting stream to the terminal. Model-supplied
terminal control characters are escaped. `/quit` exits; `/stop` or Ctrl-C during
a turn cancels it and waits for owned cleanup. SIGTERM/SIGHUP also request cleanup.
Normal EOF during a running turn requests cancellation.

For one turn without a prompt loop, use `--prompt 'task'`. `--json` adds newline
JSON event envelopes with `schema_version: 1`; diagnostics remain on stderr.
Those events are observations, not durable archive receipts. The private session
files are the persistent conversation. A disconnected or overloaded event consumer
cancels work; the driver does not continue editing invisibly.

Defaults are the locked server context, 900 seconds per turn, 300 seconds per
model call, 90 seconds per command, 8,192 output tokens per call, 32,768 generated
tokens per turn, 4,096 summary tokens and 16 KiB per tool result. Supported
overrides are listed by `--help`. `--context-tokens` can select a smaller harness
allocation; it cannot exceed the locked server context. It does not reconfigure
the server or claim a benchmark at its full context size.

## Project tools

`playtime-docker::project::ProjectDirectory` resolves the selected project and
records its device/inode/owner identity. It holds a cooperative exclusive lease
under private state, keyed by device and inode. Admission checks at most 500,000
entries for ten seconds, refuses nested mounts, special files and external hard links, and never
follows project symlinks. Internal hard links are allowed when all links are
within the selected tree. Both files and symlinks are counted. Same-device bind
mounts are checked using [`/proc/self/mountinfo`](https://man7.org/linux/man-pages/man5/proc_pid_mountinfo.5.html), so a mount cannot hide files from
admission that would be exposed by Docker's nonrecursive bind. Host Unix sockets
cannot enter the initial workspace.
The project must be owned by the caller and not world-writable. The native uid-1000
probe must confirm that the mounted project is writable.

One container is created per turn. The only host bind mount is the selected
directory at `/work`, writable, nonrecursive and with private propagation.
Symlinks resolve in the container namespace. Model commands run through the
locked bridge as uid 1000. The container has no external network, a read-only root,
no extra capabilities, no Docker socket and no mounts for host credentials or
session records. It uses two CPU cores, 4 GiB memory without extra swap, 256 PIDs
and 256 MiB `/tmp`. The immutable PID-1 timer also bounds lifetime if the client
disappears. These are enforced container limits; **project files have no aggregate
disk quota**. The experiment sandbox still uses its original bounded tmpfs volume.

The current locked image provides Bash, Git, Python 3.12, Node/npm, GCC/G++, Make,
rg and SQLite. Rust/Cargo and Go are not installed, and external dependency
downloads are unavailable. The model is told these limits. A separate reviewed
image/toolchain update is needed for builds requiring those tools.

Admission and the live native probe verify the same directory identity; each tool
checks the selected host path and exact owned container again. Other host programs
are not fenced by this cooperative lease. Do not replace the project directory,
introduce outside hard links or change mounts concurrently. This is isolation
from model commands, not protection from a hostile host process with your UID or
root privileges.

Before any Docker create, construction durably reserves the project and the
`ProjectJournal` commits its exact proposal. The proposal binds root identity,
container labels, image, process limits and original lifetime. Create/start/tool
are one-shot operations; lost or cancelled operations fence further tools.
The shared lifecycle code removes only the exact verified owned container.
An unresolved create cannot be cleared by an early absence query because Docker
could publish it later. Cleanup retains that uncertainty and its private marker.
Only confirmed cleanup clears the reservation; it never removes project files.

## Persistence and interruption

`runtime::run_turn` records dispatch intent and rechecks the controller effect
before invoking an adapter. Model/tool tasks run independently of blocking record
fsync, with their own deadlines and cancellation. The project timer is independent
of the process. A failed record publication stops new work and still invokes owned
cleanup. No alternate compaction or tool-selection loop exists in the console.

Successful turns publish the existing private conversation checkpoint. A new
console process can reopen it and send the full retained conversation to Strata.
Tools are never replayed from a saved transcript. Model requests with uncertain
remote state fence further inference in that client; the external host owner must
finish its server cleanup. Socket closure alone is not remote termination proof.

**Interrupted or failed turns do not support automatic continuation.** Their
partial text, usage uncertainty, inputs and records remain saved, and the session
head stays active. A later open refuses it. Similarly, a crashed process can leave
a private project `active` proposal; subsequent project admission refuses to edit
until its exact container ownership has been reconciled. There is no automatic
general recovery command. Do not delete these markers simply to bypass the checks.

One narrow failure has explicit recovery: a turn's first model request was a
tool-free compaction, it completed with `length` at its summary cap, all usage is
known, and owned cleanup completed. `--recover-compaction /absolute/old-session`
validates its contiguous saved events and creates a **new** session from the last
completed checkpoint. Only an increased `--summary-tokens` value may differ from
the original configuration. The source stays fenced and unchanged; its rejected
summary is never accepted as conversation memory. The console prints the pending
user message so it can be submitted again, or `--prompt` supplies an explicit
message to run. Other failures, work/tool requests, missing records, unknown usage
and cleanup failures remain refused. Recovery does not reconcile a live server;
the launcher must still acquire its normal exclusive host ownership.

The old default was 2,048 summary tokens. Existing completed sessions created with
that cap still require `--summary-tokens 2048` when reopening. New sessions use
4,096; the higher cap gives a detailed summary room to finish, but is not a
guarantee against truncation. `finish_reason=length` always remains a failed summary.

The [November 13 recovery evidence](../experiments/chat-console/2026-11-13-compaction-recovery.json)
captures the motivating live failure: the first summary stopped at 2,048 tokens.
An explicit fork with a 4,096-token cap completed its summary normally at 2,153
tokens, answered the pending message, and saved a resumable checkpoint. All
14,844 original session files matched their pre-recovery checksums; the host
helper restored Framework. Summary fidelity is a separate quality assessment.
Graceful cancellation normally confirms container cleanup and releases the
project, so a new conversation can be started after the inference owner finishes.

## Verification

```sh
cargo test -p playtime-chat --locked
cargo test -p playtime-docker --locked
./scripts/test-rust-docker-e2e.sh --case project
./scripts/test-rust-docker-e2e.sh --case sandbox
```

The CPU driver fixtures use the real controller and private persistence with
explicitly synthetic effects. They cover tool/follow-up turns, reopen, cancellation
with unknown usage, a disconnected frontend, failed record publication and
uncertain cleanup. The project fixtures cover external hard links, sockets,
protected paths, journal failure, crash fencing, late creation, changed mounts,
host path replacement and uncertain tool completion.

The native project gate mounts only its own temporary project. It exercises real
exec/read/write, uid/network/root boundaries, cancellation without replay, exact
cleanup and persistent edits. It does not run a model or claim quality results.
Live console evidence is recorded separately; a short task or a deliberately
smaller context allocation is not a populated 128K/160K performance benchmark.

### Framework fast-profile smoke, November 13

The [recorded report](../experiments/chat-console/2026-11-13-framework-fast.json)
binds the tested fast-profile lock and private raw evidence. The owner verified
remote artifacts/configuration against that lock before inference.

- The console repaired a small Python function using real project tools and ran
  all three tests successfully in 14.477 seconds, across five model calls.
- A new process reopened the saved session and recalled its conversation-only
  codename without tools, in 2.654 seconds.
- Five saved turns under a 32,768-token harness allocation triggered compaction.
  The requested fact appeared only in the derived summary before the final work
  request, and the model recalled it correctly. The server retained its 163,840
  context setting. This establishes a live compaction path, not full-window
  performance or broad summary-quality accuracy.
- SIGINT after an output delta stopped the turn and confirmed workspace cleanup
  in about 0.268 seconds. One request remained charged as unknown; the session
  stayed fenced while its workspace reservation was released.

The helper then stopped the owned server and restored Framework successfully.
No owned local project containers remained. Bird's CUDA transport has prior fast
smoke evidence, but these project-console cases have only run on Framework.
