# Native Ollama adapter — #55 / #23 O1

`playtime-ollama` implements independent native HTTP(S) adapter primitives. It
accepts the engine's already-admitted `ModelRequest`, preserves native observations
and returns one result. It does not execute tools, retry, settle canonical usage,
change model locks, supervise Ollama, or integrate the daemon/packaged worker.

The only supported profile is Ollama **0.32.3**, **gpt-oss:20b**, digest
`17052f91a42e97930aa6e28a6c6c06a983e6a58dbb00434885a0cf5313e376f7`, definition
SHA-256 `5968b9af1d86b0583a95912df186c2c8ec6dc1a70a7e2bc612a294908402d912`.
The committed lock is never regenerated. Additional backend versions or models
need a reviewed profile. The pinned [upstream API types][types] have no modern
`show.thinking.values/default` requirement. `think: "low"` is supported;
`None` omits it. Boolean false and `"off"` are rejected rather than equated with
low/default. Expanded reasoning controls remain #5/#7.

## Interface and ownership

- `Adapter::new(transport, lock, limits)` binds an immutable endpoint/lock pair.
  `NativeTransport` uses explicit `Endpoint` and owned `Trust`, either bundled
  WebPKI roots or bounded PEM bytes. Explicit PEM trust replaces the public
  roots, is never read from an ambient file, and must be archived as frozen
  configuration by O2. Client certificates/service bearer credentials have no
  input path. Static Rustls/AWS-LC verifies certificates and hostnames.
- `inspect(loaded, deadline, cancel)` only calls version, tags, show, and optionally
  ps. The five-field definition projection treats missing fields as null. It
  checks the exact profile, digest, definition and tools capability, and rejects
  reported cloud/remote delegation. Extra metadata remains bounded diagnostic
  data. No warmup, model management or inference occurs. Optional loaded state
  is presentation, never request termination evidence.
- `chat(request, settings, absolute_deadline, cancel, observations)` checks
  identity before this adapter's first inference. Failed explicit inspection or
  an observed response identity change invalidates its cached verification.
  Metadata is not an atomic model-name lease or backend-executable attestation.
  O2 must construct/revalidate the adapter in the executing worker.
- Effective context/sampling settings preserve values and omissions: zero
  temperature/top-p/top-k/repeat penalty and `use_mmap: false` survive encoding.
  The exact admitted cap/seed/messages/tools are sent, with stream true and
  frozen keep_alive `"5m"`. The adapter never computes context budgets or changes
  the cap. The engine retains the original request and allowance.
- `observations(limits)` provides a bounded queue. Each complete raw record is
  enqueued before interpreting its terminal outcome. Invalid records have a
  `ProtocolFragment` observation. Persistence must run independently of polling
  chat. The queue never blocks: overflow/disconnection fails with `ByteLimit`,
  retaining the unqueued record in `pending_fragment`. Partial text, thinking,
  and complete tool objects remain data and never authorize dispatch.
- `ChatResult` separates `outcome`, `dispatch` and `remote`. A pre-dispatch failure
  is `NotDispatched/NotStarted`; a possibly sent failed request is uncertain.
  A validated done record can be `TerminalObserved` even with unknown usage.
  This is an API terminal observation, not a supervisor/cgroup termination
  receipt. Only the controller settles known usage or the **original full cap**
  for an unknown dispatched request. Overruns are returned unchanged.
- There is no adapter receive task. The caller owns/polls chat; dropping its
  future leaves remote uncertainty for O2. The native HTTP connection driver
  may have bytes in flight; the upload producer is fenced on request drop.
  Closing a socket, expiration, ps output or idle GPU cannot prove backend stop.
  O2 must persist uncertainty before dispatch and fence backend reuse across
  cancellation, worker loss and daemon restart. This crate supplies no such
  durable ownership authority.

`Transport` allows injected streams for CPU-only fixtures. An implementation
must release its owned future/body on drop and must not replay requests. The
native transport retains the configured base path, forbids URL credentials,
query and fragment, ignores proxy environment variables, rejects redirects,
disables all reqwest retries and decompression, and requests identity encoding.
It accepts no backend-supplied URL as a new destination.

## Bounded parsing and deadlines

| Work | Maximum (caller can reduce) |
| --- | --- |
| Metadata | 16 MiB/response; 15 seconds/request, 45 seconds/inspection |
| Chat request body | 4 MiB before dispatch |
| NDJSON record | 512 KiB including LF/CRLF |
| Chat body before terminal | 4 MiB, 65,536 records |
| JSON nesting | 64 containers |
| Assembled message | 4 MiB cumulative UTF-8 text/thinking and encoded tool-array bytes |
| Observation queue | 65,536 records and 4 MiB raw bytes |
| Chat HTTP error capture | 512 KiB, explicit truncation/incomplete flag |
| Connect/setup | 5 seconds, shortened by caller deadline |

The caller's one absolute deadline covers setup, upload, headers, bytes and error
capture. Connect timeout is not an idle generation timeout. Cooperative yields
between bounded body fragments prevent always-ready streams from starving cancel
and timers; synchronous parsing itself is bounded. No timer is restarted by a
new chunk. Error capture includes a bounded prefix and records truncation on
size, timeout, disconnect or cancellation.

The decoder accepts split UTF-8, LF/CRLF and a final complete record without LF.
It rejects blank records, malformed JSON/types, duplicate keys and excess depth.
Content/thinking append in arrival order. Entire tool objects, including IDs,
indexes, arguments and extras, append without deduplication or string-delta
merging. Final-frame message fields are retained. The first valid done record
ends reception; trailing bytes/duplicate done records are discarded. An error
envelope takes precedence over same-frame message/done/usage.

Generated usage is only a terminal nonnegative JSON integer representable as
u64. Zero is known zero; missing/null/negative/fraction/bool/overflow is unknown.
Prompt usage is separate and never added to generation. Nonterminal counts are
ignored. Only the exact legacy prefix `error parsing tool call:` has the separate
`BackendToolParse` classification. It is not a guaranteed upstream error code;
malformed protocol, lookalikes and truncated errors never become retryable
controller `ModelFailure::Parse`. The adapter issues no retry in either case.

## Evidence and deliberate Python differences

`tests/fixtures/generate.py` is an independently authored **Python stdlib test
oracle**, not runtime code or a test-time regeneration step. Checked-in canonical
bytes cover the existing 8,845-byte definition, all three #23 vectors, Unicode
surrogates/escapes, integer and float boundaries, and 256 deterministic finite
binary64 values. Native Rust reproduces Python sorted compact ASCII encoding,
including float versus integer and negative zero. Identity integers outside
i64/u64, float overflow/underflow and unpaired surrogate strings are explicitly
unsupported; they are rejected instead of coerced. `serde_json/float_roundtrip`
preserves supported binary64 input values before canonical rendering.

Synthetic stream/request/error fixtures are independent of any private run.
The HTTP-error → success → streamed-error → success test uses core accounting as
a separate accumulator: two 512-token unknown reservations plus two measured
32-token successes give **64 known + 1,024 unknown = 1,088 charged**. No engine
accounting golden changes or adapter retry are involved. Rust integration tests
also exercise native port-zero HTTP(S), exact request paths, proxy isolation in
a child process, redirects, certificate/hostname rejection, frozen explicit
trust, quiet generation beyond connect timeout, dribbling bodies/errors, upload/
header/body cancellation, deadlines, lost replies and server work continuing
after client disconnect. Virtual-time tests independently pin 15/45-second
metadata budgets and setup under a shortened caller deadline.

Existing Python controller tests and accounting oracles remain unchanged.
Intentional stricter Rust behavior: exact supported profile, explicit transport
bounds, no redirects/proxy/retry, strict duplicate/type validation, blank records
rejected, terminal integer usage required, malformed assistant objects separated
from the legacy tool-error classifier, and bounded raw observation queues.
Python's broader implicit numeric/JSON acceptance is not copied.

## Explicit compatibility probe

Metadata only (no GPU inference):

```sh
cargo run --locked -p playtime-ollama --example compatibility -- \
  --endpoint http://oldman-bird.local:11434
```

After verifying that the dedicated backend is available, explicitly add `--live`
for exactly **one** synthetic chat. The source freezes seed 55, cap 256,
context 32,768, temperature 0, think low, keep_alive 5m, a 60-second absolute
deadline, and zero retries. It asks for one inert `emit_result` tool object and
never executes the returned object. Exact settings, request/body hash, raw chunk
hashes, partial message, usage and uncertainty are printed as JSONL; preserve the
output locally. Default tests never contact Ollama. No live cancellation or
backend stop is performed by this example.

Remaining gates: #23 O2 real worker/runner integration and backend-wide ownership,
#24 real Docker plus scripted model and then the two-session live run-and-view,
#20/#21/#22 actual artifact inspection, cancellation/restart/worker-loss recovery.
A successful standalone compatibility probe does not complete these gates.

[types]: https://github.com/ollama/ollama/blob/fce745fe5e4f0c7741ca46fc5484c9471c997d00/api/types.go

For a static worker on a host whose libc resolver cannot resolve `.local`,
`NativeTransport::with_resolution` accepts `Resolution::Pinned` with 1–16 owned
IP addresses for the configured DNS hostname. This is an explicit frozen input,
not a discovered fallback. Host headers, TLS verification name, port and base
path remain those of the configured URL. O2 must capture the chosen addresses in
its frozen inputs and fail on stale addresses rather than silently refreshing
them. The probe accepts `--resolve-ip IP` and prints that configuration. Default
`Resolution::System` uses the system resolver under the same deadline.

The adapter's Rustls feature includes reqwest's default crypto provider (AWS-LC),
so workspace Cargo feature unification also leaves existing native clients able
to construct their HTTP clients. The custom Ollama TLS config still uses only
its owned trust roots. No OpenSSL runtime is linked. A musl probe can be built
with a musl C toolchain explicitly selected:

```sh
CC_x86_64_unknown_linux_musl=musl-gcc cargo build --locked \
  -p playtime-ollama --example compatibility --target x86_64-unknown-linux-musl
```

O2 must record the C compiler/crypto build inputs in worker provenance when this
adapter is actually added to the packaged worker; this PR leaves that package
and its runtime capabilities unchanged.
