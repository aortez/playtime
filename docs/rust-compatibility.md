# Rust compatibility and goldens

## Saved compacting chat turns (#208, 2026-11-13)

The new `Controller::chat` entry point ends a turn on an explicitly completed,
nonempty plain reply, then cleans up without experiment capture/publication.
`playtime-harness` persists private history and completed-turn checkpoints using
version-1 types in `playtime_protocol::conversation`. Saved state binds the
model/workspace/configuration and continues observation/request numbering. Resume
accepts only complete exchanges and successful cleanup; active or uncertain turns
remain fenced. The new mode is not registered as an experiment recipe or worker
profile. Existing memento/continuous records, snapshots and behavioral goldens are
unchanged. See [the conversation contract](rust-conversation.md).

## Shared compaction context planner (#208, 2026-11-13)

The continuous policy now uses `playtime_engine::conversation::ContextPolicy`
for context-fit decisions, summary scheduling and replacement checks. The same
component accepts a caller-owned transcript and common limits without workshop
or archive configuration. Runtime admission, tool dispatch, failure history,
summary validation and publication remain in the existing controller/policy.
The compaction records and their golden remain unchanged; this extraction adds
no wire format, saved-session format or new execution mode.

## Release refusal reasons (#204, 2026-11-10)

A refused release used to reach clients with empty details: the daemon put
readable sentences in `details.remaining`, and the HTTP error filter, which
keeps only identifiers, dropped them. Each entry is now a record with a stable
`code`, such as `resource_present`, and for a Docker resource its `kind` and
`name`; a refusal lists at most 32, and `details.omitted` counts the rest. The
sentences, with any error text, go to the daemon's log instead. To carry these,
the filter now also keeps records of up to 8 identifier keys, within its
existing depth and size bounds; strings still pass only as identifiers. The CLI
prints each entry's code and resource. The refusal's message no longer says the
run still owns something, which wasn't true of every code; it now reads "The
release was refused; see details". A daemon that starts without Docker, the live
model or an inference target's model now says why on stderr, and a Docker run's
release that it can't verify says `docker_unavailable`. The ledger, worker
control, API schemas and goldens are unchanged.

## Cancel on a run with no worker left (2026-11-10)

A cancel used to take a run out of waiting for a release when no worker for it
could still run: none was started and preparation had ended, the supervisor had
seen it stop, or it started on an earlier boot. The cancel couldn't reach a
worker, so it left the run cancelling, with reason `cancel_requested` or
`control_uncertain`, which clients don't show as waiting for a release. Cancel
now returns such a run unchanged, without recording an intent or asking the
supervisor, and a plain `runs cancel` result adds the release command as
`next`, like the observers. A boot ID that can't be read never counts as
evidence, so it can't hold back a stop. When the supervisor sees the worker
stop while a cancel is still trying to reach it, the run now stays
`worker_unavailable`; the cancel used to replace that with `cancel_requested`
or `control_uncertain`.

Clients now also count a run whose preparation failed (`preparation_failed`) as
waiting for a release. The CLI's observers exit 9 with the release command
instead of waiting until their deadline, and the workbench shows the command
and turns Cancel off. When a cancel had no worker left to cancel, the workbench
now says so instead of reporting an acknowledged intent. The ledger, worker
control, API schemas and goldens are unchanged.

## Release after a host reboot (#43, 2026-11-10)

Operator release used to refuse a run whose worker started on an earlier boot,
because the supervisor can't observe a unit from an earlier boot. No process
outlives its boot, so release now takes the changed boot ID as the evidence that
the original invocation stopped. It doesn't ask the supervisor then, and the
release record gains an optional `later_boot_id` holding the current boot ID,
with `supervisor` left empty. Every resource in the worker's Docker ownership
journal must still be absent. A boot ID names a boot of one host, so this
assumes the root stays on the host that ran the worker. Older binaries ignore
the new field, and ledger records without it read as before, so the ledger
version stays 7. The `Clock` port gains `boot_id`, which reads the kernel's by
default. Worker control, API schemas and goldens are unchanged.

## Daemon restart reattachment (#59, 2026-11-10)

A restarted daemon now follows the active run whose worker start was submitted
on this boot, with the refresh a launch starts, instead of leaving it
reconciling. It does so before revalidating the rest of the history. Unless the
last observation saw the worker stop, the run's reason changes from
`coordinator_restarted` to the existing `control_uncertain`, which clients read
as control being resolved rather than a run waiting for a release; the ledger
trace gains a `reattaching` transition. The run is current again only once its
original invocation authenticates through the supervisor and the private control
socket and its receipt extends the saved confirmation. A saved cancellation is
delivered again, and an accepted stop is only observed. The daemon never
prepares or starts another worker for the run. The CPU test supervisor adopts a
worker an earlier daemon started on this boot, through a pidfd and after
checking its saved PID and kernel start time. The ledger schema, worker control
protocol, API schemas and goldens are unchanged. The service wrapper gains
`--case lifecycle`, which CI runs, and an opt-in `--case lifecycle --backend
systemd`. `go.sh` still refuses to rebuild the daemon during a run.

## Bounded tool-batch recovery and README assessments (2026-11-06)

Continuous work now rejects an oversized tool batch without dispatching any
call, records `model_error` kind `tool_batch_limit`, and supplies a notice naming
the allowed count. The existing eight-call, aggregate-output and five-retry
bounds remain unchanged. Batch and parse failures share a consecutive streak;
the sixth batch failure retains `too_many_tools_in_one_response`. Generation
settles once as known usage, and `failure_history_v3` can retain the additive
`model_tool_batch_error` record kind. Mixed/invalid finish declarations, server
overruns and memento behavior retain their previous stop semantics.
`oversized-tool-batch-v1.json` pins the new notice, failure kind and accounting.

Coding assessment revision 3 requires a regular README file for planner and
byte-range tasks; ledger already required one. Revision 2 catalog/checker bytes
and historical scores remain unchanged, and `--assessment-revision 2` selects
the old rules explicitly. New assessments default to revision 3 and record the
selected checker/catalog and current helper hashes.

## Framework Q8 coding models and recipes (2026-11-06)

Two additional compiled registrations bind the exact 128K coding aliases for
Ornith 1.5 35B Q8 and Qwen3.6 35B Q8 to Ollama 0.40.14. Native probes passed
with omitted, On and Off thinking, tool-result receipts, and cancellation at
131,072 context. Their locks enter the worker capability digest, so daemon and
worker must be rebuilt together. The three coding-repair recipe sources use the
owner-approved 900-second task budget; existing recipe limits and controller
bounds are unchanged. See the [study record](../experiments/framework-q8-repeatability.md).

## BF16 Qwen identities (2026-11-03)

The native adapter explicitly supports Ollama 0.40.14 alongside the existing
exact versions. New Qwen3.8-27B BF16 and Qwen3.6-35B-A3B BF16 locks and compiled
capabilities bind that version to their manifest and canonical definition
digests. Model-default omission and boolean On/Off passed native streaming,
tool-result and cancellation probes at 98,304 context on Framework. Existing
locks still require their original backend versions; no version range or
implicit upgrade is accepted. The new locks enter the worker capability digest,
requiring matching daemon and worker builds. No archive or API schema changes.
The standalone review example can now request an explicit context allocation.
See [the retained hardware evidence](rust-framework-models.md#bf16-qwen-candidates-2026-11-03).

## Model context defaults (2026-11-03)

Choosing a different registered model now defaults context capacity to the exact
identity's reviewed window, capped at the existing 131,072-token recipe maximum.
The workbench shows the resulting editable value. API saves without explicit
model settings use the same default; explicit settings, edits retaining the same
registration, saved-receipt replay and existing revisions preserve their frozen
capacity. Reasoning, sampling, response/handoff allowances and whole-run budgets
are unchanged. Unknown capabilities still fail validation. No model locks,
controller policies, archive formats or wire fields change. The OpenAPI golden's
two selection descriptions and the recipe model screenshots intentionally reflect
the new default and explanatory text.

2026-10-27: The UI displays model identity and settings from the verified archived
configuration separately from inherited recipe titles. New `archived-model`
wide/narrow screenshots cover the distinction. This adds a bounded background
artifact read when selecting a run; wire schemas and archived inputs are unchanged.

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

## Ornith 35B GPU identity (2026-11-04)

The reviewed catalog adds `ornith-1.5-playtime:35b-gpu` on Ollama 0.32.3,
derived from Ornith 1.5 35B Q4_K_M with `num_gpu=99` in the locked model
parameters. Boolean On/Off and model-default omission are supported; GPT-OSS
reasoning levels remain unsupported for this identity. The lock enters the
Docker capability digest, requiring matching rebuilt daemon and worker bundles.
No request/API schema, controller policy, existing model lock or golden changes.
Native probes on Bird's RTX 4090 verified tools and 96K allocation; the metadata
context ceiling of 262,144 is not a claim that this GPU fits that context.

## Framework large-model identities (2026-10-27)

The reviewed identity catalog adds Qwen3-Coder-Next Q4_K_M and GPT-OSS 120B
on Ollama 0.32.3. Exact manifest and canonical definition locks are embedded in
the worker. Coder-Next accepts model-default omission and explicit Off; GPT-OSS
120B accepts model-default omission and Low/Medium/High. Unsupported inherited
reasoning settings fail validation without modifying the selected recipe.
Existing model locks, controller goldens and archive/API schemas are unchanged.
Both new locks enter the Docker capability digest, so daemon and worker bundles
must be rebuilt together. See [the exploration record](rust-framework-models.md)
for native validation and the distinction between metadata context ceilings and
tested memory allocations.

## Continuous native execution (#110 step B, 2026-10-26)

Promotes the existing compaction controller behind an explicit frozen native
profile. It uses input manifest v2 and `execution_v2`, with no fabricated sessions.
Version-1 evidence links identify original work/summary request observations and
the retained plan by sequence and record hash; missing sources stay explicit.
`compaction-links-v1.json` is a separate serialization golden. Storage validates
admission, purpose, scope and source locations for inline and external payloads;
the bounded `compaction_history` projection preserves omissions.

Ollama now carries the first terminal `done_reason` to the shared controller.
Missing metadata remains unknown; truncated summaries cannot replace history.
One native agent and its absolute deadline survive compactions, followed by one
stopped capture and the existing publication/cleanup barriers. The package
capability digest includes the new fixed smoke template and probe seed. Existing
memento goldens, recipes, allocations, model/environment locks and service ledger
are unchanged. Catalog/API/workbench selection remains step C.

See [native profile and evidence](rust-continuous-native.md) for the explicit
smoke allocations, gates and retained failed attempts. This does not implement
restart/recovery or continuation from an archive.

## Continuous compaction policy (#110 step A, 2026-10-26)

Adds the test-only `continuous_compaction_v1` controller policy and version-1
settings: 75% trigger, two retained complete exchanges, 2,048 summary tokens and
editable instructions. The frozen per-call allocation bounds the summary cap.
Normal-completion metadata is an explicit new effect outcome; old responses do
not acquire invented completion proof. Native adapters are unchanged in this slice.

New controller outcomes distinguish `context_limit`, `compaction_failed`,
`compaction_no_progress` and `invalid_completion`. The new
`compaction-policy-v1.json` golden retains defaults, tool schemas, exact message
hashes, global request/seed order, context estimates, compaction observations and
finalization. It does not replace or regenerate any memento fixture. Legacy
request/response serialization, saved recipe bytes, archive versions, locks,
reviewed budgets, service ledger and production selection remain unchanged.

The controller requires a sole, bounded `finish_run` call, then actual publication
acknowledgment before agent-finished finalization. Cancellation and late receipts
retain first-stop semantics. The CPU gate simulates effects/receipts; native
process continuity, durable compaction read contracts and API/workbench support
remain #110 steps B/C. Production workers cannot select this policy.

PR #142 review correction: before continuing work with a retained suffix, check
the next boundary's summary input on the conservative byte basis it will need
after dropping recent exchanges. Compact earlier if deferring would make that
summary impossible, even when measured work occupancy remains below its trigger.
Regression coverage performs three compactions with reported prompt usage and
both one and two retained exchanges. Existing byte-estimate compaction and memento
goldens remain unchanged; no archive or schema version changes.

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

PR #141 review correction: purpose accounting also retains request identities
from omitted v2 observations. A verified prefix before admission keeps its call
count unavailable, just like a retained observation; later admission and settlement
count the request once. Archive bytes and existing goldens remain unchanged.

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
CLI works with the real daemon. Its fixture gate and the normal and lifecycle
service gates are implemented; worker-loss recovery isn't planned (#58). See
[the native client guide](rust-cli.md) and [development](development.md).

| Python behavior | Rust CLI behavior |
| --- | --- |
| `run RECIPE_PATH` freezes and owns foreground execution | `run RECIPE_ID` uses a catalog ID, saves a keyed receipt, submits to the service, and detaches after acceptance by default; `--wait`/`--follow` observes without owning execution |
| Model adapter `--endpoint` is an Ollama endpoint | Rust `--endpoint` is the Playtime HTTP service, paired with `--token-file`; no model connection occurs in the client |
| Browser control and worker status through `serve` | `service status`, catalog/preview, runs, events, checkpoints, and artifacts use the authenticated versioned API served by `playtimed` |
| Stop the owned foreground experiment | SIGINT/SIGTERM only detach the Rust observer; use `runs cancel ID [--wait]` to request service cancellation |
| Inspect local run folders/reports | Rust inspects opaque run/session/artifact IDs through the API and verifies explicit downloads; it never opens the service's run folders. `runs download` saves a run's final workspace through the API, verified file by file |
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

## Continuous recipe and workbench integration (#110 step C)

Definition schema 3 freezes compaction settings and its effective protocol and
inventory alongside inactive source memento data. Optional clone/revise settings,
`HarnessView.compaction`, `RunView.request_accounting`, and the corresponding
capability/schema entries are deliberate additive API changes. The HTTP schema
golden adds only these fields/DTOs; existing response and memento behavior goldens
remain unchanged. Installed harness enumeration now includes continuous compaction.
The native capability digest changes to cover saved continuous profiles and their
default protocol, inventory and instructions. The inventory's last line is written
from the recipe's limits, so the digest covers the reference recipe's version of
it. Older worker bundles therefore can't run any Docker profile, memento included,
through a new daemon; rebuild the bundle.
No service ledger migration/reset or model/environment lock changes are involved.

Continuous trace projection preserves run/request purpose identities and final
capture publication without inventing sessions. Unknown accounting remains
explicit. New synthetic wide/narrow editor and trace screenshots cover this UI;
existing screenshot baselines remain unchanged. See
[continuous execution](rust-continuous-native.md) for active versus inactive
parameters, native verification, and the bounded final-file listing.

## Continuous tool-call recovery

`continuous_compaction_v1` no longer stops at a model's first tool-call mistake.
A batch with an invalid ordinary call still runs none of its calls. Each call now
gets an error result and work continues; before, the run stopped with
`tool_parse_retry_limit`. A work request whose tool call the server can't parse
is retried with memento's notice, without its budget sentence. More than five
consecutive failures stop the run with `tool_parse_retry_limit`. A notice opens
the next exchange, so compaction keeps it with the response that follows. A
malformed or mixed finish, an oversized batch, more than eight calls and a failed
summary still stop as before. A recovered batch's calls appear as `tool_request`
and `tool_result` records without dispatch, as invalid memento calls do. They
record no `tool_batch_rejected` observation; that kind still means the run
stopped.

A work response that calls no tool is now followed by a notice to continue or
call `finish_run`. Without it, the next request ended with the model's own
message; in a gpt-oss trial that produced 153 empty one-token responses in 54
seconds. The notice opens the next exchange, like a retry notice.

Archive, API and settings schemas are unchanged. `compaction-policy-v1.json`
changes deliberately: its responses call no tools, so each later input grows by
204 bytes per notice in the conversation. Request IDs, order, seeds, allowances,
estimate bases, compaction points and the final result are unchanged, and a
pending notice stays out of the summary input. New CPU cases cover recovery and
the notice. Memento behavior and goldens are unchanged. The new `continuous_tool_recovery`
capability changes the native capability digest, so a new daemon refuses older
worker bundles; rebuild the bundle.

## Continuous run limits in the protocol

Selecting `continuous_compaction_v1` now freezes a protocol that ends with the
recipe's limits, for example "This run lasts at most 30 minutes or 300,000
generated tokens, whichever comes first. Calling finish_run ends it early;
unused time and tokens are not carried over." Before, a continuous model saw no
limits, unlike memento's per-turn budget, and in a 10-minute gpt-oss trial it
finished after 90 seconds. The line is written at selection, like the
inventory's workspace line. Saved recipes that already selected continuous keep
the protocol they froze. Schemas, the controller and its goldens are unchanged.
The default definition is part of the native capability digest, so a new daemon
refuses older worker bundles; rebuild the bundle.

## Optional finish_run in continuous runs

Continuous compaction settings gain `allow_finish_run`, true by default. When it
is false, work requests offer only `exec`, `read_file` and `write_file`, and the
run lasts until its time or token limit. A `finish_run` call is an ordinary
unavailable tool, and the no-tool notice says the run ends when its budget runs
out. The protocol is rewritten from a separate versioned text,
`continuous-protocol-no-finish-v1.txt`, that never names the tool. In a
10-minute gpt-oss trial that offered finish_run, the model finished after 77
seconds even with its limits in the protocol.

The field is omitted when true, so existing settings, definitions, digests and
the controller golden are unchanged. The HTTP schema golden adds only the
property. `recipe_compaction_finish_v1` advertises it, and clients refuse to send
`false` to a service without it. The continuous editor shows a checkbox, so its
two reviewed screenshots change; in the wide one, the launch row moves below the
fold, and `harness-editor.png` still shows it. The new `continuous_finish_choice`
capability changes the native capability digest, so a new daemon refuses older
worker bundles; rebuild the bundle.

## Runs that end at their wall time

A worker's systemd unit now has `RuntimeMaxSec` set to the frozen wall allocation
plus 180 seconds, instead of the allocation alone. Both clocks started at READY,
so systemd stopped every run that reached its wall time at the moment its
controller stopped. The worker still finished its archive under SIGTERM, but the
coordinator hadn't requested that stop and left the run `reconciling`
(`worker_unavailable`) until an operator released it. A continuous trial without
`finish_run` ended this way on 2026-10-26. Now the controller's own clock ends the
run, and the coordinator confirms completion and stops the unit as for other
normal endings. A worker that hasn't finished within the margin is still stopped,
and a stop request keeps its 180-second timeout.

Units created by an older daemon fail the new property check, so let runs finish
before upgrading the daemon. Wire formats, archives, worker bundles and goldens are
unchanged.

## Continuous command limits in the inventory

The frozen continuous inventory now ends with the recipe's command limits, for
example "A command still running after 57 seconds is stopped, along with any
background jobs it started. The tool results of one response share 16 KiB;
longer output is shortened." It then suggests starting longer jobs in the
background with their output in a file. The time is `command_seconds` less the
3-second return allowance, which is when the bridge kills the command's process
group. Before, the inventory said only that commands have a timeout and an
output limit. In a 10-minute gpt-oss trial, the model's later renders ran into
the limit it hadn't been told. Like the workspace line, the paragraph is written
at selection, so saved recipes that already selected continuous keep the
inventory they froze. Schemas, the controller and its goldens are unchanged. The
default definition is part of the native capability digest, so a new daemon
refuses older worker bundles; rebuild the bundle.

## Continuous summary retries

A summary request that the server can't parse is now retried like a work
request: it counts toward the same five consecutive retries, and the failed
attempt is charged its full allowance as unknown usage. The retry has no notice.
The policy plans the same compaction again, so its budget and fit checks see the
charge, and sends the same summary request. The new `compaction_planned` record
keeps the compaction's ordinal, so the applied compaction has the ordinal of its
last plan. Before, the first such failure stopped the run with
`tool_parse_retry_limit`, though nothing had been retried. A 10-minute gpt-oss
trial stopped that way at its first compaction, 82 seconds in: the model
answered with a tool call that Ollama couldn't parse, though summary requests
offer no tools. Other summary failures still stop the run. Archive and event
schemas and the controller golden are unchanged. The new
`continuous_summary_recovery` capability changes the native capability digest,
so a new daemon refuses older worker bundles; rebuild the bundle.

## Continuous summary transcripts

With recent exchanges retained, a continuous summary request now has two
messages. A short system message replaces the agent's protocol and inventory,
and one user message gives, as text, the task, the message holding any previous
summary, the older exchanges and the compaction instructions. Responses appear
as their content and tool calls, followed by their results and notices; thinking
is left out. Before, the request repeated the work conversation's native
messages, tool calls and thinking under the agent's protocol, then the
instructions. In 10-minute gpt-oss trials, the model treated that request as its
next work turn. One first summary came back as a tool call that Ollama couldn't
parse, and another as a valid `exec` call, which stopped the run with
`compaction_failed`. In those trials, thinking was 24–54% of the 50–53K-byte
summary requests, and gpt-oss's template drops it anyway: Ollama counted only
6.4K–10.9K prompt tokens. With zero retained exchanges, the request still
extends the measured work conversation, because the work reserve covers only
that continuation. In the controller golden, the three summary requests' message
hashes and input bounds change (6,830 to 6,979 and 7,186 to 7,294), and so do the
plans' `summary_input_bound`. Work requests, compaction points, replacements and
the result are unchanged. Archive and event schemas are unchanged. The new
`continuous_transcript_summary` capability changes the native capability digest,
so a new daemon refuses older worker bundles; rebuild the bundle.

## Continuous summaries answered with tool calls

A summary request answered with tool calls, although summary requests offer no
tools, is now retried like one the server can't parse: it counts toward the same
five consecutive retries, the policy plans the same compaction again under the
same ordinal, and no notice is sent. The response settled normally, so the
attempt is charged its known usage rather than a full unknown allowance, and the
`model_error` record has kind `summary_tools` with no budget charge of its own.
The attempt leaves the measured context unchanged, so the retry is planned from
the basis the first attempt had, as a retry after a parse failure is.
A settled summary no longer resets the consecutive-error streak; only an
accepted summary does, so settled and unparseable summary failures count
together. Exceeding the streak with tool-call summaries stops with
`compaction_failed`. Before, the first such response stopped the run with
`compaction_failed` and the diagnostic "summary returned tools despite its
tool-free contract". The first live run on main after transcript summaries
stopped that way at its first compaction, 189 seconds in: gpt-oss reasoned that
it should check a file size before writing the summary and called
`container.exec`, a tool that exists nowhere in Playtime (#153). Archive and
event schemas and the controller golden are unchanged; `model_error` kinds are
an open vocabulary. The new `continuous_summary_tool_recovery` capability
changes the native capability digest, so a new daemon refuses older worker
bundles; rebuild the bundle.

## One-pass prefix extension checks and reused snapshots

`ArchiveReader::sub_snapshot` validated the covering prefix and then the
historical position in two complete passes over the event log, and the daemon's
confirmation of each worker status made three such passes (`confirmed_snapshot`
plus `sub_snapshot` for the previous receipt) before the projection scanned the
log once more. On a 14.5 MB, 3,203-record archive, each pass took about a
second in a release daemon and 15 s in a debug one, so during a 10-minute
continuous run the published view fell behind by 65 to 85 s near the end and the
terminal state followed the archive by about six minutes.

The scan now takes an optional checkpoint position and verifies it where the
pass crosses its byte count, by sequence and running SHA-256, so a sub-snapshot
is one pass, and the new `confirmed_snapshot_extending` verifies a reply's prefix
and its extension of the previous confirmation in one pass. A position inside a
record, with another history or with the wrong sequence is `invalid_cursor`
rather than corruption, once the whole prefix has validated; over HTTP that
mapping was already `invalid_cursor`. The daemon keeps the snapshot its own
verification produced, for at most the 16 runs it refreshes at once, and a
projection or completion check at the same confirmed frontier reuses it instead
of verifying again; a changed frontier or a restarted daemon verifies again. The
projection built from it still reads the history in pages of 200 records, and
each page validates the complete committed prefix, so a projection's passes
still grow with the log. Archive and event schemas, the storage limits and every
verification rule are unchanged; a history page still validates its complete
covering prefix.

## Named inference targets (2026-10-27)

`inference_targets_v1` adds a bounded target configuration catalog and read-only
`/targets` route. Metadata calls accept an explicit target header. Optional target
IDs in preview/launch requests and lifecycle admission records preserve historical
serialization when absent. Target descriptors in previews, run views and frozen
native hosts are also omitted for old/unselected records. The HTTP schema golden
adds these fields, target schemas and route/header declarations; existing HTTP
behavioral fixtures are unchanged. The native profile capability digest now
requires daemon and worker bundle rebuilds. Model locks, recipes, prompts, limits,
concurrency and historical archives are unchanged by target selection.

## Calibrated context estimates and deferred summaries (2026-10-28)

The continuous policy counted every byte of unmeasured input as a token: the
summary transcript, the replacement after a summary and rewritten history. On
the Framework's 120B runs that overstated gpt-oss text almost fourfold, fired
compaction at 48% of a 65,536-token context and then stopped a 30-minute run
after its first summary because a 29,402-byte replacement, about 8K real
tokens, failed the 29,144-token room check by 258 bytes. The context window now
keeps the densest server prompt measurement of the run and estimates unmeasured
input at that rate with a 3/2 density allowance, never above the byte count;
reserves stay in bytes. Request records and the `compaction_planned`,
`compaction_applied` and `compaction_deferred` records report the basis
`calibrated_bytes` once a measurement exists; `compaction_planned` adds the
`calibration` it used and `compaction_applied` adds `input_bytes` beside its
estimate. A summary that the run's measured pace says cannot finish before the
wall deadline is deferred with a `compaction_deferred` record while work still
fits with its reserve, past the trigger if need be. The policy facts gain the remaining wall time, model time and known
tokens. The compaction contract golden adds the two fields; memento goldens,
archive and HTTP schemas and the runner's bounds are unchanged. The reviewed
`curiosity` recipe raises `model_seconds` from 180 to 400 with the owner's
approval: gpt-oss:120b generates about 30 tokens per second on the Framework,
so an 8,192-token call can take close to 300 s, and the 180 s set for the 20B
stopped a 60-minute run after one 163 s think; the controller's timeout stop
is not recoverable. Existing archives keep their frozen recipes. The
protocol's compaction evidence validator accepts the `calibrated_bytes`
basis; the first calibrated `compaction_applied` record of a live run was
rejected as malformed before that, which stopped the archive writer while
the worker stayed alive and the daemon reconciled. The scripted continuous
gate's summary responses now carry a prompt measurement so that gate
publishes calibrated records through the validator. A review of the branch
extended its deferral workload past the first deferral: the run stopped at its
context limit at 60 s of 100 s, because deferred work had outgrown the summary
and the reserve then forced one that could not fit. Deferral now applies to the
trigger alone; the reserve and the summary-fit checks compact whatever the time
left, and the pace behind the deferral estimate counts only requests with known
usage, since failed calls added model time without tokens and made the
hour-long run's estimate about three times too long. `compaction_deferred`
joins the compaction-history evidence kinds, unlinked and unvalidated like
`compaction_rejected`, so the history query returns the explanation for
continuing past the trigger, and the workbench trace labels it; the HTTP
schema is unchanged. The calibration comment and the controller docs now
describe the estimate as a heuristic rather than a bound, since content denser
than the measured prompts is underestimated until the server measures it; a
unit test covers a change in content density and an engine test a summary
transcript measured denser than its estimate. A fourth estimate basis,
`calibrated_prefix_plus_new_bytes`, marks requests that extend the admitted
estimate of a calibrated request whose response brought no prompt count, such
as the replacement after a compaction on a backend that omits the count.
Before it, every request after such a response was rescaled whole at the
calibrated rate, new tool output included, which is why the scripted
continuous gate's compaction count fell from 15 to 5 once its summaries
carried a measurement. The validator accepts the basis.

## Run summary lines and call costs in the workbench (2026-10-29)

The run header gains a context line (the latest work request's admitted
estimate and basis against the run's context capacity, the compaction trigger
and the compactions planned, applied, deferred and rejected), a line counting
model errors and failed tool results, and, for a live run, a note when no
record has reached the workbench for 30 seconds. Call cards add their duration,
the server's prompt and generated counts and the generated tokens over the
whole call. All of it is read from records the archive already holds, so
archives and the HTTP schema are unchanged. The reviewed UI baselines that
show a call card or a run header were regenerated for the new status and
header lines, and the synthetic continuous scene now records a context
capacity, a compaction configuration, context bounds and token counts, with
its compaction records starting at sequence 7.

## Target refusals exit as rejections (2026-10-30)

A CLI command refused with `execution_unavailable` whose reasons are all target
choices (`target_required`, `target_unknown`, `target_requires_live_execution`)
now exits 5, a rejected operation, instead of 3; a retry can't clear it. Other
execution outages still exit 3. The HTTP responses, the schema golden and the
behavioral fixtures are unchanged.

## Failed executions stop their worker (2026-10-31)

A worker whose execution ended without a normal committed outcome kept serving
status until systemd's `RuntimeMaxSec` (the wall allocation plus 180 seconds),
holding the run and live-model locks, and its run waited as `reconciling` with no
reason (#162). The worker's private status now has an optional `failure`: its
own bounded account of why, set once before its publication settles. A
cancellation or stop that ends cleanly is not a failure. The daemon stops such a
worker once its publication settles, then records reason `execution_failed`. The
run stays reserved until an operator releases it, as for `worker_unavailable`. A
cancelled run keeps its own stop path.

`RunView.orchestration` gains an optional `failure` with that text, and the HTTP
schema golden adds it. `runs wait`, `run --wait` and `runs cancel --wait` exit 9
for `execution_failed`, and the run summary and workbench details show a
`Worker failure:` line. Older clients ignore the field and don't recognize the
new reason, so they keep waiting on such a run.

Once the daemon's own stop of a completed or failed worker is accepted, a refresh
now only observes the unit. Before, a refresh while the unit was still stopping
asked the worker for status, which it no longer answers, so a normal run that took
more than one poll to stop could briefly read as `reconciling` with
`control_uncertain` or `worker_unavailable`. That was the gap left after #93.

The private worker protocol version stays 1: older workers omit the field, and a
missing `failure` keeps the earlier behavior. An older daemon rejects a status
that carries one and records `control_uncertain`, so with a newer worker a failed
run still waits for `RuntimeMaxSec`, as before. The ledger version is unchanged
too. Older daemons reject a saved status with `failure` or a record with the new
reason, and they read every record at startup, so they refuse to start on a
ledger holding such a run, even after its release. Archives, recipes and the
behavioral goldens are unchanged.

## One-pass run projections (2026-10-31)

The daemon's run view projection read the history through `ArchiveQueries` in
pages of 200 records, and each page validated the complete committed prefix and
its referenced content again. `run()`, `request_accounting()`, the projection's
own page loop and `captures()` each walked the history that way, so a projection
made about four passes per 200 records. In a release build, a projection's
archive reads took 10.3 s for trial 8's archive (3,203 records, 14.5 MB of log)
and 22 s for Framework run 6's (6,416 records, 17 MB). The daemon projects again
at each saved confirmation, so the published view of a long run fell further
behind as its log grew.

`ArchiveReader::visit` hands every record to a visitor in one validating pass.
It validates what a page does, but the visitor sees each record before the rest
of the prefix has validated, so what it builds counts only if the visit returns
Ok. `ArchiveQueries` reads through it, so `run()`, `request_accounting()`,
`captures()` and `observation()` are one pass each, and the new `overview()`
returns `run()` and, for an execution archive, `request_accounting()` and
`captures()` from one pass that also serves the projection's own per-record
work. The same reads now take 0.25 s and 0.27 s; in a debug build, trial 8's
took 77.6 s and now take 2.1 s. The daemon's completion check reads its run in
one pass too. Archive, event and HTTP schemas, the goldens, storage limits and
verification rules are unchanged. On all 97 archives on the development machine
(52 v1, 45 v2), the queries' results, every history page and the first five
observations were byte-identical before and after. On an archive that fails
validation, a visit can report the visitor's own error, from a record before the
corrupt one, where a page would have reported the corruption; nothing is
published either way. History pages over HTTP still validate their complete
prefix each.

## Workbench navigation and file text (2026-10-31)

The timeline's session row becomes a navigation row: the session chooser, a
**Call** box with **Go to call**, and a jump list of compactions, model errors
and failed tool results, latest first. The jump list and the call lookup read
the transcript index, which charges its entries to the existing 32 MiB bound,
so they make no new API requests. An opened file leads with its name, size and
**Close file**, and its text pages by 24 KiB in a framed block, with the page
controls above it. Record source pages shrink from 64 KiB to 24 KiB, because the
inspector draws before the timeline and a 64 KiB page used the whole frame's
text budget, so a long source or file left the timeline cut short. Paging back
through source, file or preview pages no longer skips bytes: a previous page
ends where the shown page starts, and the first page can at most repeat a few.
Before, when the offset one page back fell inside a multi-byte character, the
previous page could stop a few bytes short. The reviewed UI baselines whose
timeline gains the row or its call box were regenerated, `image` also for the
file heading, and `jump-list` and `file-text` are new. Archives, the HTTP schema
and the behavioral goldens are unchanged.

## Read mode and inspector text shares (2026-11-01)

A cut timeline section names its whole size and offers **Read in full**, which
pages that section's text in the Details inspector by 24 KiB from the transcript
index, reading an evicted record from history first. The run's JSON under
Details and the memory diff page by 24 KiB too; the Memory tab's delivered,
saved and evidence excerpts shrink from 8 KiB to 4 KiB, and their source buttons
still page each whole text. The inspector keeps at most 40 KiB of each frame's
64 KiB text budget, whichever pane draws first, so the timeline keeps 24 KiB.
An open file beside other inspector text shows what is left of that share, and
its byte range and steps follow the part shown. A text still loading closes, with
the reason when its read failed, once its reply can no longer arrive. The jump
list adds rejected tool calls. Jump to live now returns the reader to
the latest activity; before, it only resumed following, which egui applies only
to a view already at the end, so a reader mid-history stayed there. Hack backs
the proportional font, so the non-breaking hyphen, narrow no-break space and
arrows that gpt-oss writes no longer draw as boxes. Applied summaries and
completion claims read as prose. A window section's copy is no longer cut when
cutting would save no more than its ellipsis adds. The reviewed
`compaction-trace`, `trace-long` and `trace-long-narrow` baselines changed for the
prose font and the Read in full button, and `read-mode` is new. Archives, the
HTTP schema and the behavioral goldens are unchanged.

## Archive readers hash frozen inputs once (2026-11-01)

`ArchiveReader` hashed every frozen input at open and again before every
snapshot, page and visit. Since worker bundles became inputs, that is about
54 MB per pass. A reader now hashes them once, at open. Each later pass re-reads
`archive.json` and the input manifest and requires their exact bytes, so a
changed descriptor or manifest still fails every pass. One difference: a
descriptor changed within its 64 KiB bound now always fails with
`content_mismatch`. Before, a pass decoded and checked the new descriptor first,
so one that failed there gave that error instead, such as `malformed_data` or
`unsupported_version`. Every pass still verifies the event-log prefix and the
content its events reference beyond the frozen inputs. An input changed after
open is caught by `read_content` and by the next open, not by the open reader's
passes, and a snapshot's `executable_bytes_verified` now reports the check at
open. The daemon opens a reader for each refresh and history read, so it still
verifies the inputs on each of them, once instead of two or three times. Archive
bytes, the HTTP schema and the goldens are unchanged.

## Long Ollama failures keep their end (2026-11-01)

The Ollama adapter kept the first 4,096 bytes of a failure's message, which a
model error archives as is. Ollama's tool-parse error puts the raw call first
and its reason (`err=...`) last, so a call with long arguments lost the reason:
two of nine parse failures in a 60-minute gpt-oss:120b trial on the Framework. A
message over 4,000 bytes now keeps about 2 KB from each end, with
`[… N bytes omitted …]` between them, at most 4,000 bytes in all: a workbench
timeline section shows up to 4,092 bytes, so a 4,096-byte cut lost the reason's
last bytes there too. Messages of 4,001 to 4,096 bytes, which were kept whole,
are now cut. Only an archive with a failure message over 4,000 bytes differs;
the HTTP schema and the goldens are unchanged.

## Continued reads of a running archive (2026-11-01)

The daemon's work for a running run grew with its log. Each refresh opened a
reader, which hashed the frozen inputs, and validated the whole committed
prefix, even when the receipt hadn't changed. The completion check then rebuilt
the run from the whole history, a changed receipt projected it again, and each
live stream update opened a reader and made two more passes. In a 60-minute
trial on the Framework, the daemon used 52% of a core on average and 93% by
15 MB of log.

A storage snapshot now keeps the pass that validated it, 72 bytes a record
(7.2 MB at the 100,000-event limit) and its state, which
`Snapshot::without_continuation` drops for a snapshot kept for reads.
`confirmed_snapshot_continuing` continues that pass over the new bytes, through
the reader that made it: it checks the new records as a full pass does, and the
content they reference that no earlier record did, without reading the earlier
ones. `Snapshot::check_position` checks a position against the kept record
boundaries without reading. A page reads only the records it returns and fails
with `archive_corrupt` when one's digest differs from the pass's; it no longer
validates the rest of the prefix or the content that earlier records reference.
A sub-snapshot checks its position in the covering snapshot and then validates
the prefix up to it, so corruption past the position no longer fails it.

The daemon keeps, for at most 16 runs, the reader that verified a run's latest
confirmation and the snapshot it verified. While the worker runs or cancels, a
confirmation that extends that snapshot continues its pass. A new reader and a
full pass verify the first confirmation, one that doesn't extend the latest,
each one from a worker that hasn't begun or that ended without a normal outcome,
and at least one every 60 seconds. After a normal end the worker still reports
running or cancelling, so instead the completion check reads the whole history
again and a new reader hashes the frozen inputs again before the run completes.
So changed bytes that an earlier confirmation verified fail a confirmation
within 60 seconds, or keep the run from completing, rather than failing at the
next refresh, and a page or content read that returns them fails at once. Apart
from that check, the frozen inputs are hashed when the daemon opens a reader for
a full pass or a run it hasn't kept, rather than at each refresh and history
read. The projection keeps its fold of the history for each run until the run is
terminal and, when the fold's prefix is a record boundary of the new snapshot,
reads only the records after it; otherwise, as after a restart or for a
historical frontier, it reads the whole history. The completion check reads the
history only once storage holds a committed run end. History and downloads use
the run's kept reader when it has one. A followed cursor is checked against the
published snapshot without a pass, and a download verifies its content's bytes
without a log pass, so a corrupt log no longer fails a download with
`archive_corrupt`. Published evidence keeps its snapshot for reads only,
72 bytes a record for each published run; the state a continuation needs, about
three times as much, stays only with the latest verification of at most 16 runs.

Measured in release builds on that trial's archive (5,685 records, 15.2 MB),
refreshing at every fifth record as the log grew: verification took 56.5 s of
CPU as full passes and 0.28 s continued, one unchanged receipt at the end took
102 ms and now takes 0.28 ms, and projections took 131 s from the start and
0.39 s continued, with the same views, checkpoints and artifacts at all 1,137
steps. On the 99 archives in the trial roots, continued snapshots equal full
passes at about a hundred points each, and continued projections equal whole
ones at about twenty. Their snapshots for reads hold 5.0 MB in all, against
0.9 MB of inspections before and 17.6 MB with the state a continuation needs.
Archive bytes, the HTTP schema and the goldens are unchanged.

## Parse retry notices name the server's reason (2026-11-01)

The notice that asked the model to try again after a response the server
couldn't parse, in memento and in continuous compaction, said only that a tool
call could not be parsed. In Framework run 8, nine of 156 work calls failed to
parse, and one response thought for 8,192 tokens about a parse error it had been
told nothing about.

The notice's first sentence now ends with the server's reason: the text after
the last `, err=` of Ollama's message, which follows the raw call, or else the
whole message, within 512 bytes. A reason that is cut ends with an ellipsis, and
an empty one leaves the sentence as it was. The controller's own parse failure,
`invalid assistant message`, is named the same way. The goldens carry no retry
notice and are unchanged; the archives of new runs record the longer notice in
their requests.

## Workbench: cut-off calls, narrow spaces and new runs' recipes (2026-11-01)

A call the wall limit cut before its response kept the state of its last chunk,
"Streaming · incomplete", after the run had ended. A request settled with
unknown usage and no response now shows "Cut off before a response ·
8,192 tokens charged as unknown usage" (the reserved tokens of its settlement),
in both archive formats. A failure recorded after the settlement, such as a
parse error, names itself as before. A call's answer time now comes from the
latest record that carries one, so a call settled and then failed shows the
failure's time whatever the order its records arrive in; before, the last record
delivered that carried a time won.

gpt-oss writes a narrow no-break space (U+202F) between a number and its unit.
The proportional font lacks it and the fallback font shows it as a wide gap.
Prose the workbench shows (model text, thinking, summaries, mementos, claims and
previews) now carries a no-break space (U+00A0) in its place, which the fonts
have at a space's width. Monospace text, where every glyph is one cell wide,
keeps the character, as do source pages, the archive and downloads.

The workbench read the recipe list when it connected and never again while open,
so a run launched after that was listed by its recipe id. The run list now asks
for the recipe list again when a run names a recipe it lacks, once per recipe,
and names the run once the list arrives.

## A slow archive write no longer ends a run (2026-11-01)

The driver gave an in-flight archive batch five seconds
(`DriverOptions::publication_wait`) before calling the write uncertain: the
execution failed, the queued events were retired, and the run ended, while the
write usually committed a moment later (issue #157: an 11-second disk reset in
trial 10, and run 9's write during a build on the same disk). The default is now
60 seconds. The rules are unchanged: no same-writer retry, no promotion of
uncommitted bytes, and expiry still means uncertainty; only the bound moved.

A batch that commits within the bound but takes
`DriverOptions::slow_publication` (five seconds) or longer is now recorded: the
controller emits a `publication_delayed` observation with `waited_ms`, `events`
and `bytes` as the next record, so the archive shows that no record was
published for that long; dispatched model and tool work went on meanwhile. A
slow terminal batch can't be followed by a record, so the driver keeps it in the
report's `unrecorded_delays`, which the worker's journal prints and which never
counts as a failure. New archives may carry the new observation kind; readers
treat an unknown kind as data, and the workbench names it. The goldens are
unchanged: no scripted scenario writes slowly.

## Reads of base64 reach the model as a head and a note (2026-11-02)

`read_file` returned up to 16 KiB of any UTF-8 file, embedded images included.
In Framework run 8 the model read a gallery page of base64 four times: each read
cost about 10,800 tokens, set off a compaction and grew the context by 22K
tokens, and taught it nothing.

Runs of 64 or more base64 characters in a slice, when they total at least 1 KiB
and one of them holds `+` or `/` (hex digests, nucleotides, digits and letters
alone never do), now reach the model as markers with their byte counts, `[…
5520 bytes of base64 withheld …]`, with the rest of the slice as it was: a
gallery page keeps its captions and loses its images. `base64.withheld_bytes`
and `base64.slice_bytes` say what was withheld and how long the slice was, and
`base64.note` says that exec reads, decodes or summarizes the file as is and
that `offset_bytes` past `slice_bytes` reads on. The tool's definition is
unchanged: an argument or a sentence added to it would have changed the pinned
definitions the frozen Python suite checks, the memento observation goldens and
the compaction contract golden, whose byte-based bounds count the tool
definitions in every request. The archive's `tool_result` record keeps the whole
slice as `unabridged_result`, as it does for fitted results, so the evidence is
unchanged. No scripted scenario reads such a file, so no golden moved.

## Tool-failure evidence survives compaction (2026-11-02)

In Framework run `20261102T173751Z-b2f3612b`, tar reported quota and unexpected-EOF
failures, but later summaries claimed the archive had been recreated and
verified. The full results were recorded and sent to the model; summarization
removed them from its working conversation.

Continuous compaction now carries `tool_failure_history_v1` beside the generated
summary. The controller derives it from recorded tool errors, nonzero exec exits
and timeouts before output fitting. It retains up to four failures in a
4,096-byte encoded message, with bounded head/tail excerpts, explicit omission
counts and original request/controller-observation references. It neither parses
commands for semantic meaning nor claims failures remain unresolved. Later
success does not erase prior evidence. Retained suffix exchanges contribute when
they are compacted; failed summaries do not change the history. Later work and
summary requests carry it, and existing context/progress checks count its bytes.

`tool-failure-history-v1.json` is a new simulated golden for the record and notice.
Regression coverage retains a failed archive check through a later successful
call and two summaries falsely claiming repair, in both summary request formats.
Healthy compaction and memento goldens are unchanged. No archive/API schema or
recipe limits change. The `continuous_tool_failure_history` capability changes
the native digest, requiring a matched daemon and worker rebuild. This preserves
evidence for the model; it does not establish that the model will interpret it
correctly or verify artifacts. Parse failures remain outside this tool-result
history.

## Native smoke summary allocation (2026-11-02)

The owner requested a correction after Framework's GPT-OSS 120B High-reasoning
summary exhausted the native smoke's 512-token cap without final content. Its
isolated replay needed 3,104 tokens and 85 seconds. The fixed continuous native
profile now allows 4,096 summary/output tokens and 400 seconds per inference,
matching the existing hour experiment's summary allowance and deadline. Its
absolute run cap rises from 300 to 900 seconds and total generated-token cap
from 16,384 to 32,768 so repeated compactions can complete. Context rises to
32,768 tokens to fit the larger reserves and retained exchanges; the smoke's
trigger moves to 40% so its 48 stages still exercise multiple compactions.
Normal admission, accounting and partial-summary rejection stay in force.
Catalog recipes and policy/API defaults do not change.

The frozen template changes the native capability digest, so preparation requires
a matching rebuilt worker. Old archives retain their original inputs and failed
outcomes. No behavioral golden contains this fixed native template. An archive
driver regression completes a 3,104-token summary after 85 simulated seconds,
then continues through later compaction and final capture. The native gate can
explicitly select the already reviewed GPT-OSS 120B identity at High reasoning;
Qwen with reasoning off remains its default.

## Parse-failure history and explicit gate diagnostics (2026-11-02)

The owner approved retaining malformed-call evidence before the next Vulkan
hour experiment. `failure_history_v2` extends the bounded tool-only history with
typed `tool_result` and `model_parse_error` records. Parse records come only from
emitted `model_error` observations, retain the failed request ID/purpose and
source sequence, and mark usage unknown with its charged reservation. Bounded
pending records join the next successful work exchange and survive intervening
compaction. Summary retries keep their existing inputs; accepted compaction
promotes removed exchanges' evidence. Success does not erase it. The same four
record/4-KiB message/256-byte excerpt limits apply to both kinds together.

The new versioned history golden and captured Framework parser-error fixture
exercise recovery, no malformed tool dispatch, accounting and repeated
compaction. The old tool-only golden remains historical; healthy compaction and
memento goldens are unchanged. The native capability digest includes version 2,
requiring matching daemon and worker builds. The strict native gate now states
its unknown-request and reserved-token counts when it fails; its acceptance
conditions are unchanged. This does not invent missing backend token usage or
establish that the model interprets retained evidence correctly.

## Workshop pipeline status (2026-11-03)

The owner approved fixing a masked failure from the Vulkan hour: `tar -tzf ... |
wc -l` returned zero while tar reported a truncated archive. The bridge now starts
Bash with `pipefail`, without enabling `errexit`. Expected early-consumer SIGPIPE
statuses are visible too; explicit handling and normal sequential-command
semantics remain available. Successful stderr output is not reclassified.
The existing controller failure history consumes the resulting nonzero status.

The workshop was deliberately prepared with refresh through the existing
environment-only preparation helper; this does not refresh the model lock or run
a Python experiment. The general and embedded native image locks identify the
new image; prior image archives and run inputs remain intact. New continuous
selections use `continuous-inventory-v2.txt`, and the catalog inventory describes
the shell semantics. Recipe limits are unchanged. Existing saved definitions
remain immutable and require their original matching runtime; new runs need a
new selection and a matching rebuilt daemon/worker. The new image identity
changes the native capability digest. No behavioral or schema goldens change.

## Measured usage on rejected tool calls (2026-11-03)

The owner approved fixing the observed loss of 405 generated tokens between
llama.cpp and Ollama's parser-error response. A versioned local Ollama patch
preserves terminal metrics through both the Harmony-error and stream-writer
paths. Its distinct `0.32.3-playtime-usage-v1` identity is selected explicitly;
the original model locks, runtime and native inference payload remain available.

The native adapter validates the terminal error envelope and passes a typed
`ModelParseError` outcome to the controller with its measured count. This is not
a successful assistant response. Known errors settle once, retain parse/retry
behavior, obey the total token allocation, and never execute rejected tools.
The full error observation carries `request_usage`; existing unknown errors keep
their old record shape and full reservation. Overflow, cancellation, invalid
usage and late completions cannot fabricate a count or replace a settlement.

`failure-history-v3.json` versions the notice/record contract so a parse-failure
history can carry either known generated tokens or an unknown reservation.
Version 2 remains historical. Regression coverage checks both request purposes,
zero and nonzero measured counts, recovery, repeated compaction and archive
accounting. Existing healthy compaction and memento goldens do not change.
The native capability digest includes the usage contract and new reviewed model
lock; paired daemon/worker rebuilds are required. Recipe limits do not change.

## The workbench totals a run's results (2026-11-02)

After each real run the operator summed the archive by hand: time in model
requests by purpose, calls with their durations and rates, errors by kind,
tool failures by message, compactions and the final files. The workbench now
keeps those totals in its transcript index, next to the progress tracker, and
shows them on a new **Results** inspector tab, which a run opens on. Each
count links to the first record it counts. The totals stay bounded (open
calls, durations per purpose, error kinds, tool names, failure messages and
compaction times each have a cap, with the overflow counted) and appear only
once earlier history has loaded, so a partial load never reads as a total. A
call counts when its request and its settlement are both indexed; the
settlement's time, which the controller records with the response or error,
bounds the call, and the time of calls settled with unknown usage is reported
as such rather than attributed to errors. A request the controller never
dispatched counts as such, and a memento request whose payload is retained
elsewhere, with its handoff flag, counts on a line of its own rather than as
work. The workspace size is the latest captured until the run has ended, and
an ended continuous run without a final capture says so rather than listing
an empty workspace. A rate or median is left out, never guessed.

Run rows add a line under the state: an ended run's stop reason, tokens
charged and cleanup, and a running run's tokens so far, from the run view
alone. Rows are taller for it.

No archive bytes, API or goldens change. The reviewed screenshots changed on
every screen with a run list (the row line) and every run view (the tab); the
three results views are new baselines.

## `playtime runs report` totals a run (2026-11-02)

The totals behind the workbench's Results tab moved to a shared accumulator
in `playtime-client` (`report`), fed by the workbench from its transcript
index and by the CLI from a run's event pages, so both word a run the same
way. `playtime runs report RUN_ID` reads every record of the run as of the
frontier its first page fixes, totals them within the workbench's bounds,
then lists the final workspace: an ended continuous run's final capture from
its artifact listing, or a memento run's last published checkpoint. Its text
output is Markdown, a heading per section with a line per total, with the
link, image and HTML punctuation of archive-sourced text escaped; `--format
json` adds the typed totals (`report`), the listing (`files`), the run view
and the records counted next to the worded sections (`summary`). The run has
ended when the service reports it terminal or its archive records its end. A
run that hasn't ended reports its totals so far, with open calls in progress
and its latest workspace. The reserved top-level `report` command (#26, the
Python HTML report) is unchanged.

Reporting the Framework hour run in sequence order found a defect in the
accumulator the Results tab shipped with: the continuous format records a
settlement fact a few records after the observation that settles the call,
and in sequence order that fact opened a second, phantom call, read as a
settled call without a recorded request. The workbench loads history latest
page first, which paired the records by luck; a run watched live from its
start would have shown the line. The accumulator now remembers the last
1,024 settled calls, so a late usage record adds the usage the call still
lacks (its observation omitted by the controller log limit) or nothing.

The number words both clients use (counts, sizes, durations, excerpts) live
in `playtime-client` too (`words`). No archive bytes, API or goldens change,
and the workbench's wording and screenshots are unchanged.

## Parser-error accounting review corrections (2026-11-03)

PR review found that measured parse failures enforced the total allocation but
missed continuous requests' per-call allowance. The controller now captures the
admitted allowance before settling usage and stops with
`TokenLimitExceededByServer` on either overrun. The measured count and failure
remain recorded, with recovery disabled. Tests distinguish an exact-cap failure
from a one-token overrun for work and summary requests below the total allocation.

Memento retries previously claimed unknown usage even when the patched backend
reported a count. They now state the measured charge; unknown failures retain
the existing notice verbatim. Work/handoff regressions cover zero, nonzero and
unknown usage without changing legacy goldens. The internal
`terminal_parse_error_usage` capability advances to version 2, requiring matching
daemon/worker builds. The backend wire extension, backend version and model
locks do not change.

## History pages take no pass over the prefix and are bounded once (2026-11-02)

A history page read at a fixed frontier (`GET /runs/{run_id}/events` with a
cursor, as `playtime runs report` and the workbench's history load read a
run) rebuilt a snapshot at the cursor's frontier with a new pass over the
whole committed prefix, hashing every record and the content it references,
before reading the page. Reporting the Framework hour run
`20261102T173751Z-b2f3612b` (6,478 records, 16.9 MB of events, 27 MB of
referenced content) took one such pass for each page after its first, 32 at
the default page size, and the workbench, which starts from the run view's
history cursor, 102 at its. The daemon now pages the records up to the cursor's position
from the published evidence, whose pass validated them, through
`ArchiveReader::page_within`; a page still checks every record it returns
against that pass's digest and re-reads the archive descriptor and manifest,
as a page of the current snapshot does. What a page validates changes only
for a record before the position that the page doesn't return, and for the
content such records reference: a change to either no longer fails that page
as `invalid_cursor`. A changed record fails the page that returns it
(`archive_corrupt`, as at the current frontier); either fails a historical
snapshot, which checkpoint and artifact pages at an earlier frontier still
take, a download of the content, and the next full confirmation pass. A
changed archive descriptor or manifest, or a read error, on that path is
`archive_corrupt` too, as on a first page and when following, where the
rebuilt snapshot answered `invalid_cursor`. A checkpoint or artifact page
whose cursor pins the published frontier is served from the published
evidence, not a pass and a projection.

The HTTP layer also encoded the whole page again after each event it added,
to check the page's byte bound, so a page's cost grew with the square of its
size: on the hour run, through a debug daemon, 0.05 s for a 64-record page,
0.4 s for 200 and 10 s for 1,000, which is why 1,000-record pages outran the
30-second handler deadline. The page is now bounded from its framing and each
event's encoding, once each, to the same encoded size.

Pages are unchanged: the same events, frontier, cursors and `has_more`. On
the hour run, through a debug daemon and CLI on a copy of its service root,
`runs report` took 72.7 s at the default page size and 134 s at 64 records a
page, and outran the deadline at 1,000; it now takes 5.2 s at each, the time
the client takes to decode the records, and the reports are byte-identical. A
first page takes 0.01 s, 0.03 s and 0.2 s at those sizes. No archive bytes,
API or goldens change.

## Reviewed parser-usage backend registration, 2026-11-03

The merged-hour preflight exposed a service discovery check that still required
Ollama 0.32.3 even for the compiled, reviewed GPT-OSS 120B identity on
`0.32.3-playtime-usage-v1`. Native worker tests could execute it, but API model
review returned `unverified` and no registration request, blocking normal launch.
Discovery now accepts a different backend version only when the complete model
identity matches the existing compiled capability review. Stock-version candidate
behavior is unchanged. A metadata-only regression covers successful registration
and rejection after version, name, digest or definition drift; it performs no
inference. No wire/schema, worker capability or existing golden changes.

## `playtime runs download` saves a run's final workspace (2026-11-02)

Collecting a run's final files took a script: page the artifact listing,
pick the final capture's or the last checkpoint's file versions, download
each by artifact ID to a numbered file and keep an index of which was which.
`playtime runs download RUN_ID --output DIR [--session SESSION] [--force]`
does that through the shared client (`playtime_client::workspace`): it
resolves the final workspace that `runs report` lists, from the final
capture's index (downloaded and decoded as a checkpoint's is, with each
retained file matched to the listed artifact holding its content) or from
the checkpoint's file list, and saves each retained file as a single
verified download is saved: staged beside its destination, verified, then
published.

The saved names are the workspace's own, which the model chose. Until now no
local filename came from model-controlled metadata; this command takes the
names under a policy instead of sanitizing them: a name is a relative path
without an empty, `.` or `..` component, a control character or a
backslash, with components of at most 255 bytes and at most 1,024 in all,
and any other name is refused, its content still downloadable by artifact
ID. The directories a name lists are created private as needed and opened
without following a link at any level, so a link planted among them, or at
the name, is never written through, even with `--force`, and is reported as
blocking the file, as a directory or special file at the name or a file
where the name needs a directory is; saved files are private (0600)
whatever the archived mode. An existing file is left alone without
`--force`, which replaces regular files only. Content the archive didn't
retain is listed as unavailable with its reason and isn't a failure; a
refused name, a blocked entry, an existing file or a failed download is,
with the first such file's exit code after every file was tried. The result
lists each file with its status, path and verified bytes or its reason or
error. No archive bytes, API or goldens change.

## The Results tab shows thumbnails of the final images (2026-11-03)

The Results tab listed a run's final images as names with an **Open image**
button each. It shows them as thumbnails now: once the final workspace's
listing is in hand (an ended continuous run's final capture, or a memento
run's last published checkpoint), the workbench asks the shared client for
each listed image's bytes, at most 12, one at a time under one of the two
preview permits, verifies them as a single download is verified, decodes
them under the preview limits (4 MiB encoded; a still image's 4,096 a side
and 16 million pixels, a GIF's 1024² canvas and first frame) and scales
them to fit 224×168 pixels, outside the render loop. Each thumbnail is a
button, named **Open image** and the file's name, that opens the image in
place, with the name and size written under it, in cells of one size; an
image still loading, or without a preview (too large, unsupported or
corrupt), says so in its place, with the reason on hover. Thumbnails follow
the listing: a later listing keeps the ones of the content it lists again,
under the new artifacts a memento run's next checkpoint publishes, and
drops the rest, so no more than one listing's thumbnails are held; another
run's selection drops them all; a reply for another run or an unlisted
image is ignored; a newer request, another run or a new connection stops
the request in progress; and the ones a reconnect or a full queue left
pending are asked for again when the tab draws. The texture payload
bound gains the thumbnails, at most 12 of 224×168 pixels. The `results` and
`results-sessions` screenshots were re-reviewed with synthetic pictures in
place; `results-narrow` is unchanged, since its drawer shows the tab's top.
No archive bytes or API change.

## Continuous retry attempt preservation (2026-11-04)

The controller appends a `schema_version: 1` rejected-attempt record
only to continuous work parse retries. It includes the failed request ID and
verbatim raw arguments when the known diagnostic envelope is complete; otherwise
it retains the diagnostic as incomplete evidence. No arguments are repaired or
executed. The entire retry message is bounded to 8 KiB after JSON escaping;
additional omissions are counted, and upstream omissions never claim complete
arguments. A fitting complete candidate is checked before truncation, preserving
Unicode at its midpoint and messages exactly at the encoded limit. Context
admission accounts for the message normally. Summary retries,
memento, usage accounting, retry limits and compacted failure history are
unchanged. The native capability digest gains `continuous_retry_attempt: 1`.
The native gate also accepts the existing locked 20B model and an explicit
`PLAYTIME_CONTINUOUS_LIVE_LOCK` for a separate backend host.

The owner-authorized 600-second Bird RTX 4090 run exercised one parse retry:
the next valid call preserved its exact command, which then failed because the
command was unavailable. This establishes neither successful task execution nor
a causal efficiency gain. Results and costs are retained in
`experiments/retry-preservation/live-10min-results.json`. The retry message grew
from 237 to 1,774 encoded bytes, and its appendix appeared in 16 work requests
and three summary attempts before compaction removed it. The bound is per notice,
not per run; multiple failed attempts can retain multiple notices. No additional
model request is introduced, but retained input can advance compaction and alter
generation length. Exact marginal token/runtime and electricity costs were not
measured. Stock Ollama's missing parse-failure token count stayed explicitly
unknown, with its existing 8,192-token reservation.

## Seed bundle authoring and repair recipe (2026-11-04)

The reviewed file-backed recipe catalog adds `simulation-repair`, with a
600-second allocation and two frozen model-visible seed files. Independent
assessment code stays outside that seed directory. This adds catalog content,
offline authoring/checking helpers, and a catalog regression test; it changes no
runtime policy, API schema, existing recipe allocation, model default, or golden.

## Ollama parameter listing order (2026-11-10)

Ollama lists a model's `parameters` in a different key order after each process
start, so a reviewed identity with more than one parameter failed its check until
a restart happened to reproduce the lock's order (#155). Execution verification
and API model review now accept the expected lines in another key order, with
lines that share a key, such as `stop`, still in order. They use the lock's
listing, so the definition fingerprint stays the reviewed one; any added, missing
or changed line still fails. The raw `/api/show` response keeps Ollama's order.
No lock, fingerprint, wire/schema or golden changes, and existing registrations
stay valid.
