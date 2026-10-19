# Native HTTP API and shared client

Tracking: [#35](https://github.com/aortez/playtime/issues/35). Base:
`0d781da14e49bc51aa4fd22e4bd0718c003c23ef`, the merged foundation, controller and
storage. This slice implements HTTP and client behavior against a **scripted
application fixture**. It executes no controller, worker, model, Docker command,
or GPU operation. Production integration remains
[#20](https://github.com/aortez/playtime/issues/20).

## Run the examples

In one terminal, choose a new data directory:

```sh
cargo run --locked -p playtime-server --example api_fixture -- \
  --root /tmp/playtime-api-demo --listen 127.0.0.1:0 --lifetime-seconds 120
```

In another terminal, while the fixture is running:

```sh
cargo run --locked -p playtime-client --example api_walkthrough -- \
  --connection-file /tmp/playtime-api-demo/.playtime/connection.json
```

The walkthrough discovers the service, previews a recipe, saves a launch receipt
before sending it, launches and recovers the same keyed request, inspects paired
history/live anchors, reconnects from its acknowledged position, and downloads
and verifies a small PNG. Its receipt and downloaded file are written beside the
connection descriptor. The server exits at the declared lifetime and closes
observers. Omit `--listen` for `127.0.0.1:8766`; `::1` and port zero are supported.
Non-loopback listeners are refused. Python continues to use port 8765.

The demo reveals four canned records at 150 ms intervals. `fixture-example`
includes an unknown event payload, a checkpoint, incoming/outgoing memory,
separate evidence, a saved image, an unretained file, a frozen prompt and unknown
inference usage. `fixture-empty` ends with explicitly incomplete, empty evidence.
The unknown observation includes a synthetic retained JSON ContentRef. Its
opaque artifact appears at the referencing event's frontier and downloads through
the ordinary content route. Payload strings resembling paths remain inert data;
this synthetic wrapper is not #42's observation schema.
These are transport fixtures, not two-session engine simulations or portable
archives. Their manifests and publication receipts are labeled synthetic.

The example uses an explicit `fixture-service` identity; `--service-id` selects
a different test identity. Its application state is in memory, and each example
invocation starts a fresh script. Do not use it as a persistent service or to
recover receipts from an earlier invocation. Restart tests keep the same scripted
backend outside the stopped HTTP server; #34/#20 own persistent admission and
process restart recovery.

## Modules and application ownership

| Module | Responsibility |
| --- | --- |
| `playtime-protocol::api` | HTTP DTOs, independent cursor version, OpenAPI schema mirrors of core values |
| `playtime-local` | Shared descriptor-relative credential/discovery file policy; no network or runtime |
| `playtime-server::application` | Application interface for catalog, admission, lifecycle and verified evidence |
| `playtime-server::http` | Axum/Tower authentication boundary, routes, bounds, streaming and schema |
| `playtime-server::fixture` | Canned replies, test call recording and explicit script advancement |
| `playtime-client` | Service-pinned requests, acknowledged SSE reconnect and verified streaming downloads |

HTTP has no engine/storage/SQLite dependency. The core crate retains its I/O
independence; protocol has no runtime dependency. Schema-only mirrors describe
core enums and `RunParameters` without adding schema dependencies to core.
The foundation OpenAPI fixture and archive/event schemas are unchanged.

The application adapter must implement these authority boundaries:

- `admit` checks an existing key and typed request before today's recipe revision
  or busy state. A returned acceptance means a saved reservation. Stale previews
  do not consume keys. The adapter owns accepted work beyond the request lifetime.
- `cancel` returns the original saved intent and its current enforcement state.
  A request acknowledgment does not mean that a process stopped. HTTP never
  extends grace or infers an experimental outcome.
- `release` is an explicit operator resolution. The adapter itself verifies the
  worker and resources; a refusal is `release_refused` with `details.remaining`.
  Adapters without it return `unsupported_capability`.
- `run` supplies status separately from evidence and obtains the frontier and
  both anchors as one read. Unknown usage remains `None` or a typed unknown
  reservation; it is never synthesized as zero.
- `history` checks both identity and the exact claimed historical prefix against
  trusted committed evidence. `follow=false` pins that prefix. `follow=true`
  validates the old prefix before advancing to newer writer-confirmed evidence.
  Historical validation cannot require a superseded marker file.
- `subscribe` registers a bounded watch notification before reads begin. A
  notification is a reason to reread evidence, not evidence itself. Only the
  lifecycle owner supplies a reconciled `StreamEnd` when no commits remain.
- `download` resolves an opaque ID through committed/frozen references and
  verifies containment, regular-file/link policy, length and digest before
  returning a reader. It never opens a caller-provided path or URL.

`Frontier` and `EventCursor` are serializable read-position metadata. They are
**not** `playtime_storage::CommittedFrontier` receipts. #20 must bind archive
reads to the verified original worker/manifest/invocation, or an equally explicit
owner-validated port. Deserializing a worker message or finding a marker does
not establish live publication authority. #42 supplies the actual archive/query
projection and content references; HTTP assigns no experimental meaning to
unknown event kinds or observation fields.

## Wire contract

Every route is under `/api/v1`, authenticated, and returns `Cache-Control:
no-store`. Authenticated replies identify the service in
`X-Playtime-Service-ID`; errors include a generated request ID. All POSTs require
the same service header. A missing/malformed identity fails with 400 and a
different identity with 409, before any application call.

| Method and path | Result |
| --- | --- |
| `GET /capabilities`, `GET /openapi.json` | Cached provenance/limits/versions and generated schema |
| `GET /recipes`, `POST /recipes/{recipe_id}/preview` | Opaque catalog IDs and read-only preview for `{mode}` |
| `POST /runs`, `GET /launches/{idempotency_key}` | Keyed launch and recovery lookup |
| `GET /runs`, `GET /runs/{run_id}` | Paged run views and detailed inspection |
| `POST /runs/{run_id}/cancel` | Empty object; original intent and current enforcement |
| `POST /runs/{run_id}/release` | `{"reason"}`; operator release after verification, or `release_refused` |
| `GET /runs/{run_id}/events` | Fixed-frontier event page |
| `GET /runs/{run_id}/events/stream` | Catch-up/follow SSE |
| `GET /runs/{run_id}/checkpoints[/{session_id}]` | Published checkpoints and downloadable content references |
| `GET /runs/{run_id}/artifacts[/{artifact_id}]` | Committed/frozen artifact metadata |
| `GET /runs/{run_id}/artifacts/{artifact_id}/content` | Full verified byte stream |

Launch takes `{recipe_id, mode, preview_revision}` and `Idempotency-Key`.
New acceptance is 202; a matching replay is 200. Cancellation is 202 while work
remains, and 200 when terminal. Keys contain 1–128 ASCII letters, digits,
underscores or hyphens. JSON mutations reject duplicate and unknown fields.
Content encodings are refused; JSON is bounded by actual received bytes,
including chunked requests.

Lists use `cursor` and `limit`, default 200 and maximum 1,000. Run pages sort by
descending admission timestamp/run ID and retain the first page's upper cutoff.
Statuses can change between pages. Checkpoint/artifact pages pin their frontier.
The fixture's list cursor is an implementation detail of its application adapter;
clients treat it as opaque. No offset pagination is exposed.

Event cursors are base64url JSON without padding, at most 2 KiB. Version 1 carries
run ID, manifest SHA-256, after-sequence, and frontier sequence/byte-offset/prefix
SHA-256. Event history returns a resume cursor even at an empty prefix and a next
cursor only when more records remain. Pages stop before their byte cap without
skipping a record. `RunView.event_anchors` gives history after zero and live after
the selected frontier, so clients can index old history and follow new commits
concurrently.

SSE subscribes before its first read, retains notifications during catch-up, and
rereads after every observed change. Each `record` has an EventEnvelope and a
resume-cursor ID. Heartbeat comments run every 15 seconds, including during
subsequent backend reads. `stream_error` and reconciled `stream_end` have no ID.
EOF is a disconnect, never proof of completion. Pending queues are bounded by
both records and bytes. Each backend read takes as many records as the queue can
still hold, because every read rescans the archive prefix. A client that doesn't
drain a full queue within one heartbeat interval is closed rather than blocking
experimental work. A resumed client can receive an applied-but-unacknowledged
record again.

Default limits: 1 MiB request, 4 MiB JSON/page, 1 MiB event JSON plus 8 KiB framing,
64 MiB artifact, eight streams, four transfers, and 64 records/4 MiB pending per
stream. Control operations retain separate capacity. Handler work is bounded to
30 seconds. Accepted mutation tasks retain one of 16 independent permits after a
handler disconnect/timeout; their application owner must bound and supervise the
operation. Streams use a handshake deadline and a 45-second client idle timeout,
without a whole-stream timeout. Downloads have a 30-second transfer deadline.

Downloads send octet-stream, exact Content-Length, Content-Digest (SHA-256),
X-Playtime-SHA256, an attachment filename based on the opaque ID, `nosniff`, and
`Accept-Ranges: none`. A Range header still receives the complete 200 response.
The server checks streamed bytes again; the client verifies them before accepting
a destination. `download_to_path` stages privately and publishes without replacing
an existing file. An abrupt client process death may leave a named `.part` file.

## Credentials and client state

Initialization creates `.playtime` with mode 0700 when needed and one OS-random
256-bit bearer credential in `api-token` with mode 0600. Existing invalid or
unreadable credentials are refused, never replaced. File opens walk held directory
descriptors with `NOFOLLOW`; regular files must have one link, correct ownership,
private modes and bounded size. Writable credential parents are refused. Root-owned
sticky temporary-directory ancestors are allowed, but the actual data/credential
directory must be owned by this user and not writable by others.

`connection.json` is atomically replaced and contains endpoint, service ID and
token-file location, never token bytes. Clients apply the same filesystem checks.
All routes require `Authorization: Bearer`; comparison is constant-time for the
fixed credential. URL credentials and cookies are not authentication mechanisms.
Any Origin is rejected. Host must be localhost, 127.0.0.1 or [::1] with a valid
port, including a different local SSH-forward port. Forwarded headers are ignored.
No permissive CORS headers or unauthenticated discovery endpoint exist.

To rotate: stop the API, replace its credential securely with a new 64-character
lowercase hex token (32 random bytes) in the same owned 0600 file, and restart.
Stopping closes streams; old credentials then fail. Worker lifetime belongs to
the application, not HTTP. Live rotation and remote setup remain #25.

`Client::connect` verifies loopback resolution and pins addresses and service
identity. The reqwest transport has redirects, system proxy, decompression, and
automatic request retries disabled, a five-second connect bound, and explicit
request/idle deadlines. Every response is bounded, including chunked bodies.
Authorization headers are marked sensitive; normal error formatting omits peer
messages, request bodies and reqwest diagnostic URLs.

Create and retain `PendingLaunch` before sending. It includes the service identity,
key and complete typed request. `AmbiguousMutation` means lookup or replay that
same request; no automatic fresh key is issued. A saved request cannot be rebound
to another service even when its endpoint is reused.

`EventStream::next` and `Follow::next` deliver one record pending acknowledgment.
Apply it, durably save its cursor as appropriate, then call `acknowledge`.
Reading/parsing alone does not advance the saved cursor. Deduplicate by run and
sequence if application and cursor persistence can be interrupted. Records must
preserve run/manifest identity and advance prefix sequence/bytes monotonically;
an unchanged prefix sequence must retain exactly the same offset and digest.
A terminal notice must be structurally valid and match the entire acknowledged
frontier after all its records have been applied. Contradictions return a
non-retryable `InvalidResponse` without advancing acknowledgment or reporting
completion. These checks compare metadata; verifying actual archived bytes
remains the backend's responsibility. Follow has a
caller-selected reconnect count, consecutive-outage budget and bounded backoff.
Valid SSE activity resets that budget; stalled reconnect bodies cannot extend it. Auth,
version, corruption and parser failures are surfaced; they do not trigger an
endless retry. Cancelling observation only detaches and never calls run cancellation.

#21 adds shared connection resolution, private durable launch receipts, cursor
files, and explicit regular-file replacement for verified downloads. See
[the native client guide](rust-cli.md) for those APIs and the CLI fixture gate.

## Validation and integration gate

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 -m unittest discover -s tests -v
```

Tests use dynamically assigned loopback ports and separate temporary roots.
They exercise actual HTTP requests, not only router unit calls: identity rebinding,
lost replies/timeouts, matching/conflicting launch keys, stale previews, cancelled
observation, service restart, fixed pages, paired anchors, commits on both sides
of subscribe/read, partial frames, unacknowledged redelivery, slow observers,
unknown/empty records, credentials, Host/Origin, byte limits, redirects, malformed
errors, stalled replies and failed downloads. The client is also tested against
independently scripted hostile HTTP peers. Real replies are validated against the
generated OpenAPI schemas.

Reviewed goldens live under `crates/playtime-server/tests/fixtures/`; they are
independent of the foundation goldens. `openapi` and `contract_fixture` examples
print candidate replacements for deliberate review; tests never regenerate them.

#20 must reuse these handlers and this client with real #34 lifecycle and #42
archive adapters, and rerun the public conformance/failure cases against those
adapters. In particular, the fixture's in-memory admission map does not prove
SQLite durability, and its watch notifications do not prove authenticated worker
publication. #43 supplies exclusive recovery after verified worker loss. #21/#22
can build against this fixture, while their real-integration completion gates remain.
