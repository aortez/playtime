# Strata and Basalt Responses transport

`playtime-strata` implements the model-transport part of
[#207](https://github.com/aortez/playtime/issues/207). It accepts the shared
`ModelRequest` and returns canonical `ModelResponse` / `Outcome` values. Engine
policy, private conversation storage, server ownership, tools and presentation
remain outside the adapter. The live developer example connects it to
`playtime-harness::Session` and the existing isolated Docker workshop.

Supported profiles are a single-slot Strata server or a one/two-slot Basalt server reached through
loopback HTTP, normally an independently owned SSH forward. Requests use the
stateless `/v1/responses` dialect with complete history, `store:false`, streaming
and a strictly positive output limit. No Responses state is stored remotely;
`previous_response_id`, hosted tools, images and background responses are not
supported. No ambient proxy, redirect, HTTP retry or implicit authentication is
used. Other serving dialects remain future adapters.

## Identity and settings

`inspect_endpoint` reads `/props` and `/v1/status` under a bounded deadline.
Strata also supplies `/settings`; Basalt 0.4.1 has no such route and its lock
records `shared_settings: null`. `ModelLock` freezes model/path, engine version, actual template hash and
capabilities, context, generation defaults and shared settings. Before every POST,
`Client` rechecks these and the process start identity. Strata refuses any
in-flight request. Basalt permits an occupied peer slot and refuses a full
endpoint. Activity checks do not reserve capacity: the caller must retain the
external owner lease, with at most two clients sharing one Basalt model. The
owner retains the exclusive host lock against other serving runtimes.

The caller also supplies `HostIdentity`: backend, quantization, executable hash,
artifact-manifest hash, launch-configuration hash, Responses-source hash and, for
Strata, server-release manifest hash. This is an attestation by the owner, not a
fact established by deserializing JSON or reading HTTP metadata. Verify weights,
packs/tokenizer, executable and actual launch arguments/environment before
lending the endpoint. Bind drafting, KV
format, PLE placement, route approximations and other engine flags through that
configuration identity. Refuse a changed identity instead of refreshing a lock
while resuming a conversation. The complete lock fingerprint becomes the
conversation's `model` digest.

The reviewed source profile is Strata engine 0.1.57 / 0.1.58 with
`serve/responses.py` SHA-256
`aba98b4aba933fe55cf699985add2e1014a66dacb55ed768ca9aa404c3fcb29b`.
Both installed copies had these bytes. Framework HIP and Bird CUDA have now
passed the live tool/follow-up and cancellation smoke with DFlash2 enabled.
A newer source/version needs review and new evidence.

Each Strata lock also pins the complete server release for its engine version.
`server_release_manifest_sha256` is the digest of every file in the release
directory the API server runs from, by path inside the release. Symlinks are
recorded as links, and `__pycache__` directories are left out because Python
writes them at runtime. Validation accepts only these reviewed pairs:

| Engine | Files | Symlinks | Manifest SHA-256 |
| --- | --- | --- | --- |
| 0.1.57 | 6,169 | 1,047 | `0e18282520906d8c01153ea0f6cf14fc2746847d06bf9ed5b96ae63f1a294797` |
| 0.1.58 | 6,176 | 1,047 | `f49826e2c75d33790f0bce86651e740c0b40fe34c9e63e31b1134155e79466b6` |

The engine component's own libraries and kernels, the running API server process
and stale bytecode caches remain unchecked; see #235.

`framework-strata-gyro-m-fast-v2.lock.json` and
`bird-strata-gyro-m-fast-v2.lock.json` are their v1 locks with this field added,
and the presets use them. A Strata lock
without the field no longer validates. A v2 lock requires everything its v1 lock
did, so a conversation saved under the v1 fingerprint resumes under that saved
binding (`ModelLock::earlier_fingerprint`); new conversations take the v2
fingerprint. The drafting-off `framework-strata-gyro-m-v1.lock.json` stays as the
record of the 2026-11-13 result and has no successor.

Basalt schema-2 locks additionally bind `service: basalt` and slot count. The
reviewed runtime is CUDA Basalt 0.4.1, engine SHA-256
`5a15f95f8bf41fbd61c8e7299910d08049243eb32b26699b0b7a6f7976ad92e8`,
Responses source SHA-256
`c1ba29dd88ac98f052b34e75de46106737a559871c75eb565fb852930e7bcf23`.
The host verifier checks the API process, its CUDA child, selected GPU, complete
model pack, serving Python/templates and launch configuration before lending it.
`bird-basalt-iq4-v1.lock.json` and `bird-basalt-nvfp4-v1.lock.json` freeze the
PRO 5000 profiles with two 163,840-token slots and MTP8. Both passed the owned
live Docker tool/follow-up and local cancellation smoke. Basalt requests explicitly
disable relaxed drafting. Existing schema-1 Strata fingerprints remain unchanged.

Sampler settings are explicit on every request: temperature, top-p/top-k/min-p,
repetition/presence/frequency penalties, penalty window and the controller's seed.
Work and summary requests can select different thinking efforts. The adapter
passes `none`, `low`, `medium`, `high` or `xhigh`; the selected model template
determines their meaning. The initial lock uses medium for work and none for
summaries. A reasoning budget is not manufactured from the output allocation.

## Streaming, failures and accounting

The decoder validates SSE sequence, response/item identities, output order and
terminal consistency. Text, reasoning and function arguments assemble separately.
Only complete function items with object-valued JSON arguments become executable
calls. IDs survive the tool-result round trip; reasoning is returned as a
`reasoning_text` input item rather than being silently discarded.

`events()` provides a bounded channel. Drain it concurrently with `complete`.
`StreamEvent::controller_chunk` exposes compact text/reasoning/argument deltas for
the controller; keep full events separately when diagnostic evidence is needed.
Raw SSE snapshots are intentionally not charged as repeated generated text.
If using `Decoder` directly, drain `take_events` after each `push`, even after an
error: valid frames before a malformed frame in the same read remain available.

Local limits bound metadata, encoded input, SSE frames, received bytes, assembled
output and event count. Cancellation and absolute deadlines cover metadata,
headers, stream reads and blocked event consumers. `CallResult` retains partial
items and distinguishes not-dispatched, uncertain and terminal-observed states.
None of these is remote termination authority. Closing a connection cancels local
work; the server owner must observe/stop its own invocation and finish cleanup.
The adapter never retries a possibly dispatched request.

Only terminal usage can settle generated tokens. Missing usage remains `None`,
including after disconnect/cancellation. Malformed tool arguments with validated
terminal usage map to `ModelParseError` with the measured count; shared controller
policy decides whether another bounded attempt is allowed. A length-limited
response preserves usage and partial text but supplies no executable calls.

## CPU checks

```sh
cargo test -p playtime-strata --locked
```

The fixtures cover byte-fragmented UTF-8/SSE, reasoning and multiple calls,
stateless follow-up encoding, malformed/incomplete calls, unknown/contradictory
usage, identity changes, byte/event limits, partial evidence, HTTP failure,
disconnect, cancellation, deadlines and consumer backpressure. Their provenance
is in [the fixture README](../crates/playtime-strata/tests/README.md). They launch
no inference server, GPU workload or workshop. Run the repository-wide Rust and
frozen Python checks as well.

## Archived experiment worker (#233)

The packaged experiment worker also reaches Strata, for the archived live smoke
and memento recipes. `Profile::freeze_strata_live` freezes a `LiveModel::Strata`
record: the reviewed lock by name, the request settings (the lock's own) and what
the daemon's hold verified, namely the launcher profile, the loopback endpoint the
hold lent, the server process's `started` value and the verifier report, whose
host identity must be the lock's. `Profile::freeze_strata_recipe` and
`Profile::freeze_strata_definition` freeze the same record for a catalog or saved
recipe, whose request settings are the recipe's sampling over the lock's; see
[recipes on a Strata target](rust-recipes.md#on-a-strata-target). The worker
build compiles the reviewed locks it runs, now `framework-strata-gyro-m-fast-v2`,
a copy of the `models/` lock that a test keeps equal. Validation refreezes the
record, so another lock, other settings or a host that doesn't match the lock is
refused, and a run whose context budget exceeds the lock's context doesn't
freeze. Archived Ollama models have no `provider` field and keep their bytes. The
daemon freezes these profiles on a
[Strata target](rust-inference-targets.md#strata-targets), from the hold it takes
for the run, and the host record keeps that target, whose profile must be the
host's. Continuous profiles refuse Strata for now.

The worker binds its client to the frozen `started` value with
`Client::with_server_start`, so even its first check refuses another server
process. It checks the server at startup, before Begin, at each session start and
before each request, and refuses a busy one. As in the chat driver, a request
whose outcome is uncertain fences the port: nothing more is sent, not even a
check. A request whose terminal event arrived, including a tool-parse failure the
controller retries, doesn't.

Streamed deltas reach the controller as `model_chunk` records while the request
runs. They're coalesced like Ollama's native records, into at most one chunk
every half second or 16 KiB of text. Each `strata_deltas` chunk carries a
`message` with `thinking` and `content`, the shape the workbench and the trace
already render, then the function-call argument deltas by item and name, and
its delta count. After every request one `strata_call` chunk records the remote
state (`not_dispatched`, `uncertain` or `terminal_observed`), the event count,
received bytes, the latency the worker observed and, when it arrived, the
terminal event. That event keeps its status, usage, timings and errors as sent;
each output item keeps its identity and a `text_bytes` size, but not the text,
which the response record already holds and which would otherwise count twice
against the response's 4 MiB budget. Strata reports none of Ollama's durations,
so the run's contention summary stays unknown.

`cargo test -p playtime-worker --locked` runs the worker against a scripted
loopback server, [`server.rs`](../crates/playtime-strata/tests/support/server.rs), on its own threads because the
driver under test is synchronous. Its metadata produces the synthetic
`fixture-strata-gyro-m` lock, which only the runner's `strata-fixture` feature
compiles: the worker enables it for its tests, and it stays out of the profile
digest. The tests cover a two-session run, a restarted server at a session
start, refusals before Begin, a cut stream, missing usage, partial output
committed during a slow response, the fence after cancellation or a deadline,
and a retried tool-parse failure.

## Live smoke

First acquire an idle target through its existing host session helper; do not
stop or attach to another user's active conversation. Keep that helper's
heartbeat alive. Its exclusive lock and restoration rules remain authoritative.
The examples never start/stop SSH, Strata, Ollama or power settings themselves.
Run only one live model test at a time.

Hash and record the owner's actual artifacts/configuration separately. Supply a
JSON evidence file containing a `host` object with the `HostIdentity` fields.
On a held host, `verify-strata-host.py SESSION --discover` writes one without
publishing a hash cache, and lists the server release's full manifest for review.
Then inspect the owned endpoint and write a **new** lock:

```sh
cargo run -p playtime-strata --example inspect-lock --locked -- \
  http://127.0.0.1:18080/v1 /absolute/host-evidence.json /absolute/new-lock.json
cargo run -p playtime-strata --example workshop-smoke --locked -- \
  http://127.0.0.1:18080/v1 /absolute/new-lock.json \
  environments/general.lock.json /absolute/new-evidence-directory
```

The smoke requires the existing locked image and local rootful Linux Docker
socket. It creates a private evidence directory, commits resource ownership,
prepares a random file in an isolated uid-1000 workshop, and asks the model to read
and return it. At least two model requests and one real tool call must finish
successfully. The controller allows 240 seconds total, 90 seconds per inference,
512 tokens per call and 2,048 generated tokens per turn. Its same chat admission,
streaming accounting, tool dispatch, saved-session and cleanup paths are used.
This short smoke does not force or qualify live compaction.

A separate request is then cancelled after its first observed delta, with a
90-second absolute bound. The example records local cancellation and uncertain
remote state. Independently record `/v1/status` and `/slots` idle observations,
close the owned helper's input, and retain its server-stop/restoration result.
The gate does not grant authority to terminate unrelated processes. Retain
failed attempts in separate directories.

This is a developer gate, not the interactive driver: it has a fixed task and a
disposable workspace, and it does not implement console input, SIGINT policy,
an editable project workspace or automatic remote reconciliation. A persistence
failure leaves the session fenced and runs outer owned-workshop cleanup. Docker's
absolute lifetime and the model task's deadline continue independently of fsync.
The separate [project console](rust-chat-console.md) now supplies an async driver
and editable project tools for #210. Launcher-managed startup remains in
codex-launcher #14.

## Recorded Framework result (2026-11-13)

[The saved report](../experiments/strata-adapter/2026-11-13-framework-smoke.json)
and [model lock](../models/framework-strata-gyro-m-v1.lock.json) identify the
qualified target: Gyro-M TQ2_0, HIP engine 0.1.58, context 163,840, int8 KV,
drafting off, PLE in RAM and route approximations disabled.

The live turn used two model requests and one `read_file` call, finished in
9.701 seconds, and accounted for 141 generated tokens with no unknown requests.
Strata reported decode rates of 28.2 and 28.1 tokens/second for those two short
requests. This is a transport smoke, not a sustained performance or quality
benchmark. Cancellation preserved partial output and unknown usage; later status
showed zero in-flight requests and an idle slot. Workshop resources were confirmed
absent, the owned server stopped, and the helper restored Ollama successfully.

The initial Bird startup check reported its RTX 3090 unavailable to the NVIDIA
driver, so that attempt did not run CUDA inference. A later retry on the same
date found both GPUs and passed the fast-profile smoke below. No reboot or GPU
repair was performed by these tests. Later project-console evidence is recorded
in the [console guide](rust-chat-console.md#framework-fast-profile-smoke-november-13).

## Preferred fast profiles (2026-11-13)

Use `framework-gyro-m-fast` for the Framework pilot and `bird-gyro-m-fast` for
Bird's initial harness work, per the user's preference. These are the existing
host-owned presets with DFlash2 enabled and medium work thinking. Framework
allocates 163,840 context tokens; Bird allocates 131,072. Keep these distinct
from the earlier drafting-off lock and use a new saved conversation when the
model lock/settings change. The non-fast preset remains available for comparison.

The [Framework fast report](../experiments/strata-adapter/2026-11-13-framework-fast-smoke.json)
and [lock](../models/framework-strata-gyro-m-fast-v1.lock.json) identify HIP
engine 0.1.58 and the actual DFlash2 pack/arguments. Its two model calls and one
real `read_file` call finished in 6.135 seconds with 113 generated tokens and
complete usage accounting. Reported decode rates were 50.1 and 63.7 tokens/second,
compared with 28.2 and 28.1 in the earlier drafting-off smoke. Cancellation passed,
the server was observed idle, owned workshop resources were removed, and the
helper stopped its server and restored Ollama.

The [Bird fast report](../experiments/strata-adapter/2026-11-13-bird-fast-smoke.json)
and [lock](../models/bird-strata-gyro-m-fast-v1.lock.json) record CUDA engine
0.1.57, both the RTX 4090 and RTX 3090 loaded, and the actual drafter arguments
and file hashes. Its two model calls and one real `read_file` call finished in
3.536 seconds with 118 generated tokens and complete usage accounting. Reported
decode rates were 131.5 and 197.8 tokens/second. Cancellation passed, the server
was observed idle, owned workshop resources were removed, and the helper stopped
its server, restored the original 450/420 W limits and restarted idle Ollama.

These short, sampled responses use different generated text and random file
contents. Their timing is integration evidence, not a controlled speedup ratio,
a populated-context test or an accuracy comparison.

Basalt 0.4.1 reports `activity.in_flight` as a busy flag plus queued requests.
Admission therefore also reads `/slots`, validates IDs/context/count and boolean
processing states, and counts active slots. Missing or malformed slot metadata
rejects dispatch; a two-busy-slot regression keeps the status busy flag at one.
