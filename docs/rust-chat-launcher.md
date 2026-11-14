# Launching project chat from codex-launcher

The first pilot entry is `playtime-framework-fast`. It runs the standalone
[project chat console](rust-chat-console.md) with Framework's reviewed Gyro-M
DFlash2 preset, 163,840 server context tokens, medium work thinking and no
summary thinking. Codex does not participate in the agent loop.

Install from this checkout on the workstation where the project lives:

```sh
./scripts/install-chat-launcher.sh
```

This builds the release client and installs the binary, wrapper, verifier and
reviewed locks under `~/.local/lib/playtime-chat`, outside the writable project.
It requires Python 3.11+ on both ends, the existing `framework` SSH alias and
remote Gyro session helper, plus the locked local Docker workshop image.
It does not install or update the remote helper, model or engine.

Use a codex-launcher build that supports standalone `[[harness]]` entries. Add
this table to `~/.codex/codex-launcher.toml` (or the same file in `CODEX_HOME`):

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
```

From the project directory, select that entry in the menu, or run:

```sh
codex-launcher playtime-framework-fast
# One turn, with optional machine-readable event output:
codex-launcher playtime-framework-fast --prompt 'Inspect the project and explain its tests.' --json
```

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

The launcher's effort picker does not translate settings for this independent
program. Effort is part of the reviewed model lock. The wrapper accepts a leading
`--full-verify` before any chat arguments; remaining CLI arguments pass unchanged
to `playtime-chat`. Endpoint, model-lock and image-lock overrides are refused
because this preset owns them. Existing Codex profiles keep their own configuration
and effort controls.

## Inference ownership and verification

`scripts/chat/run-strata.py` owns an SSH forward and the installed remote helper
for the whole chat session. The helper takes its exclusive host lease and refuses
busy Strata/Ollama/GPU workloads. Menu status checks call only its read-only
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
console shuts down local tools, with up to 45 seconds for graceful client cleanup. Closing the
helper's stdin then asks it to stop its server and restore the host. Lost SSH or
verification failure stops the local client; unconfirmed host restoration returns
failure. The remote helper's existing heartbeat watchdog handles lost owners.
EOF and a terminal SSE event alone are not treated as remote termination proof.

The installer currently includes only Framework's tested fast preset. Bird's
transport qualification does not establish this console ownership path; add its
entry only after an owned end-to-end test. No other active inference session is
evicted to make a test run.

Run the CPU fixtures without contacting a host:

```sh
python3 -m unittest discover -s scripts/chat -p 'test_*.py' -v
```

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
