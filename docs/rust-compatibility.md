# Rust compatibility and goldens

The owner retired the legacy compatibility matrix on 2026-10-18 (#16). The
Python controller is frozen and is no longer a parity oracle. Rust doesn't read
or repeat Python archives, and legacy and Python fixtures no longer constrain
Rust behavior. The retired matrix and its behavioral evidence table are in this
file's Git history.

The committed Rust goldens are the spec. When Rust behavior changes on purpose,
update the affected goldens and add a short reviewed note to this file. Never
regenerate goldens just to make a test pass.

The Python suite stays in CI for now, and some of its tests still read Rust
files: `crates/playtime-engine/src/tools.json`, the engine's `canonical.json` and
`evidence.json` fixtures, the protocol's `legacy/` and `two-session/` fixtures,
and the Docker capture helpers and their reference tar. Python is frozen, so a
deliberate Rust change that would break one of those tests adds a new versioned
file instead of editing the shared one.

## Session-neutral internal contract (#109 step C, 2026-10-25)

Adds explicit internal work/handoff/compaction purposes, run/session scopes and
common allocations without memento settings. Existing memento request serialization
retains its `handoff` boolean; behavioral traces, recipe/schema-1/2 bytes, tool
schemas, model/workshop locks, budgets and service ledger are unchanged.

The feature-gated CPU workspace probe uses input-manifest v2, `execution_v2`
payloads inside the existing event envelope, and a distinct final capture index.
Strict v1 decoders remain available. Current readers explicitly support the new
manifest/record versions and retain original JSONL; old readers and production
worker acquire routes reject the test contract. The probe binds its actual test
executable and shares the controller, dispatch barriers, archive loop and writer
receipts. Native process continuity and actual compaction remain #110.

New protocol, archive-driver and failure tests plus the separate
`workspace-execution-v2.json` golden cover the new contract. Purpose projections
keep unknown usage and unclassified legacy evidence explicit. No old goldens,
Python controller/tests, database migrations or recovery semantics are changed.

## Memento policy extraction (#109 step B, 2026-10-25)

The engine now separates private memento conversation/progression proposals from
runtime admission and effect authority. Frozen memento capabilities determine
allowances, reserves, tool schemas, and retry/turn bounds. The runtime computes
context fit from the actual proposed input and verified prior measurements,
retains all accounting and deadlines, and requires a publication receipt before
accepting a successful finish.

This is an internal extraction: no definition, API, event, observation, database,
worker-profile, or archive schema changes. Existing behavioral goldens and tool
definitions remain unchanged; worker source/build identities change naturally.
Simulator and native worker still use the same controller. Session-neutral
purposes and record contracts remain #109 step C, with no compaction harness,
context-triggered memento, new call cap, or change to recipe limits in this slice.

## Harness identity and readback (#109 step A, 2026-10-25)

Adds `recipe_harness_selection_v1`, a bounded offline built-in catalog, typed
definition readback, and optional clone/revise selection. Omission still inherits
and serializes identically for old save requests; explicit IDs are exact and
unsupported choices fail without mutation. Existing schema-1/2 memento definition
bytes/digests remain unchanged. Unknown retained IDs can be read and edited,
including registered-model definitions; execution still rejects them.

New Docker/native profiles explicitly freeze `memento_turns_v1`. Their compiled
profile digest changes, so a new worker bundle is required. Preview and worker
verification cover the selection and all existing parameters. This does not
change memento policy, allocations, locks, behavioral traces, or the service
ledger. Old archives are read without rewriting; authored profiles can supply
their retained definition identity, while configurations without an ID remain
unrecorded. Run readback verifies its manifest-bound configuration before projection.

The HTTP OpenAPI golden adds two routes, six harness DTOs and optional selection,
preview and archived-run fields. Existing HTTP fixture responses remain unchanged.
Four new pinned screenshots cover editor, narrow layout, preview and archived
inspection; existing screenshots remain unchanged. No frozen Python changes.
Step A did not extract policy or introduce session-neutral records; those are
separate #109 slices.

## Registered-model execution (#134, 2026-10-25)

Adds `recipe_model_execution_v1` for compiled, reviewed identity profiles. Selected
schema-2 definitions now project their registration into the worker's model lock,
settings and archived configuration; base lock provenance is unchanged. The
supported identities are the existing exact GPT-OSS lock and the new Qwen3 1.7B
lock on Ollama 0.32.3. Qwen supports explicit boolean On/Off; its default stays
unknown. Existing locks, recipe budgets, schema-1 definition bytes and ledger
version 7 are unchanged. No API schema changes or old golden rewrites are needed.

The compiled Docker profile digest includes selected-model execution and the Qwen
lock. Rebuild worker bundles; older bundles cannot execute through this daemon.
Preview/admission and selected worker startup/session/each inference dispatch
recheck identity. Model availability is checked for the requested recipe rather
than requiring GPT-OSS during endpoint setup. Transport failures and metadata
drift never fall back to the base model. The controller still conservatively
charges an unknown allowance once it has dispatched a native effect, including
an adapter metadata refusal before inference. This does not relax uncertainty
accounting or claim atomic mutable-tag verification.

An execution metadata `/api/show` HTTP 404 is an identity failure, including
removal between tags and show. Preview/admission therefore report
`selected_model_missing_or_changed` rather than an endpoint outage. Other
metadata HTTP failures remain availability errors; no inference is dispatched
after either refusal. The HTTP metadata fixture now returns 404 for absent models.

Local CPU Qwen probes establish boolean thinking and native tool-call support.
The separate registered-model gate exercises API/workbench launches with real
systemd/Docker and retains both outcomes. It does not replace the existing
GPT-OSS acceptance gate; full two-model acceptance keeps #134 open while that
host is unavailable. See [local development setup](local-ollama-development.md).

The run-and-view worker-loss fixture's teardown accepts an already absent agent
only with the same exact absence proof it requires after removal. A history read
can arrive after checkpoint capture removed that agent. Transport ambiguity still
fails closed; existing resources still require exact proposal checks, and test
cleanup never releases the retained service admission.

## Registered-model recipe authoring (#134, first slice, 2026-10-24)

Adds `recipe_model_selection_v1`, an optional registration ID on clone/revise
requests, a typed selected summary on definitions, and explicit On/Off reasoning.
The reviewed OpenAPI golden includes those additive fields and enum variants.
Selected frozen definitions use schema version 2 and retain their immutable
registration separately from base model-lock provenance. Old schema-1 definitions
and omitted request fields keep their original bytes/digests; ledger version 7
is unchanged. Older binaries reject selected schema-2 definitions.

Every new save validates selected capabilities and inherited settings/context.
No second native profile is approved; boolean-profile tests are synthetic. The
worker rejects selected registrations rather than using the base lock. Native
execution verification and the two-model acceptance required by #134 remain
incomplete. Four new pinned-renderer screenshots cover model selection, preserved
unsupported reasoning and explicit corrections; existing baselines are unchanged.

## Read-only model discovery (#131, 2026-10-24)

Adds opt-in `model_discovery_v1`, two authenticated metadata routes and bounded
inventory/inspection DTOs. The OpenAPI golden adds these routes, schemas and metadata error codes;
existing routes, fixtures, recipe revisions and launch identities are unchanged.
The workbench adds reviewed wide/narrow, missing-model and stale-metadata browser
screenshots. Discovery does not register models or expand execution support; the
shared reviewed capability resolver still requires the exact existing lock.

Review fixes match discovery capabilities by the exact execution identity fields
(request name, digest, backend version and canonical definition fingerprint), so
a host-specific path in the generated Modelfile cannot hide known support. The
frozen recipe resolver keeps its complete-lock comparison. Inspection that
contradicts cached inventory now marks it stale until an explicit inventory
refresh; the removed-model screenshot records this behavior. Wire schemas and
execution identities are unchanged.

## Contract notes

Rust references use opaque IDs rather than exposing path semantics. HTTP, event
and input-manifest versions are independent. New manifests hash exact stored
input bytes and hash the manifest's own stored bytes separately. Worker
executable identity is independent of service/viewer builds; archived source and
dependencies alone do not establish executable identity or guarantee a
bit-identical rebuild.

Limit DTOs name explicit byte units, and `cpu_cores` retains fractional CPU
values without rounding. The daemon freezes model/recipe fields and preview
revisions as described in [recipes](rust-recipes.md). Interactive edits create
immutable saved revisions; launch never applies an unsaved override.

## Rust CLI command map

`target/debug/playtime` (or `cargo run --locked -p playtime-cli -- …`) is the Rust
API client. `./playtime` is the frozen Python launcher. The
CLI works with the real daemon. Its fixture gate and the normal service gate
are implemented; the dedicated CLI/UI simulation acceptance gates still fail
pending full restart/recovery integration. See [the native client guide](rust-cli.md)
and [development](development.md).

| Python behavior | Rust CLI behavior |
| --- | --- |
| `run RECIPE_PATH` freezes and owns foreground execution | `run RECIPE_ID` uses a catalog ID, saves a keyed receipt, submits to the service, and detaches after acceptance by default; `--wait`/`--follow` observes without owning execution |
| Model adapter `--endpoint` is an Ollama endpoint | Rust `--endpoint` is the Playtime HTTP service, paired with `--token-file`; no model connection occurs in the client |
| Browser control and worker status through `serve` | `service status`, catalog/preview, runs, events, checkpoints, and artifacts use the authenticated versioned API served by `playtimed` |
| Stop the owned foreground experiment | SIGINT/SIGTERM only detach the Rust observer; use `runs cancel ID [--wait]` to request service cancellation |
| Inspect local run folders/reports | Rust inspects opaque run/session/artifact IDs through the API and verifies explicit downloads; it never opens the service's run folders |
| `prepare`, `check`, `smoke` | Reserved, exit 6 with #23/#24 ownership; simulation does not prove real Docker smoke |
| `report` | Reserved, exit 6; #26 |
| `repeat`, `pilot-prepare`, `pilot-run` | Reserved, exit 6; #27 |
| `serve`, internal `worker` | Reserved in the client, exit 6; use the separate `playtimed`/packaged worker |

The shared resolver, durable launch-receipt format, cursor publication, and
verified-download helper live in `playtime-client` for both CLI and workbench. CLI
parsing/presentation stay in `playtime-cli`. A CLI/client failure and an
experimental outcome have distinct exit classifications; unknown usage is never
converted to zero. No hidden Python fallback is installed.

## Reviewed behavior changes

These notes describe changes at their introduction. Later entries can supersede
earlier capability sets or limitations; current operation is described in the
linked component guides. The historical notes and reviewed golden decisions are
retained rather than rewritten to describe today's implementation.

### Native workspace capture v1

D2's PAX exporter deliberately normalizes uid/gid to 1000, owner/group names to
empty, and mtime to zero while preserving permission bits. It captures links as
inert metadata, reads sparse filesystem files densely under logical-byte limits,
and rejects unsupported/unreadable entries. Its outgoing-memory helper refuses
symlinks and reports explicit missing/unreadable/oversized/binary states. These
bytes are snapshot candidates, never continuation/extraction authorization. See
[rust-workspace-capture.md](rust-workspace-capture.md) for the strict subset,
capacity preparation and independent reference fixture. Earlier archives remain
inspectable. Worker build records add an optional `capture_helpers` digest map;
executing bundles verify it against embedded literals. That slice advertised no
real-workshop execution capability; the Docker composition below adds one.

### Docker/scripted-model composition v1

The optional daemon catalog now supports `docker-two-session` in `scripted_smoke`
mode using real Docker and a packaged systemd worker. See
[run-and-view](rust-docker-run-and-view.md). No Python recipe, prompt, workshop
source or lock is modified. The native tool policy distinguishes a confirmed
bridge timeout from transport uncertainty; only the former can continue work.
Its five-second effect cap includes bounded transport/return time, while a model's
shorter requested timeout bounds the command inside the bridge. Simulation timer
behavior and historical oracle/golden bytes are unchanged. This frozen scripted
profile uses a 32K conservative context budget and does not claim compatibility
with the later 8K live profile. Interrupted runs have no automatic recovery or
admission release in this slice.

Every new bundle, including one used only for simulation, now advertises
`real_workshop` and an optional `docker_profile` capability: the digest of its
compiled profile's version, recipe, image lock, helpers and limits. Host Docker
selection and daemon identity are excluded. A `--docker-scripted` daemon refuses
to start when that digest differs from its own, and the Docker preview revision
includes it. Bundles without the field still decode and run simulation, but
cannot prepare or execute Docker runs.

### Per-session tool-call parse ceiling

Since 2026-10-18 (#73), `tool_parse_error_limit` counts parse errors per session
instead of per run. Reaching it ends only that session, through the normal
handoff (`early_handoff` with reason `tool_parse_error_limit`), and the next
session counts from zero. More than `tool_parse_retries` consecutive errors still
stop the run, and every failed request still charges its full allowance as
unknown usage. No golden files changed, and older runs that stopped with
`tool_parse_error_limit` still decode. See
[the controller notes](rust-controller.md).

### Live model profile v1

New bundles also advertise `real_model`. `docker_profile` now digests both
compiled profile templates, scripted and live, so a bundle from before this
change can't run either through a new daemon. `implements_docker` no longer
requires the absence of `real_model`; live runs also need it. The worker kind
`docker_live` and run mode `live_smoke` are new. Scripted Docker archives are
unchanged. See [live model](rust-live-model.md).

Reviewed golden change: the HTTP `openapi.json` gains a `Contention` schema and an
optional `contention` property on `RunView`, its only difference. A run without
live timing omits the field, so recorded responses and the other goldens are
unchanged. The flags come from the live worker's per-request timing chunks,
which use the existing `model_chunk` observation.

### Recipe profile v1

Catalog recipes freeze as profile `docker_live_recipe_v1`, run mode `experiment`
and worker kind `docker_live`. The profile carries the recipe, including its
seeds, in a new optional `recipe` field. Older workers refuse that field.
`docker_profile` now also digests a reference recipe's frozen template and the
recipe bounds. A bundle from before this change can't run any Docker profile
through a new daemon. The scripted and live smoke profiles are unchanged. The
HTTP goldens are unchanged: catalog recipes use the existing recipe, preview and
seed summary types. See [recipes](rust-recipes.md).

### Operator release

Since 2026-10-18 (#71), `playtime runs release RUN_ID --reason TEXT` (`POST
/api/v1/runs/{run_id}/release`) ends a run that isn't terminal once the daemon
has verified that its original worker invocation is inactive with an empty
cgroup and, for Docker runs, that every resource in the worker's ownership
journal is absent. Otherwise it refuses with `release_refused` (409) and lists
what remains; there is no force option. A release records its time, reason and
evidence in the ledger (schema version 5) and marks the run terminal with reason
`operator_released`. The archive is unchanged, and the event stream of a released
run ends as incomplete. The OpenAPI golden gains the route, its request and
response types, and `release_refused`; the daemon advertises the
`operator_release` feature.

### Command return allowance

Since 2026-10-18 (#78 item 3), the Docker profiles reserve 3 s instead of 1 s
inside each command's effect cap for the Docker calls around `docker exec` and
the bridge's kill and reap. A command that runs to its bridge cap then ends with
a confirmed timeout instead of an infrastructure failure. A tool call with less
than the allowance left before the work deadline no longer reaches Docker, and
the run ends with `wall_limit` instead of `infrastructure_failure`. Recipes now
need a `command_seconds` above 3. The frozen `command_return_ms` field is 3000, so
`docker_profile` digests change and older bundles can't run Docker profiles
through a new daemon. The goldens are unchanged.

### Reconciling during normal runs

Since 2026-10-18 (#93), a normal Docker or live run no longer passes through
`reconciling`. Until its startup deadline, a worker whose unit is up but that
hasn't answered yet keeps its phase instead of reporting `worker_unavailable`.
Once the daemon confirms normal completion and stops the unit itself, the run
stays `running` until it is terminal instead of briefly reporting
`worker_unavailable`. Both had made `run --wait` and `runs wait` return exit 9 on
normal runs. Cancels, failures, lost workers and daemon restarts still end in
`reconciling` and need a release. The API's values and the goldens are unchanged.

### Workbench refusals and run status

Since 2026-10-18 (#97), the workbench removes the receipt of a key it just
generated when the service refuses that first request with `run_busy` or
`stale_preview`, through the new `ReceiptStore::discard`. Lookups and retries
keep their receipts. The run header names the orchestration reason and prints
the controller phase without `Option` debug text, so five reviewed workbench
screenshots change from "controller None" to "controller unavailable". The HTTP
and protocol goldens are unchanged.

### Readable timeline

Since 2026-10-19 (#92, part 1), Compact and Readable list each step once. The six
archive records that repeat the observations are left out, and session starts,
checkpoints and the run's end are one-line markers. Full still lists every
record. Workbench window requests now name the view and can carry a row to
locate. The reviewed screenshots, the HTTP and protocol goldens and the archive
format are unchanged.

### Run header

Since 2026-10-19 (#92, part 2), the run header leads with progress against the
frozen limits, read from the observations as they are indexed, and an ended run
leads with its outcome and the sessions it completed. The workbench shows states
in words: the run list says "Ended" for a terminal run, and the launcher's
preview names the mode and memory condition. Six reviewed screenshots change,
and two new ones show a running and an ended run. The HTTP and protocol goldens
and the archive format are unchanged.

### Workbench notices and run list

Since 2026-10-20 (#104), a workbench status notice about a run clears when
another run or the launcher opens, and reconnecting from the launcher no longer
watches the run it left. A download's result is no longer tagged with the open
run, so it still arrives, and stays, after another run opens. A failed download
now names its destination. An accepted launch refreshes the run list at once
instead of at the next 5-second poll. The reviewed screenshots, the HTTP and
protocol goldens and the archive format are unchanged.

### Immutable recipe authoring v1

The `recipe_authoring_v1` capability adds strict definition, clone, revision,
head/history, and save-lookup DTOs and routes. `Recipe.saved` is optional and omitted
for existing sources, preserving old HTTP fixture responses. The reviewed OpenAPI
golden adds these routes and schemas; launch DTOs and baseline controller goldens
are unchanged. Service ledger version 6 transactionally adds retained recipe
heads, revisions, and keyed save results; older binaries refuse the upgraded ledger.

Saved revisions carry their own input bytes and execution identities. Definition
reads and saves no longer require a ready live adapter. Unavailable execution
returns a typed preview/admission limitation; corruption remains an error. Picker
cursors now distinguish stable logical ordering from immutable execution handles.

Authored Docker profiles embed a versioned definition that must agree with the
executing recipe/scenario. The compiled profile digest now includes this capability,
requiring a rebuilt worker bundle. Existing profiles omit the new optional field.
See [saved recipes](rust-recipes.md#saved-recipes-and-prompt-editing) for bounds,
recovery, and the native acceptance evidence.

## Model settings authoring (#5)

The HTTP schema adds `GET /recipes/{recipe_id}/model-settings`, its typed response,
and optional complete `model_settings` objects on clone/revise requests. Omitted
settings serialize exactly as earlier requests, preserving stored idempotency
keys and receipts. Frozen recipes add optional capability evidence only for new
settings saves; old definition digests are unchanged. The ledger remains version
6, without a migration/reset. This is additive compatibility for this slice,
not a general development retention commitment.

Recipe reasoning additionally supports the explicit `default` choice, projected
to an omitted Ollama `think` field. Existing explicit levels and unchanged recipes
retain their wire settings. The worker profile digest includes settings-authoring
support; rebuild bundles. Save/preview now refuse initial context configurations
that the controller would immediately hand off. The controller's prompt and
reserve calculations are shared without changing its behavior or goldens.

Initial context validation also applies to prompt-only saves that inherit model
settings. Rejected saves publish neither a revision nor an idempotency result;
existing keyed saves still replay their original response without revalidation.
Recovering a save preserves a current settings draft with unfinished or invalid
numeric input instead of attempting an infallible request conversion.

Reviewed OpenAPI changes are the new route/DTOs and the two optional request
fields. New settings-editor screenshots cover wide/narrow layouts; existing
prompt-only editor baselines remain as older-service compatibility views.

### Live smoke command grouping

The `live-two-session` smoke prompt now identifies each numbered shell block as
one `exec` call, distinguishes the current session from the alternative session's
instructions, and combines session-2 verification and the conditional result-file
write in one Python block. Assertions must pass before the result is written.
Limits, sampling settings, controller behavior, and success assertions are
unchanged. The compiled profile digest changes with the prompt; rebuild the
worker bundle. Existing run archives retain the earlier prompt as evidence.

Process-gate failures now include the actual outcome, accounting, work-call and
forced-handoff counts by session, and tool-request sequences mentioning the
result file. A mention does not prove a successful write; referenced observations
that were not inspected are counted explicitly. No schema or behavioral goldens
change.

The gate's fresh-conversation check selects session 2's first model request by
archived session identity instead of searching prompt text for `Session 2/2.`.
Prompts may describe both sessions; the first session's text is not evidence of
which session executed a request. The check still rejects extra conversation
history and a missing or mismatched delivered memento.


## Readable call traces (#124)

The Rust workbench now groups calls and their tools using recorded request/session
identities, with typed tool summaries and content-sized virtualized blocks. Long
text uses explicit excerpts; missing results and partial inference remain visible.
Raw records and retained content remain available through source inspection; Full
keeps its source-oriented presentation. No protocol, archive, controller or service
ledger version changes. Existing rendering baselines change deliberately for the
trace layout, with new wide/narrow failure-handoff, streaming and long-text
fixtures. Session navigation and contextual published-memory summaries remain #125.

## Session navigation and memory handoffs (#125)

The workbench now indexes recorded session boundaries independently of payload
eviction and offers a paged chooser. Paused reconnect navigation uses stable event
sequences. Readable coalesces the canonical session-start repeat when an exact
controller delivery observation is present; Full and source inspection retain both.
Delivered-memory excerpts and verified published-checkpoint excerpts stay distinct
from submitted tool text. Inline controls select the corresponding checkpoint in
the existing inspector, with late selection/inspection replies rejected.

File-change summaries no longer treat absent hashes as evidence of unchanged files
or classify changes across incomplete checkpoint lists. Reviewed UI screenshots
add wide/narrow memory and interrupted-handoff scenes and update the running/ended
trace views. No API, archive, controller, service-ledger or recipe schema changes.

PR #127 review fixes: navigation acknowledgments retain sequence/row identity,
not an absolute position across history updates. The UI resolves that row in
the current window or requests a fresh location without completing the jump.
Canonical events retain their explicit session IDs, so an s2 checkpoint stays
unresolved until s2's boundary is indexed rather than inheriting s1's ordinal.
Regression coverage includes backfill between reply application and rendering,
a target displaced out of the window, and a coalesced canonical session start.


## Readable launcher and file inspection (#129)

Frozen previews now have prepared sections with explicit unavailable metadata,
named seed sizes, wrapped protocol/inventory and paged full-source disclosure.
Run rows show recipe and UTC admission time; connection descriptions and per-file
identities move to Details. Fixture/scripted provenance remains explicit.
Observation reads treat `evidence_not_ready` as bounded archive waiting while
transport, identity and authorization failures retain their distinct behavior.
These changes affect only UI presentation/read retries, with no API, archive,
controller, service ledger or frozen Python changes. Rendering baselines change
intentionally for the connection bar/run list and launcher/files; new wide/narrow
live-profile previews, expanded text, file drawer and archive-wait views cover the
new states with synthetic inputs on the pinned software Vulkan renderer.

PR #130 review correction: run state now occupies its own untruncated line beneath
the recipe button. Long recipe names cannot hide `needs release`; fixed run-row
height includes the extra line. Regression and pinned-renderer coverage exercise
default and minimum sidebar widths with running, ended and release-needed runs.


## Explicit Ollama identity registration (#133)

Adds opt-in `model_registration_v1` review/register/list/read/keyed-lookup routes,
immutable version-1 identity DTOs and private shared-client receipts. The HTTP
OpenAPI golden deliberately adds these schemas/routes and full/corrupt/busy errors.
Existing discovery DTOs, recipe locks, worker verification, archives, controller
behavior and frozen Python remain unchanged. New acceptance re-inspects complete
identity; accepted-key replay returns retained evidence without live metadata.

Ledger version 7 adds registrations and keyed acceptances transactionally to
version 6, preserving existing run/recipe rows; older binaries refuse it. No reset
is required, and this does not decide a general development migration policy.
New pinned screenshots cover wide/narrow registration, offline readback and receipt
recovery. Existing discovery-only and workbench baselines remain unchanged.
