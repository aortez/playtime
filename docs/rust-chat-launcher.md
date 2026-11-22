# Launching project chat from codex-launcher

The launcher runs the standalone [project chat console](rust-chat-console.md)
with the host's reviewed Gyro-M DFlash2 preset:

| Entry | Inference host | Server context |
| --- | --- | --- |
| `playtime-framework-fast` | Framework, HIP | 163,840 tokens |
| `playtime-bird-iq4` | Bird PRO 5000, Basalt Flash-Next IQ4 | 163,840 tokens |
| `playtime-bird-nvfp4` | Bird PRO 5000, Basalt Flash-Next NVFP4-DQ8 | 163,840 tokens |

All use medium work thinking and no summary thinking. Codex does not participate
in the agent loop. Project tools run on the workstation that launches the console.

For ordinary host development, select the [host coding preset](rust-host-coding.md):
`codex-launcher playtime-bird-nvfp4 --preset coding-host`. It uses the same chat
and compaction harness with Codex-derived instructions, your real workspace path,
and your user account's tools/filesystem/network/device access. The existing
entries continue to select the Docker workshop unless this preset is explicit.

Interactive chat defaults to `--agent-prompt codex`: the pinned
[Codex CLI 0.166.0 base instructions](../prompts/codex-cli-0.166.0-base.txt),
followed by an [interactive Playtime adapter](../prompts/codex-playtime-chat-adapter-v2.txt).
The adapter defines the real tools, workspace, terminal controls, and completion
behavior. Playtime's existing harness still owns compaction and execution. The
startup banner names the prompt; the prompt does not change the selected model
or grant additional filesystem/network access.

The complete instructions are frozen into each saved conversation. Start a new
conversation to use the Codex prompt. To select or resume sessions made with the
original short prompt, pass `--agent-prompt minimal`, for example:

```sh
codex-launcher playtime-bird-nvfp4 --sessions --agent-prompt minimal
```

Use that same selection for explicit `--session` and recovery commands. Prompt
changes are never applied silently to saved history. Headless worker and
read-only role prompts remain separate from this interactive selection.

Install from this checkout on the workstation where the project lives:

```sh
./scripts/install-chat-launcher.sh
```

This builds the release client and installs the binary, wrapper, verifier and
reviewed locks under `~/.local/lib/playtime-chat`, outside the writable project.
The wrapper never imports from that directory and runs its two modules from
source, so a file added there, or a `__pycache__`, adds no code. The [headless
worker guide](rust-headless-worker.md) lists the files a worker launch reads.
It requires Python 3.11+ on both ends and the locked local Docker workshop image.
Framework uses the existing `framework` SSH alias and its Gyro owner helpers.
Bird uses `oldman@oldman-bird.local`, `~/.ssh/id_ed25519_oldman_bird_codex` and its
`.local/lib/codex-basalt/basalt-session.py` owner. Install Playtime first,
then run Notes' `oldman-bird/install-basalt-profiles.py` to install that owner,
Codex profiles and both Playtime menu entries. It refuses active clients before
updating. Framework retains its existing `warm-session.py` / Gyro helpers.
The legacy Bird Strata preset remains readable for historical sessions; its old
4090 hardware configuration is no longer available.
The Playtime installer does not update remote helpers, models or engines.

Use a codex-launcher build that supports standalone `[[harness]]` entries. Add
these tables to `~/.codex/codex-launcher.toml` (or the same file in `CODEX_HOME`):

```toml
[[harness]]
name = "playtime-framework-fast"
harness = "Playtime"
description = "Project chat on Framework with the tested Gyro-M fast preset."
launch = ["~/.local/lib/playtime-chat/run-strata.py", "framework-gyro-m-fast"]
status = ["~/.local/lib/playtime-chat/run-strata.py", "--status", "framework-gyro-m-fast"]
model = "qwen3.8-flash-next-gyro-gyro-m"
provider = "Strata on Framework"
context_window = 163840
effort = "medium"
tools = "project Docker; no external network"

[[harness]]
name = "playtime-bird-nvfp4"
harness = "Playtime"
description = "Flash-Next NVFP4-DQ8 on Bird PRO 5000; up to two clients."
launch = ["~/.local/lib/playtime-chat/run-strata.py", "bird-basalt-nvfp4"]
status = ["~/.local/lib/playtime-chat/run-strata.py", "--status", "bird-basalt-nvfp4"]
model = "bird-qwen38-flashnext-nvfp4-dq8-basalt"
provider = "Basalt on Bird"
context_window = 163840
effort = "medium"
tools = "project Docker; no external network"
```

From the project directory, select that entry in the menu, or run:

```sh
codex-launcher playtime-framework-fast
codex-launcher playtime-bird-nvfp4
# One turn, with optional machine-readable event output:
codex-launcher playtime-framework-fast --prompt 'Inspect the project and explain its tests.' --json
```

Use `playtime-bird-iq4` for the workhorse baseline. Both Bird entries share one
Basalt process with Codex and Devbot: at most two clients of the same model,
160K context per client. A third client or a different quant is refused while
leases are held. The last clean release leaves the model warm for 30 minutes.
An interrupted client closes admission until its peer finishes, then stops the
owned server. A new launch waits up to two minutes for this drain, reports why it
is waiting, and can be cancelled with Ctrl-C. If the peer is still active at the
deadline, the launch asks you to let it finish or close it before retrying.
Switching quant with no clients reloads the model. The 3090 stays
idle. The first verified launch hashes the whole pack; later launches can reuse
its verified hash while file identity is unchanged.

Interactive launches show a local session picker **before starting the model**
when a resumable conversation exists. Otherwise they start a new conversation
without prompting. Saved interrupted or unavailable sessions remain untouched;
use `--sessions` to inspect them or request recovery explicitly.
Choose `0` (or Enter) for a new conversation, a numbered resumable session, or
`q` to cancel. The list shows the last user message and age for recent sessions
with the selected workspace and model identity. Empty sessions with no completed
or active turn are omitted; their logs and
session files remain on disk for troubleshooting. Sessions with changed settings,
corrupt checkpoints, or a held lease cannot be selected. Interrupted entries offer
`r<number>` to inspect their report and explicitly fork after typing `recover`.
This requires complete recorded cleanup; it never replays pending commands.
An unused recovery fork remains visible even without a completed turn.
Explicit `--session`, `--recover-compaction`, `--recover-interrupted`, and headless `--prompt` calls
bypass selection. Use a leading `--sessions` to request the picker explicitly:

```sh
codex-launcher playtime-bird-nvfp4 --sessions --workspace /absolute/project
```

Discovery checks at most 200 recent directories and displays up to 20 matches
under `--state-dir` (default `~/.local/state/playtime-chat`). Sessions saved
elsewhere remain available through `--session`. `--list-sessions` emits the same
catalog as JSON without starting inference. `--inspect-interrupted PATH` likewise
prints the validated report locally without acquiring the host. Selection validates the saved
checkpoint, and normal startup validates it again after acquiring the host.

Projects can supply a prepared `.devcontainer/devcontainer.json` environment.
Run `--prepare-environment` explicitly before its first use; normal launches only
verify and select it. See [project environments](rust-project-environments.md) for
the supported definition, offline image contract and saved-session behavior.

The current directory becomes `/work` in the tool container. Explicitly select
another project with `--workspace /absolute/path`. The console prints its private
session directory; pass `--session /that/directory` and the same project/settings
to reopen a completed conversation. Bracketed paste inserts one editable multiline
message; Enter submits it. `/paste` then `/end` is also supported; `/quit` exits.
Escape, Ctrl-C or `/stop` cancels a turn and waits for cleanup. Safe live stops
return to input through a recovery fork; the interrupted source stays fenced and
no input or commands are replayed. Unknown tool outcomes require explicit recovery.
A model cancellation may restart the client after confirmed inference-owner shutdown
and reacquisition; persistent host processes end at that restart. A truncated first compaction with confirmed
cleanup can be explicitly recovered into a new session:

```sh
codex-launcher playtime-framework-fast --recover-compaction /absolute/old-session --summary-tokens 4096
```

This preserves the original evidence and displays the pending message for you to
resubmit. It does not accept the cut-off summary. See the console guide for the
exact recovery conditions and tool limits. Completed sessions created with the
previous 2,048-token summary cap need `--summary-tokens 2048` when reopening.
Saved conversations remain bound to their model lock and settings: resume a Bird
session with the Bird entry, and a Framework session with the Framework entry.
Start a new conversation when switching hosts.

The launcher's effort picker does not translate settings for this independent
program. Effort is part of the reviewed model lock. The wrapper accepts a leading
`--full-verify` and/or `--sessions` before any chat arguments; remaining CLI arguments pass unchanged
to `playtime-chat`. Endpoint, model-lock and image-lock overrides are refused
because this preset owns them. Existing Codex profiles keep their own configuration
and effort controls.

## Inference ownership and verification

`scripts/chat/run-strata.py` owns an SSH forward and a client lease for the whole
chat session. Strata's warm supervisor keeps the original Gyro helper alive and
admits one client. Basalt's owner holds the same exclusive Bird host lock and
admits up to two clients of one quant. Both retain ownership between clients. An idle matching profile is
reused; an idle different profile is stopped through its original helper before
starting the requested one. Active clients cannot be evicted. Menu status checks use the read-only
`--status` path; they never start or hash a model.

After startup, the wrapper reads the helper's own session location and runs the
bundled verifier over SSH. It records actual argv, relevant environment and launch
configuration, hashes the engine executable and Responses source on every launch,
and requires an exact match with the committed model lock. The console separately
verifies the endpoint/template/settings identity. The wrapper never rewrites a
lock to accept a changed engine.

Model/drafter files can reuse hashes from the inference host's private
`~/.cache/playtime-chat/strata-hashes-v1-<reviewed-host-digest>.json`. Each launch
enumerates the current files and checks their device, inode, size, nanosecond
mtime and ctime. A missing or changed entry is hashed again. The verifier checks
the opened file against its pathname and rechecks all identities before publishing
the cache. Only a report matching the reviewed lock can publish it, using an
atomic replacement with mode 0600 in a caller-owned 0700 directory. Malformed or
unsafe caches fall back to hashing; a cache write failure does not invalidate an
otherwise matching report.

The first launch reads all artifacts. Later launches reuse checksums for unchanged
files; this trusts local filesystem metadata and does not check for corruption
that leaves that metadata unchanged. Force a fresh content check with:

```sh
codex-launcher playtime-framework-fast --full-verify --workspace /absolute/project
```

Place `--full-verify` immediately after the preset, before chat arguments. A
successful full check refreshes the cache. Normal verification still checks the
current process/settings and exact reviewed manifest, even when all model hashes
are reused. Status checks never start a model or populate the cache.

Local evidence is private under `~/.local/state/playtime-launcher`. `timings.json`
records model readiness, verification and total time to starting the client;
`host.json` also records hashed/reused file and byte counts. Startup messages show
the phase durations without needing to pipe the interactive prompt through `ts`.
Remote helper logs keep their existing location. The host stays owned while the
console shuts down local tools, with up to 45 seconds for graceful client cleanup.
Only a zero client exit with confirmed cleanup sends `GYRO_RELEASE` before EOF.
The supervisor then verifies that no inference request remains and parks the
model. EOF without that release, lost SSH, verification failure or failed client
cleanup stops Strata immediately. Basalt instead blocks new admission until any
remaining peer releases, then stops the process group. Its systemd cgroup owns
the API and CUDA processes. The Strata helper watchdog retains its existing
supervisor-loss handling; neither owner adopts an unowned listening endpoint. Uncertain helper cleanup fences subsequent admission.
EOF and a terminal SSE event alone are not treated as remote termination proof.

The default is **30 minutes after the client closes**, rather than a timeout
inside an open chat. Warm status appears as `ready`. Idle expiry or explicit stop
stops the owned server. Strata uses its original helper and restoration rules;
Basalt keeps the PRO 5000's existing 300 W limit throughout. Until shutdown the
model retains its GPU/RAM allocations. No model settings or accuracy tradeoff is introduced. To release an
idle host immediately (an active chat makes this command refuse):

```sh
codex-launcher playtime-bird-nvfp4 --stop-server
codex-launcher playtime-framework-fast --stop-server
```

For Strata, the host's private `warm/policy.json` under its existing helper state directory
contains `{"idle_seconds":1800}`. Zero means retain until explicit stop or profile
switch. The policy is read on each clean release. An unavailable idle-status probe
defers expiry and retries; it never authorizes interrupting an unknown request.

The installer includes the historical Strata locks and both reviewed Basalt locks.
Ollama was retired on both hosts on November 13, 2026, including the helpers'
stop/restart hooks. It is not restarted when a console exits. No other active
inference session is evicted to make a test run.

Run the CPU fixtures without contacting a host:

```sh
python3 -m unittest discover -s scripts/chat -p 'test_*.py' -v
```

The warm supervisor's subprocess/socket ownership tests live in Notes at
`agents/test_strata_warm_session.py`; the Codex wrapper uses the same supervisor
and clean-release protocol. Framework's live cold/warm launches and explicit stop
are recorded in [warm-session evidence](../experiments/chat-console/2026-11-13-warm-session.json).

## Session picker check (November 13, 2026)

The installed launcher was exercised through a real terminal: cancel before
model startup, select Bird's saved session, and select a new Framework
conversation. Both live conversations completed a file-reading turn and closed
cleanly. The menus appeared in 0.03–0.08 seconds; model startup followed selection.
Bird resumed the previous completed checkpoint. Both hosts ended idle with
Ollama absent, no running owned project containers, and Bird's original 450/420 W
limits restored. This is a session/lifecycle check, not a harness benchmark.
See [the recorded evidence](../experiments/chat-console/2026-11-13-session-picker.json).

They exercise refusal, handoff and cleanup with fake SSH and client processes.
Real project tool behavior, server configuration and restoration require the
separate owned live smoke.

## Initial live check

The November 13 [launcher evidence](../experiments/chat-console/2026-11-13-launcher-framework-fast.json)
records a real current-directory project repair through the installed launcher:
four tests passed, the agent turn took 13.8 seconds, and the full startup/work/
restoration took 84.4 seconds. Full artifact verification accounted for 46.1
seconds. A separate SIGINT during reasoning stopped the client, cleaned up its
project container and restored Framework in about two seconds. Both runs used a
32K harness budget with the server still allocated 160K; these are integration
smokes, not full-context throughput or coding-quality comparisons.

The subsequent [cache comparison](../experiments/chat-console/2026-11-13-cached-verification.json)
used the installed launcher for a full check and a cached check, each followed by
the same successful project-tool smoke and confirmed host restoration. Time to
starting the chat client fell from 65.9 to 20.8 seconds; verification fell from
45.9 to 0.3 seconds. The cached launch reused all 15 model/drafter file hashes
(93.8 GB) while freshly hashing the engine and Responses source (52.4 MB).
These are fresh-session startup measurements, not saved-history reopening or
generation benchmarks. The cache remains populated for subsequent normal launches.

The [Bird launcher check](../experiments/chat-console/2026-11-13-launcher-bird-fast.json)
qualified `playtime-bird-fast` at its full 131,072-token allocation. A short first
turn wrote and read a project file; a second launch reopened the same saved
conversation and read the file using the retained history. Both turns completed,
and both helper exits restored the original 450/420 W limits and active Ollama.
The initial artifact check took 80.3 seconds; the cached check took 0.4 seconds,
with 35.8 seconds total to starting the resumed chat client. This exercises the
CUDA console, tools, resume and normal cleanup; it does not measure a populated
128K context or coding quality.

## One-shot launch contract

A caller can run `~/.local/lib/playtime-chat/run-strata.py PROFILE --describe-launch` without
acquiring a model. The JSON response identifies schema 1, `launcher_v1`, the prompt
byte limit and a profile-specific cleanup timeout. This is capability inspection,
not readiness or a reservation. Model and effort settings remain profile-owned.
Both launch flags must directly follow the profile.

`~/.local/lib/playtime-chat/run-strata.py PROFILE --launch-request /absolute/request.json` accepts
only that request, without additional chat flags:

```json
{"schema_version":1,"launch_id":"example","workspace":"/absolute/project","prompt_file":"/absolute/prompt.md","prompt_sha256":"<64 lowercase hex digits>","receipt_file":"/private/caller/receipt.json","timeout_seconds":600}
```

The request and prompt must be bounded regular files; keys are strict. The receipt
parent must already exist, belong to the caller and have no group/other access.
The wrapper retains an exclusive `receipt.json.started` claim before acquiring
an actor. A claim or existing receipt rejects re-entry, including after a crash;
a fresh deliberate attempt needs a new receipt path; the launch ID only correlates
the request with its receipts. Do not remove a claim to retry an
uncertain task. The claim is evidence, not a recovery command.

The wrapper freezes the validated prompt into its private evidence, selects a
fresh `coding-host` one-shot conversation and uses the existing owner, model
verification and chat path. The task timeout includes startup, verification and
the turn. SIGINT, SIGTERM or SIGHUP requests cancellation, including while the
request is still being claimed; the caller must then allow the
advertised cleanup budget before deciding that ownership is unresolved. Host
coding remains trusted same-user execution with its ordinary network access.

The terminal receipt adds `inference_owner` to the client's completion fields.
Each cleanup field is `confirmed`, `not_acquired`, or `unresolved`. Tool confirmation
requires the client's correlated completion and a reaped client; inference
confirmation requires normal owner release without transport failure or cleanup
deadline escalation. A normal release may leave a shared model warm. Process exit
alone does not substitute for either acknowledgement. Publication exposes complete
JSON atomically and never overwrites another receipt. File and parent-directory
sync must succeed before the wrapper acknowledges publication.

The client's raw output (`client.stdout`, `client.stderr`), the helper's log
(`owner.log`), the copied prompt, the client completion and `exit.json` stay in the
launcher's private evidence directory, which the wrapper's first stderr line names.
The wrapper writes only a few short status lines to stderr itself, so a caller that
leaves its pipes unread can't stall the deadline or cleanup. Missing or invalid
receipts must remain unknown to callers; this interface never automatically
resumes/replays a request.

On November 20, 2026, a separately installed copy of this launcher ran these
requests on Framework with `framework-gyro-m-fast`:

- A one-turn repair from a cold start finished in 42 seconds. The model fixed a
  one-line bug and ran the project's test. The receipt recorded `completed` with
  both cleanups `confirmed`, and the model was kept warm. The caller's stdout
  stayed empty and its stderr held four status lines; the client's output and the
  helper's log were in the evidence directory.
- Replaying that request was refused before anything was acquired. Its receipt
  was unchanged and no run directory was created.
- A second request received SIGTERM 20 seconds into its turn. The wrapper exited
  143 two seconds later with `cancelled` and both cleanups `confirmed`, and the
  server stopped instead of staying warm.
- A third received SIGTERM 19 ms after starting, once its claim existed. The
  wrapper exited 143 with `cancelled` and both cleanups `not_acquired`, without
  creating a run directory.
- One attempt failed before SSH connected, because `framework.local` briefly
  didn't resolve. Its receipt recorded `failed`, with `local_tools`
  `not_acquired` and `inference_owner` `unresolved`: an SSH failure can't prove
  that nothing started on the host.

These are integration checks, not coding-quality measurements.
