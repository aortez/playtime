# Launching project chat from codex-launcher

The launcher runs the standalone [project chat console](rust-chat-console.md)
with the host's reviewed Gyro-M DFlash2 preset:

| Entry | Inference host | Server context |
| --- | --- | --- |
| `playtime-framework-fast` | Framework, HIP | 163,840 tokens |
| `playtime-bird-fast` | Bird, CUDA on the 4090 and 3090 | 131,072 tokens |

Both use medium work thinking and no summary thinking. Codex does not participate
in the agent loop. Project tools run on the workstation that launches the console.

Install from this checkout on the workstation where the project lives:

```sh
./scripts/install-chat-launcher.sh
```

This builds the release client and installs the binary, wrapper, verifier and
reviewed locks under `~/.local/lib/playtime-chat`, outside the writable project.
It requires Python 3.11+ on both ends and the locked local Docker workshop image.
Framework uses the existing `framework` SSH alias and its Gyro owner helpers.
Bird uses `oldman@oldman-bird.local`, `~/.ssh/id_ed25519_oldman_bird_codex` and its
existing `.local/lib/codex-gyro/gyro-session.py` helper. Each host also needs
`warm-session.py` beside that helper; the source is Notes'
`agents/strata-warm-session.py`. The Notes profile installers deploy both files.
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
name = "playtime-bird-fast"
harness = "Playtime"
description = "Project chat on Bird with the tested Gyro-M fast preset."
launch = ["~/.local/lib/playtime-chat/run-strata.py", "bird-gyro-m-fast"]
status = ["~/.local/lib/playtime-chat/run-strata.py", "--status", "bird-gyro-m-fast"]
model = "qwen3.8-flash-next-gyro-gyro-m"
provider = "Strata on Bird"
context_window = 131072
effort = "medium"
tools = "project Docker; no external network"
```

From the project directory, select that entry in the menu, or run:

```sh
codex-launcher playtime-framework-fast
codex-launcher playtime-bird-fast
# One turn, with optional machine-readable event output:
codex-launcher playtime-framework-fast --prompt 'Inspect the project and explain its tests.' --json
```

Interactive launches show a local session picker **before starting the model**.
Choose `0` (or Enter) for a new conversation, a numbered resumable session, or
`q` to cancel. The list shows the last user message and age for recent sessions
with the selected workspace and model identity. Empty sessions with no completed
or active turn are omitted; their logs and
session files remain on disk for troubleshooting. Sessions with changed settings,
unfinished turns, corrupt checkpoints, or a held lease cannot be selected.
Unfinished turns require separate inspection/recovery; the picker never replays
them. Explicit `--session`, `--recover-compaction`, and headless `--prompt` calls
bypass selection. Use a leading `--sessions` to request the picker explicitly:

```sh
codex-launcher playtime-bird-fast --sessions --workspace /absolute/project
```

Discovery checks at most 200 recent directories and displays up to 20 matches
under `--state-dir` (default `~/.local/state/playtime-chat`). Sessions saved
elsewhere remain available through `--session`. `--list-sessions` emits the same
catalog as JSON without starting inference. Selection validates the saved
checkpoint, and normal startup validates it again after acquiring the host.

The current directory becomes `/work` in the tool container. Explicitly select
another project with `--workspace /absolute/path`. The console prints its private
session directory; pass `--session /that/directory` and the same project/settings
to reopen a completed conversation. `/paste` starts multiline input, `/end`
submits it and `/quit` exits. Ctrl-C or `/stop` cancels a turn and waits for cleanup;
interrupted sessions remain fenced. A truncated first compaction with confirmed
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

`scripts/chat/run-strata.py` owns an SSH forward and an exclusive client connection
to the host's warm supervisor for the whole chat session. The supervisor keeps
the original Gyro helper alive, holding its existing exclusive host lease even
between clients. One client at a time may use it. An idle matching profile is
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
cleanup stops the owned server. The original helper's watchdog still handles
supervisor loss; the supervisor cannot outlive that lease by merely discovering
a listening endpoint. Uncertain helper cleanup fences subsequent admission.
EOF and a terminal SSE event alone are not treated as remote termination proof.

The default is **30 minutes after the client closes**, rather than a timeout
inside an open chat. Warm status appears as `ready`. Idle expiry or explicit stop
closes the original helper, which stops its owned server and restores Bird's GPU
power limits. Until then GPU/RAM allocations and the serving power limits remain
in place. No model settings or accuracy tradeoff is introduced. To release an
idle host immediately (an active chat makes this command refuse):

```sh
codex-launcher playtime-bird-fast --stop-server
codex-launcher playtime-framework-fast --stop-server
```

The host's private `warm/policy.json` under its existing helper state directory
contains `{"idle_seconds":1800}`. Zero means retain until explicit stop or profile
switch. The policy is read on each clean release. An unavailable idle-status probe
defers expiry and retries; it never authorizes interrupting an unknown request.

The installer includes both reviewed fast model locks.
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
