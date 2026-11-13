# Strata transport

`playtime-strata` implements the model-transport part of
[#207](https://github.com/aortez/playtime/issues/207). It accepts the shared
`ModelRequest` and returns canonical `ModelResponse` / `Outcome` values. Engine
policy, private conversation storage, server ownership, tools and presentation
remain outside the adapter. The live developer example connects it to
`playtime-harness::Session` and the existing isolated Docker workshop.

The initial profile is a single-slot, already loaded Strata server reached through
loopback HTTP, normally an independently owned SSH forward. Requests use the
stateless `/v1/responses` dialect with complete history, `store:false`, streaming
and a strictly positive output limit. No Responses state is stored remotely;
`previous_response_id`, hosted tools, images and background responses are not
supported. No ambient proxy, redirect, HTTP retry or implicit authentication is
used. Other serving dialects remain future adapters.

## Identity and settings

`inspect_endpoint` reads `/props`, `/settings` and `/v1/status` under a bounded
deadline. `ModelLock` freezes model/path, engine version, actual template hash and
capabilities, context, generation defaults and shared settings. Before every POST,
`Client` rechecks these and refuses a busy endpoint or a changed process start
identity. Activity checks do not acquire ownership and do not prevent a second
client racing the POST; the caller must hold the host's exclusive session lease.

The caller also supplies `HostIdentity`: backend, quantization, executable hash,
artifact-manifest hash, launch-configuration hash and Responses-source hash. This
is an attestation by the owner, not a fact established by deserializing JSON or
reading HTTP metadata. Verify weights, packs/tokenizer, executable and actual
launch arguments/environment before lending the endpoint. Bind drafting, KV
format, PLE placement, route approximations and other engine flags through that
configuration identity. Refuse a changed identity instead of refreshing a lock
while resuming a conversation. The complete lock fingerprint becomes the
conversation's `model` digest.

The reviewed source profile is Strata engine 0.1.57 / 0.1.58 with
`serve/responses.py` SHA-256
`aba98b4aba933fe55cf699985add2e1014a66dacb55ed768ca9aa404c3fcb29b`.
Both installed copies had these bytes; only the Framework HIP target has been
qualified live here. A newer source/version needs review and new evidence.

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

## Live smoke

First acquire an idle target through its existing host session helper; do not
stop or attach to another user's active conversation. Keep that helper's
heartbeat alive. Its exclusive lock and restoration rules remain authoritative.
The examples never start/stop SSH, Strata, Ollama or power settings themselves.
Run only one live model test at a time.

Hash and record the owner's actual artifacts/configuration separately. Supply a
JSON evidence file containing a `host` object with the `HostIdentity` fields.
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
The console/launcher work remains in #210 and codex-launcher #14.

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

Bird's existing startup check reported its RTX 3090 unavailable to the NVIDIA
driver. No CUDA inference or reboot was attempted in this work; CUDA qualification
and a usable console remain outstanding.
