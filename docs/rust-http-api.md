# Native HTTP API and shared client

`playtime-server::http` and `playtime-client` supply the authenticated loopback API
used by the daemon, CLI, and workbench. The [daemon application](rust-daemon.md)
connects these handlers to durable admission, packaged workers, and verified
archives. A separate scripted application fixture exercises transport contracts
without executing a controller, worker, model, or Docker command.

## Versioned harness readback

`recipe_harness_selection_v1` exposes `GET /api/v1/harnesses` and
`GET /api/v1/recipes/{recipe_id}/harness` through shared-client `harnesses()` and
`recipe_harness(&definition)`. The built-in catalog is bounded to 16 descriptors;
this build supplies only `memento_turns_v1`, its version, supported setting names,
required model capabilities, and exact workshop image. These are installed support
claims, independent of live model/workshop readiness. Both routes work offline.

Recipe readback is bound to the immutable execution recipe ID and definition
digest. The client checks it against the retained identity and parameters.
Clone/revise requests optionally carry `harness`; omission inherits verbatim and
is omitted from serialized legacy requests. Explicit unknown IDs fail atomically,
without fallback or translating settings. Accepted save replay still precedes
validation. Clients gate new selections on the capability; receipt lookup remains
available without it. Unknown retained IDs stay readable and unsupported, and
cannot execute.

`Preview.harness` projects its exact frozen configuration. `RunView.harness`
contains the projection plus the manifest-bound configuration `ContentRef`, read
through verified archive I/O rather than the current recipe head. Both fields are
optional for old services/configurations. An archive without explicit identity
has no projected harness; readers do not infer it from recipe names or present-day
catalogs. Existing authored archives can retain identity in their saved definition.

## Read-only Ollama model discovery

When `--ollama-endpoint` is configured, the daemon advertises
`model_discovery_v1`. Authenticated `GET /api/v1/models` reads installed tags,
backend version and advisory loaded state; `POST /api/v1/models/inspect` accepts
`{ "name": "exact request name", "expected_digest": "optional observed digest" }`.
The shared client exposes `model_inventory()` and `inspect_model()`. Inspection
is read-only despite POST and requires the normal service-identity header. Neither
route accepts an endpoint override or grants registration/execution authority.

The separate metadata reader calls only version, tags, show and ps. It preserves
exact names, including aliases, and fingerprints the canonical definition using
the existing identity rules. Inspection rechecks tags and backend version after
show. Missing, changed, incomplete, unsupported-version and remote/cloud identities
remain explicit; remote markers in either tags or show are rejected. Only an exact
match for the reviewed execution identity supplies reasoning/context claims;
generated Modelfile blob paths are host observations.
The frozen recipe resolver still validates its complete lock. Other
capability lists are advertised metadata, not inferred support.

Inventory is one bounded snapshot (256 entries, sorted by exact name), not an
unbounded catalog traversal. Each metadata response is capped at 16 MiB, each
operation at 15 seconds, and the service request at 25 seconds in aggregate
(the reusable reader also enforces a 45-second ceiling). At most two service
metadata requests run concurrently; excess requests fail explicitly. Over-limit
inventories fail instead of truncating silently. Loaded-state errors retain the
installed inventory and report unknown with a warning. Observation timestamps and
loaded status are separate from candidate identity. Refresh is explicit, without
background polling or inference, pull, load or unload operations.

Discovery is independent of workshop/model execution readiness. Saved editing and
run observation remain usable when metadata is unavailable. The default HTTP
fixture does not advertise discovery; the focused test opts into an ephemeral
fake Ollama server and exercises the actual reader, routes and client. This is
metadata-contract evidence, not live inference or registration evidence.

## Explicit model registration

The daemon advertises `model_registration_v1` even without a configured Ollama
endpoint, so stored registrations remain readable offline. These authenticated
routes keep registration separate from recipe saving and execution:

| Operation | Route | Shared client |
| --- | --- | --- |
| Review exact installed identity | `POST /api/v1/models/review` | `review_model()` |
| Register reviewed identity with `Idempotency-Key` | `POST /api/v1/models/registrations` | `prepare_model_registration()`, `register_model()` |
| List immutable summaries | `GET /api/v1/models/registrations` | `registered_models()` |
| Read frozen definition | `GET /api/v1/models/registrations/{registration_id}` | `registered_model()` |
| Recover original acceptance | `GET /api/v1/model-registration-requests/{idempotency_key}` | `lookup_model_registration()` |

Review takes the same request as inspection and returns its metadata plus an
eligible acceptance request, bound to this service and configured endpoint.
Register re-inspects before **new** acceptance and compares every identity field.
Changed, missing, incomplete, unsupported-backend or remotely delegated metadata
cannot be accepted. Unknown reasoning/context support or missing tool capability
remains explicit; registration alone does not certify harness compatibility.

An immutable version-1 record retains the opaque registration ID, exact request
name, digest, backend version, canonical definition and fingerprint, advertised
capabilities and reviewed capability evidence/provenance. The canonical definition
contains template, parameters, model_info, details and capabilities. Generated
Modelfile paths, observation timestamps, loaded state and operational endpoint
bindings are not model identity. No service credentials are stored in records.
Exact complete identities deduplicate; aliases retain distinct request names.
A newly reviewed definition or capability change requires explicit acceptance.

The registration and its keyed result commit atomically. Same key/request replays
its historical result without contacting Ollama; a different request conflicts.
New acceptance requires successful live inspection even when its identity already
exists. Client receipts are written privately in `state/model-registrations`
before POST, bound to service and service endpoint, then acknowledged with the
registration ID. Lost responses and acknowledgment failures retain the original
request for lookup/retry. Lookup absence or a failed retry never authorizes a new
key. Refresh, draft discard and failed recipe saves do not delete registrations.

Bounds: 256 immutable records, 4,096 keyed acceptances, 128 MiB aggregate serialized
records/requests, 1 MiB canonical definitions, 2 MiB encoded records and 12 KiB
encoded acceptance requests. The list is one bounded snapshot ordered by creation;
there are no mutable heads or pagination cursors. These bounds keep its summaries
below the default 4 MiB HTTP response limit. New acceptance is serialized with no
waiting queue (`model_registration_busy`); metadata retains the existing read and
time limits. Full capacity reports `model_registry_full`; invalid retained data
reports `model_registry_corrupt`; unavailable metadata or storage I/O reports
`backend_unavailable` (metadata deadlines remain `request_timeout`). Accepted keys
still replay at capacity. There is no automatic garbage collection/deletion UI.

Service ledger version 7 transactionally adds two tables to version 6; existing
run and recipe rows are preserved. Older binaries refuse version 7. This additive
upgrade requires no reset and does not establish a general historical-migration
policy. Registration records are service definitions, not archive writer receipts.
Registration itself does not grant execution; the separate
`recipe_model_execution_v1` capability requires compiled reviewed support.

Focused server/UI tests use ephemeral metadata-only Ollama and HTTP fixtures,
including drift, alias identity, offline replay/restart, capacity, corruption,
concurrent acceptance, disconnected responses and private receipt recovery. They
establish registration behavior, not live inference or worker-recovery evidence.

## Run the transport fixtures

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
Non-loopback listeners are refused.

The demo reveals four canned records at 150 ms intervals. `fixture-example`
includes an unknown event payload, a checkpoint, incoming/outgoing memory,
separate evidence, a saved image, an unretained file, a frozen prompt and unknown
inference usage. `fixture-empty` ends with explicitly incomplete, empty evidence.
The unknown observation includes a synthetic retained JSON ContentRef. Its
opaque artifact appears at the referencing event's frontier and downloads through
the ordinary content route. Payload strings resembling paths remain inert data;
this synthetic wrapper is not the controller-observation schema.
These are transport fixtures, not two-session engine simulations or portable
archives. Their manifests and publication receipts are labeled synthetic.

The example uses an explicit `fixture-service` identity; `--service-id` selects
a different test identity. Its application state is in memory, and each example
invocation starts a fresh script. Do not use it as a persistent service or to
recover receipts from an earlier invocation. Restart tests keep the same scripted
backend outside the stopped HTTP server. They do not test daemon restart with
an active packaged worker; automatic reattachment remains unimplemented.

## Modules and application ownership

| Module | Responsibility |
| --- | --- |
| `playtime-protocol::api` | HTTP DTOs, independent cursor version, OpenAPI schema mirrors of core values |
| `playtime-local` | Shared descriptor-relative credential/discovery file policy; no network or runtime |
| `playtime-server::application` | Application interface for catalog, admission, lifecycle and verified evidence |
| `playtime-server::http` | Axum/Tower authentication boundary, routes, bounds, streaming and schema |
| `playtime-server::daemon` | Production application over lifecycle coordination and verified archive queries |
| `playtime-server::fixture` | Canned replies, test call recording and explicit script advancement |
| `playtime-client` | Service-pinned requests, acknowledged SSE reconnect and verified streaming downloads |

The HTTP module delegates execution and evidence to the application interface;
the server crate's daemon/lifecycle modules own engine/archive/SQLite integration.
The core crate retains its I/O
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
  reservation; it is never synthesized as zero. `orchestration.failure` is the
  worker's own bounded account of an execution that ended without a normal
  outcome, taken from its verified status; it is absent when the worker gave none.
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
**not** `playtime_storage::CommittedFrontier` receipts. The daemon binds archive
reads to verified original worker/manifest/invocation confirmations; see
[confirmation authority](rust-daemon.md#confirmation-authority-and-publication).
Deserializing a worker message or finding a marker does not establish live
publication authority. The runner supplies archive queries and content references,
projected by the daemon; HTTP assigns no experimental meaning to unknown event
kinds or observation fields.

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

Rotate credentials only between runs, because automatic daemon reattachment is
not implemented. Stop the API, replace its credential securely with a new 64-character
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

`playtime-client` also owns shared connection resolution, private durable launch
receipts, cursor files, and explicit regular-file replacement for verified downloads. See
[the native client guide](rust-cli.md) for those APIs and the CLI fixture gate.

## Validation

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

The [normal daemon gate](rust-daemon.md#executable-gate-and-remaining-gates) uses
these same handlers and client with real lifecycle/archive adapters. Docker and
live gates add native effects. The transport fixture's in-memory admission map
does not prove SQLite durability, and its watch notifications do not prove
authenticated worker publication. Restart and worker-loss recovery remain outside
these implemented gates; see [development](development.md).

## Recipe authoring v1

`recipe_authoring_v1` advertises definition reads, cloning, immutable revision
saves, logical head/history listing, and keyed save recovery. The route table and
strict DTO schemas are in the generated OpenAPI document. Saves return 201 on
commit and 200 on replay; `source_conflict`, `head_conflict`, and
`idempotency_conflict` are 409. `catalog_full` is 413. Catalog verification failure
is `catalog_corrupt`; an execution readiness/compatibility limitation is
`execution_unavailable` (503, no preview token). Both use the ordinary authenticated
application boundary; save keys have a separate namespace from launch keys.

The shared native client owns `RecipeSaveStore`, including private durable
pending-save receipts and response/request validation. The workbench performs
these operations outside render callbacks. See
[saved recipe operation and bounds](rust-recipes.md#saved-recipes-and-prompt-editing).

### Model settings authoring

`recipe_model_settings_v1` advertises the identity-bound read endpoint
`GET /recipes/{recipe_id}/model-settings`. It returns typed settings, the inherited
whole-run generation budget, and capability provenance for the locked model.
Reads require no inference or execution readiness. Clone/revise requests may add
`model_settings`; omission retains inherited values. Unknown fields and invalid
bounds are refused, and save also validates first-work context fit using shared
controller rules. Invalid parameters return 422 with an actionable safe message.
The saved definition, receipt replay and archive retain the exact settings; the
new optional fields require no service database migration. See
[model settings editor](rust-recipes.md#model-settings-editor).

### Registered model selection authoring

`recipe_model_selection_v1` adds optional `model_registration` to clone/revise
requests. The value is an immutable registration ID from this service; omission
inherits the source revision's selected model. The service resolves the retained
record inside the save transaction, without live inspection. Missing registrations,
unreviewed capabilities, unsupported tool/reasoning settings and invalid context
return `invalid_parameters` (422) without moving the head or accepting the key.
All saves validate inherited settings too. `RecipeReasoning` now includes distinct
`on` and `off` values; no named level is implicitly converted to either.

Definitions with an explicit selection use schema version 2 and expose a typed
`model_registration` summary. Their inherited frozen definition retains the full
record and original `model_lock` separately. The settings endpoint reports the
selected registration's reviewed capabilities. Client readback and save receipts
validate the selected identity; omission in old request JSON stays omitted.
Existing schema-1 definitions and keys are unchanged, and the ledger stays at 7.

The selection flag certifies authoring only. `recipe_model_execution_v1` separately
advertises verification and execution of compiled reviewed registrations. Preview
includes the selected registration and definition fingerprint in `model_identity`;
the preview revision binds the full worker profile. Unsupported stored evidence,
missing/changed identity and unreachable endpoints return `execution_unavailable`
with reasons `selected_model_unsupported`, `selected_model_missing_or_changed`, or
`model_unavailable`. Accepted launch keys still replay the original admission.
Synthetic HTTP/UI tests establish no native capability or inference support.
See [registered model selection](rust-recipes.md#registered-model-selection).
