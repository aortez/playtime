# Experiment studio design and development plan

Status: design draft for the phase after the Rust MVP. The scope and harness
behavior below reflect the agreed direction; the contracts and implementation
sequence are proposals for review. This document does not describe released
features or authorize changes to existing recipe limits.

The next phase makes Playtime a studio for manually composing, running, inspecting,
and comparing local LLM experiments through the workbench. The first complete
workflow is: clone a recipe, choose a model and harness, edit inputs and budgets,
review the effective configuration, launch, and compare the result with another
run. The same operations belong in the public API so later study tooling can use
them without duplicating execution logic.

## Agreed scope

- Internal harnesses first: preserve the current turn-based memento harness and
  add continuous conversation with compaction.
- Multiple registered Ollama models, with explicit identities and capabilities.
- Editable recipes covering model, harness settings, prompts, seed files,
  budgets, and selection among prepared, verified environments.
- Immutable run configurations and inspectable context transformations.
- Manual comparison of configuration, outcomes, usage, transcripts, and artifacts.

Context-triggered memento is a follow-up experiment. It may eventually become the
preferred memento policy, while the current version remains identifiable in old
runs. External agent harnesses and hosted model providers are possible future
extensions. Study scheduling, study authoring, automated evaluation, parallel run
scheduling, and building environments in the UI are outside this phase. Time spent
using the studio should inform the eventual study design.

## Current foundation

The [recipe catalog](rust-recipes.md) already freezes prompts, protocol, seeds,
sampling settings, and limits. Recipes load at daemon startup. The
[live path](rust-live-model.md) uses one compiled model identity; the
[Ollama lock validator](../crates/playtime-ollama/src/identity.rs) explicitly
requires that identity. Model selection therefore needs adapter and admission
changes as well as a selector in the UI.

The [controller](rust-controller.md) combines shared enforcement with memento
behavior: it clears conversation history at session start, delivers the memento,
and requests an explicit handoff. The [API](rust-http-api.md) already supplies
preview, keyed launch, observation, cancellation, and verified downloads. Its
launch request selects a catalog recipe and preview revision; it does not yet
accept newly authored definitions. The [workbench](rust-ui.md) already observes
live execution and archived evidence.

Automatic daemon reattachment and archive recovery remain incomplete. Manual
experimentation can advance with that limitation visible. This phase must not
claim restart-safe unattended execution or silently release uncertain work.

## Experiment concepts

The run is the central execution unit. Sessions and turns remain useful events
and optional limits, but they do not define every harness.

| Concept | Responsibility |
| --- | --- |
| Harness | Conversation construction, memory policy, tool interface, context transitions, and completion decisions |
| Model configuration | Provider, verified model identity, capabilities, requested context capacity, and sampling settings |
| Environment | Verified workshop image, supported tool bridge, filesystem and process behavior, and resource limits |
| Inputs | Exact prompt texts and seed contents delivered to the harness and workspace |
| Recipe | A saved, versioned experiment definition combining components, inputs, and budgets |
| Frozen execution | A fully resolved recipe revision plus worker identity and execution bindings, preserved in the archive |
| Run | One attempt to execute a frozen configuration, including failures and incomplete evidence |
| Study | A future plan relating multiple trials, conditions, repetitions, ordering, and evaluation |

Shared budgets bound elapsed work time, generated tokens, model calls when capped,
log and archive storage, and workshop resources. Harness settings determine how
work proceeds inside those bounds. The existing memento version retains its
session and work-turn limits and current context guard. The compaction version
uses context occupancy to trigger memory maintenance.

Context capacity and total generation budget are different controls. A comparison
can give models equal context allowances or use each model's supported capacity;
the chosen policy must be explicit in the frozen configurations. Equal seeds and
budgets do not guarantee identical outputs or equal realized work.

## Harness and runtime boundary

The harness proposes model requests, tool interactions, memory transitions, and
completion. The shared runtime retains authority over admission, accounting,
deadlines, cancellation, dispatch, publication, and cleanup. A harness cannot
increase an allocation, bypass a dispatch barrier, publish its own writer
receipt, or launch model-controlled commands on the host.

Extract this boundary incrementally from `playtime-engine`, keeping the simulator
and native worker on the same execution policy. Preserve current behavior under
an explicit memento version before adding the compaction implementation. Internal
Rust implementations are sufficient; this phase does not need a dynamic plugin
ABI or an external-program execution contract.

Keep pure values in `playtime-core`, wire/archive representations in
`playtime-protocol`, and policy in `playtime-engine`. The runner and worker retain
effect dispatch and storage integration; Ollama and Docker retain their external
effects. Catalog persistence and admission belong to the service. Shared client
operations stay independent of UI rendering and CLI parsing.

The current session reset rule remains the memento contract. A compaction event
does not create a memento session or restart workshop processes. New event and
archive versions must represent that distinction without reinterpreting old
session records or inventing mementos for a harness that has none. Update the
working rules and owning architecture documents when this behavior is implemented.

## First compaction harness

The agreed behavior is one ongoing conversation and workspace. At a configurable
context threshold, the same model summarizes older conversation, preserving the
system instructions and selected recent complete exchanges. Compaction instructions
are versioned and frozen with the recipe. Summarization calls consume the run's
ordinary generation, call, and time budgets, with their usage shown separately.

The proposed execution contract is:

1. Check context fit before every model dispatch, including system text, tool
   schemas, retained conversation, output allowance, and safety headroom. Record
   whether occupancy is measured or estimated and which estimation method is used.
2. Trigger early enough that the summarization request itself fits. A trigger
   threshold is a policy choice; the pre-dispatch fit check is an enforced bound.
   Tool output is bounded before it enters the next request.
3. Compact only at a completed exchange boundary. Never split a tool call from
   its result or compact while an associated tool operation is unresolved.
4. Preserve system instructions and the original task. Freeze the policy defining
   how many recent complete exchanges to keep, the summary output cap, and the
   summary instructions. Later compactions incorporate the preceding summary so
   previously retained knowledge is not silently discarded.
5. Run summarization as a tool-free model request through the same dispatch and
   accounting path. Treat its output as derived memory, not verified facts or
   higher-priority instructions. Keep the original history in the archive.
6. Validate that the replacement conversation fits with room for the next work
   request. If the preserved content cannot fit, the summary fails, or insufficient
   budget remains, stop with a specific reason. Do not repeatedly summarize in an
   unbounded attempt to make progress or silently truncate instructions.
7. Record the source event/content references, exact summarization request,
   generated summary, resulting conversation, occupancy before and after, usage,
   and failure details. Partial generation and unknown usage remain visible.

The owner approved initial defaults on 2026-10-26: 75% projected occupancy
(including the next response allowance), two recent complete exchanges, a
2,048-token summary cap, and editable versioned instructions. The fixed v1 safety
margin is the larger of 1,024 tokens and 5% of context capacity, rounded up.
Compaction instructions, summary output and aggregate tool results have separate
explicit reserves. Hard fit checks may trigger compaction earlier. Preserve the
same selected model and reasoning/sampling settings; no silent override.

Accept only a normally completed, nonempty final-answer summary that reduces
estimated active input and leaves occupancy below the trigger with room for work
and reserves. Preserve original thinking and partial responses as evidence.
The model's usable context and accepted caps still determine valid settings;
reject impossible combinations. Estimates are not exact tokenizer counts.

Completion uses `finish_run` with one required nonempty `summary` string, bounded
to 8 KiB. It must be the only tool call in its response; validate the entire batch
before effects. Keep the model's declaration separate from capture/cleanup
outcomes. Plain text never implicitly finishes, and budget/cancellation stops
do not require another model call.

Compaction is not a workspace checkpoint. The existing capture path requires a
stopped workspace. The owner approved final-only stopped-workspace
capture at finalization, including cancellation when possible, with the event log
published incrementally throughout the run. Do not invoke that capture at every
compaction and claim processes continued. Intermediate workspace snapshots would
need a separate design and should be shown as unavailable until supported. The
comparison UI must expose this difference from memento's session checkpoints.

The [native execution slice](rust-continuous-native.md) supplies archive evidence,
normal-completion metadata, one continuous workshop and final capture through an
explicit smoke preparation capability. Recipe/API/workbench selection remains
#110 step C.

## Proposed definition and API contracts

### Definitions and frozen execution

A definition contains a schema version; harness ID, version, and settings; model
registration revision and sampling settings; environment revision; exact input
references; and run budgets. Human-readable names support browsing. Immutable
revisions identify the configurations used for execution.

Editing creates a new recipe revision and does not mutate existing runs. Cloning
retains the source revision as provenance. The service resolves content references
and defaults, checks compatibility and bounds, and returns a preview of the exact
effective configuration. Admission pins the definition revision and checks that
the preview, identities, execution bindings, and worker capabilities still match.
Changing a registration must not silently retarget an older recipe revision.

The archive includes the complete resolved definition, input bytes, component
identities, model request settings, automatic instructions and tool definitions,
and worker provenance. A human-readable configuration diff complements the
canonical representation. Endpoints and other operational bindings are recorded
separately from experimental choices, with credentials kept outside the archive.

Model discovery is not permission to change identity locks. Registration is an
explicit operation over an already available local model, with read-only inspection
and an accepted immutable identity. Model downloads and automatic lock refreshes
are outside this phase. Admission and the worker verify the selected model;
unsupported capabilities or changed identities refuse execution.

User-authored recipes are stored by the service, separately from the checked-in
reviewed examples. Inputs imported from a local file picker are transferred as
bounded content and frozen; an API request must not grant arbitrary server-side
path reads. Start with the existing seed formats and size limits. Choosing values
within reviewed limits does not authorize raising those limits or editing the
checked-in curiosity recipe's reviewed allocations.

### Service operations

Exact route names and DTO layouts remain an implementation review item. The
required operations are:

| Operation | Contract |
| --- | --- |
| List components | Return registered model and environment revisions, harness versions, supported settings, and readiness |
| Register a model | Inspect an approved local Ollama endpoint and save an explicitly accepted identity and capability record |
| Import inputs | Store bounded content and return immutable references; refuse unsupported paths, types, or sizes |
| Create or revise a recipe | Validate typed fields and persist an immutable revision; detect concurrent edits |
| Preview a revision | Resolve defaults and inputs, show effective budgets and context settings, and report incompatibilities without executing |
| Launch a revision | Require the preview revision and durable idempotency key; retain the existing launch-recovery semantics |
| Inspect and compare | Read frozen definitions, outcomes, usage, transcripts, and verified artifact references through the same API used by the UI |

Mutations that can create duplicate resources need stable request identities and
lookup/replay behavior. An ambiguous launch is recovered using its original key;
an intentional repeat creates a new run with a new key. UI closure only detaches.
Catalog edits must not require daemon restart or affect an active run. Existing
loopback authentication, bounded transfers, and private-file policy remain in force.

### Workbench workflow

The editor groups controls into setup, run budgets, and harness settings. Sampling
settings belong with the model. Selecting a harness changes the relevant controls
without silently substituting behavior. Show incompatibilities before launch and
make the effective preview available for review.

Comparison begins with two manually selected runs. Show configuration differences
alongside stop reasons, cleanup, known generation, unknown reservations, charged
tokens, work versus compaction calls, elapsed time, transcript, and saved artifacts.
Missing evidence is distinct from an empty artifact or zero usage. Do not align
different harnesses by fictional equivalent session numbers or imply a causal
conclusion from a pair of runs. Automated judging and study statistics are deferred.

## Development milestones

Each milestone includes API and minimal workbench integration needed to exercise
its capability. The editor milestone completes the authoring experience rather
than delaying all UI work until the end.

| Milestone | Deliverable | Acceptance criteria |
| --- | --- | --- |
| 1 Experiment definitions | Versioned definitions, component references, resolved previews, frozen archive representation, and service persistence | Existing curiosity behavior and limits are preserved; old archives remain readable; revision edits cannot change a prepared or running configuration; unsupported versions and stale previews are refused |
| 2 Ollama model selection | Model registration, capability validation, generalized identity checks, and UI selection | At least two explicitly selected compatible local models can run the existing harness; identity changes refuse admission or execution; unsupported options are reported; the existing live-run exclusion remains effective |
| 3 Harness boundary and compaction | Versioned memento baseline, shared enforcement, new context policy, and inspectable compaction | Baseline goldens remain valid; deterministic simulations cover repeated compaction and failures; native execution demonstrates process continuity through compaction and final stopped capture; cancellation and budgets hold during summarization |
| 4 Experiment editor | Clone, edit, save, preview, and launch through the workbench and API | Create a variant without editing repository files or restarting the daemon; inputs and revisions survive service restart between runs; stale edits and ambiguous launches do not overwrite or duplicate work |
| 5 Run comparison | Two-run configuration, usage, transcript, and artifact comparison | Compare different harnesses or models from archived data; compaction overhead and unavailable evidence are explicit; comparison remains bounded for long transcripts and large artifacts |

Milestone 1 establishes the shared contracts. Milestones 2 and 3 build on it;
milestone 4 completes their authoring flow, and milestone 5 uses the resulting
frozen definitions and observations. Before implementation tickets are marked ready,
review the proposed final-only capture policy, definition persistence/versioning,
model registration flow, and concrete compaction fit rules. Select the second
local model and numeric defaults during that review; this plan does not prescribe
new downloads, model identities, or larger reviewed bounds.

## Functional tickets

[Studio tracking issue #107](https://github.com/aortez/playtime/issues/107) groups
the work into capabilities that include their API, shared client, and workbench
integration. Existing launcher tickets are refined in place. Each ticket has
acceptance criteria, dependencies, exclusions, and validation requirements;
the functional parent tickets remain `needs-refinement` until their contracts and
open choices are reviewed. First implementation slice
[#113](https://github.com/aortez/playtime/issues/113), under #108, is ready with the
narrow contract below.

| Capability | Ticket | Main dependencies |
| --- | --- | --- |
| Create, save, and clone recipe revisions | [#108](https://github.com/aortez/playtime/issues/108) | Shared definition and persistence foundation |
| Edit model settings and context capacity | [#5](https://github.com/aortez/playtime/issues/5) | #108 |
| Select and register Ollama models | [#7](https://github.com/aortez/playtime/issues/7) | #108, #5 |
| Select and freeze seed files | [#6](https://github.com/aortez/playtime/issues/6) | #108 |
| Select prepared environments and inspect readiness | [#4](https://github.com/aortez/playtime/issues/4) | #108; integrate #7 |
| Select versioned internal harnesses | [#109](https://github.com/aortez/playtime/issues/109): A exposes existing identity; B separates memento proposals from runtime enforcement; C removes session-only assumptions | #108 |
| Run continuous conversation with compaction | [#110](https://github.com/aortez/playtime/issues/110) | #109, #5 |
| Inspect context transformations and budgets | [#111](https://github.com/aortez/playtime/issues/111) | #109 observations, #110 |
| Review and launch the exact effective revision | [#3](https://github.com/aortez/playtime/issues/3) | #108; integrate controls incrementally |
| Compare two runs and their evidence | [#112](https://github.com/aortez/playtime/issues/112) | #108, #111; memento comparison can start earlier |

The dependency on #108 is the agreed definition/admission contract and the minimum
working path, not a reason to delay every UI change until its full implementation
is complete. Existing readability work in
[#92](https://github.com/aortez/playtime/issues/92) supplies common presentation.
Linked-history review in [#11](https://github.com/aortez/playtime/issues/11) remains
separate from comparing independent runs. Recovery and backend-host enforcement
remain in their existing tickets; studio acceptance does not replace those gates.

Model selection is staged under #7: read-only discovery (#131), explicit immutable
registration (#133), then recipe selection and generalized worker verification
(#134). Registration is now available independently of recipe saves; its
[implemented contract](rust-http-api.md#explicit-model-registration) defines replay,
offline reads, storage bounds and the additive ledger change. The first #134 slice
now selects registrations in drafts, validates capabilities/settings and saves
immutable revisions offline. [Its contract](rust-recipes.md#registered-model-selection)
keeps base provenance separate from the selected authority. The execution slice
now verifies compiled reviewed registrations through preview, admission and
worker dispatch. Local Qwen development evidence is separate from the original
GPT-OSS acceptance gate; full two-model native acceptance remains under #134.

## First recipe authoring slice

The first delivery, [#113](https://github.com/aortez/playtime/issues/113) under #108,
is deliberately narrower than the full editor:
clone an experiment recipe, edit its display name and direction prompt, save an
immutable revision, and launch it from the workbench. Model, sampling, environment,
protocol, seeds, harness settings, and budgets are inherited and read-only. Protocol
and budget editing remain subsequent work under #108; other controls stay in their
own functional tickets. This slice does not add a model-call cap.

### Persistence and identity

Use the existing private service SQLite database with catalog tables owned by a
dedicated catalog module. Keep database work bounded and off async/render loops.
A schema migration must preserve service identity, admissions, confirmation data,
and existing private-file/durability rules. Catalog saves do not acquire the run
launch lock or perform model/workshop inspection.

Distinguish the logical recipe from its immutable revisions. Each revision stores
the complete inherited definition and seed contents, a canonical content digest,
its source provenance, and an opaque execution recipe ID. The logical recipe's
head is only a browsing/editing convenience. Existing recipe IDs remain valid;
new execution IDs must not collide with built-in or file-backed catalog entries.
Never infer an execution revision from a mutable display name.

A save transaction creates the revision, advances the head if its expected prior
revision matches, and records the keyed save result atomically. Identical replay
returns the original result before checking today's head/source. Reusing a key
for a different request fails. Two distinct edits from the same head cannot both
advance it; a conflict retains the user's unsaved text for explicit resolution.

The first clone snapshots the source definition and all inherited input bytes
inside the save transaction. A source digest mismatch refuses the save. Further
edits inherit from the saved revision, without reopening source TOML or seed paths.
Saved revisions load without the original `--recipes` directory. An explicitly
supplied invalid source catalog still fails startup rather than being ignored.
Definitions contain a format version and explicit versions of the existing
memento/model/environment identities, but do not require building their general
selection registries first. Saved identities must still match the supported
worker/model/environment at execution; a save never refreshes them.

Apply existing name/prompt/seed bounds and add tested finite catalog row, revision,
and total-byte limits. Capacity exhaustion refuses a new save, not an old read or
launch replay. No deletion or garbage collection is exposed in this slice; saved
revisions and save replay records are retained. A transaction rollback must not
leave an orphan revision or a moved head. Archives retain their own copied inputs.

### Authoring without execution readiness

Loading, reading, cloning, and editing saved definitions must work when the model
or workshop is unavailable, including at service startup. Catalog initialization
must be independent of live-adapter readiness; an unreachable Ollama endpoint,
missing registered model, unavailable Docker daemon, or absent workshop image
leaves execution unavailable without preventing recipe authoring. Configured
definitions remain listed when readiness changes.

Separate structural definition validation, execution compatibility, and current
readiness. Saves validate the supported definition format, edited fields, inherited
content and identities, and catalog bounds without live inspection. They preserve
registered identity mismatches or unsupported execution components as execution
limitations; they never refresh locks, substitute components, or erase the draft.
Unsupported definition formats cannot be edited by pretending to understand them.
Catalog corruption, invalid private credentials, and an explicitly supplied invalid
source catalog remain errors, not reasons to invent an empty or healthy service.

The definition read returns stored settings, identity records, and seed metadata
without requiring a resolved execution profile. The workbench can therefore show
the configuration even when preview fails. A preview that cannot establish
compatibility/readiness returns a typed `execution_unavailable` error with bounded
reasons, and no usable preview token. The UI retains the saved definition next to
the limitation and keeps editing enabled. Do not fabricate worker bindings or a
launch-ready `Preview` just to populate the form. Unsupported mode/capability and
invalid-definition errors retain their distinct meaning.

When execution becomes available, obtain a fresh normal preview and require its
ordinary admission checks. Availability changes do not alter the definition's
digest or saved revision, and never trigger automatic launch. For an existing
accepted launch key, lookup/replay still precedes current readiness/busy checks;
unavailable execution must not hide a prior acceptance or cause a duplicate run.
New model registration may require live inspection; editing an existing saved
definition does not depend on adding that later workflow.

### API and client flow

Add capability-advertised operations under the existing authenticated `/api/v1`
boundary, with strict new DTOs and reviewed OpenAPI fixtures:

| Operation | Proposed route and behavior |
| --- | --- |
| Read a clone source | `GET /recipes/{execution_recipe_id}/definition`: bounded editable fields, read-only settings, source digest, and seed manifest |
| Save a clone | `POST /saved-recipes`: source execution ID/digest, display name, prompt, and `Idempotency-Key`; the server inherits all other fields |
| Save another revision | `POST /saved-recipes/{recipe_id}/revisions`: expected head, display name, prompt, and `Idempotency-Key` |
| Read saved recipes | `GET /saved-recipes` and `GET /saved-recipes/{recipe_id}`: paginated summaries and current head metadata |
| Read revision history | `GET /saved-recipes/{recipe_id}/revisions`: paginated immutable execution handles and provenance |
| Recover a save | `GET /recipe-saves/{idempotency_key}`: the committed result bound to the service and original request |

Use 201 for a new save and 200 for its replay; return typed conflict errors for
changed sources/heads or mismatched key reuse. Invalid requests do not consume a
key. Successful saves remain recoverable after lost replies. Save keys are scoped
to the recipe-save operations and cannot resolve a launch. Bound transaction
completion independently of an HTTP client disconnect.

Keep the current launch DTO and receipt path: the saved revision's execution ID
is its `recipe_id`. Existing recipe preview, `POST /runs`, and launch lookup then
operate on that exact revision. List saved heads alongside existing catalog items
without exposing every old revision in the main picker. Older services omit the
new capability; clients disable authoring with an explanation while preserving
ordinary launches.

Saved-head pagination uses stable logical-recipe keys, never the current head's
execution ID, name, or modification time. Assign each logical recipe a persisted,
monotonically increasing creation sequence on its first save. Order saved recipes
by `(creation_sequence, logical_recipe_id)` and carry the first page's maximum
creation sequence plus the last returned key in the cursor. Later revisions do not
change that key; later-created recipes are excluded until a fresh traversal. The
creation sequence is a database ordering value, not a wall-clock timestamp.

Read each page and its head revisions consistently within a database read
transaction. Each row reports the head observed for that page; this is stable
membership/order, not a snapshot of every head across all pages. Bind a cursor to
the service, listing kind/filter, and format version. For the combined picker,
use stable source/logical-recipe keys and a source-catalog generation as well as
the saved-recipe creation cutoff. Changing the file-backed/built-in catalog
invalidates that traversal explicitly; saving a new head or changing readiness
does not. An invalid cursor requests a refresh, never silently starts page one.
The current paginator's execution-ID ordering must not be reused for saved heads.

Versioned listing metadata must expose logical identity separately from the head's
execution handle so the UI can maintain a stable row identity. Selecting a row
pins its observed execution revision. A later save or refresh may show a newer
head, but cannot replace that selected revision or its pending preview. Revision
history likewise uses immutable per-recipe ordering with an initial high-water
mark. Cursor retries may observe newer head values for the same logical rows;
clients reconcile those rows by logical identity without duplicating them or
retargeting a selected execution.

The shared client saves a private pending-save receipt containing service identity,
key, and exact typed request before sending. Lookup/replay resolves ambiguity with
that same request; a 404 is not permission to mint a fresh key automatically.
Acknowledged save replies must match the request and saved revision before updating
UI state. This extends receipt patterns without conflating recipe saves with runs.

### Preview and execution binding

`Archive::validate_preview` and `Archive::prepare` currently resolve the catalog
recipe separately. Both must resolve a saved execution handle to the same immutable
row, never the logical recipe's newest head. The preview digest binds the definition,
input bytes, effective profile, worker bundle, and operational bindings. Admission
and preparation must preserve that binding; changed worker/model/environment
identities refuse execution rather than recomputing a different profile silently.

Freeze a manifest-bound copy of the saved definition and provenance alongside the
effective scenario. Validate that the executable scenario actually matches this
definition; an extra JSON file alone is not evidence that its settings executed.
The archived prompt, inherited seeds, model, environment, budgets, and harness must
agree with the reviewed revision. Keep the existing owned prepared writer and
dispatch barriers. A future catalog edit cannot affect a reserved or running run.

### Workbench and acceptance

Provide Clone, name/prompt editing, Save, and Save revision. Show inherited settings
as read-only, mark unsaved changes, and keep save status separate from launch status.
Launch uses a saved and previewed revision; do not auto-save edits on launch or
silently discard them when changing selection. A stale reply cannot overwrite a
newer draft. Closing the workbench preserves saved revisions and pending-save
receipts, but unsaved text need not survive process exit in this first slice.

Acceptance covers the complete path:

- Clone curiosity, change its name/prompt, save, reopen the UI, preview, and launch
  without restarting the daemon. Restarting the service between runs also preserves
  revisions and keyed save results.
- Save revision B after previewing revision A, then launch A and prove A's exact
  prompt and inherited inputs execute. Editing/removing the original source files
  must not retarget the saved recipe. Record provenance independently of the name.
- Exercise concurrent saves, source changes, duplicate keys, lost replies,
  transaction interruption, database capacity, invalid input, and old-service
  behavior using the actual catalog store and API/client fixtures.
- Save new heads for both already-returned and not-yet-returned recipes between
  page requests. Traverse with small item/byte limits and assert every eligible
  logical recipe appears once. New recipes appear only after refresh. Cover cursor
  retry, source-catalog invalidation, service/filter mismatch, revision-history
  growth, and selection pinned to an older observed revision.
- Start the service with saved recipes and unavailable model/workshop execution,
  without the original source catalog. Read, clone, edit, save, and recover a lost
  save reply successfully. Use real catalog persistence and fake readiness ports
  to cover unreachable Ollama, missing model, unavailable Docker, and absent image;
  prove no inference, model loading, or image preparation occurs during authoring.
- Show the saved configuration and typed preview limitation while editing remains
  available; refuse new launches and preserve accepted-key replay. Restore readiness
  and obtain a fresh preview without changing the saved revision or auto-launching.
  Verify source-catalog/private-data corruption still fails rather than being
  masked as ordinary execution unavailability.
- Test the admission-to-preparation race against a newer saved head. Check archived
  definition/scenario agreement and rejection after model/environment/bundle drift.
- Preserve existing catalog behavior and baseline goldens. Review editor screenshots,
  keyboard/narrow-layout behavior, and run the boundary gates from development.md.
  Native evidence must use the real worker path; fake HTTP fixtures alone do not
  establish that the saved prompt executed.

## Model settings authoring slice

After #119/#113, [#5](https://github.com/aortez/playtime/issues/5) extends the same
immutable save/preview/launch workflow. It keeps the locked model and existing
memento harness. Main controls are reasoning (Model default, Low, Medium, High),
temperature and context capacity. Ordinary-response and memento-handoff caps
are separate from context and the inherited whole-run generation budget.
Advanced controls expose top-p, top-k, repeat penalty and the numeric sampling
seed, distinct from seed files. Existing reviewed bounds remain: top-k 1–1,000,
repeat penalty >0 and <=2, context <=131,072 and each output cap <=16,384.
Whole-run budgets, protocol and session counts remain #108 follow-up work.

Resets restore the selected revision's settings at editor entry; differences are
visible before saving. No field silently adjusts another. Save and preview check
initial context fit using the controller's prompt construction and reserve
calculation, without inference. An impossible first work request is refused with
an explanation. Estimates are conservative, not tokenizer or GPU-fit promises.
Runtime remaining-budget clamps and per-request context enforcement remain.

Capability resolution is scoped to the complete locked model/backend identity,
with reviewed reasoning choices and a context ceiling from locked metadata.
Model default omits the request setting and does not claim a known default level.
New settings revisions retain capability provenance; #7 generalizes the resolver.
Settings remain editable offline. Existing saved recipes/receipts remain readable
through additive fields; this slice needs no database migration or reset and does
not establish a general development compatibility promise.

## Validation and completion

Follow [development gates](development.md) for each changed boundary, including
the frozen Python suite. Preserve its fixtures by adding versioned files when
necessary. Deliberate Rust behavior/schema changes require reviewed goldens and
a compatibility note, not automatic regeneration.

Controller tests must cover context thresholds and oversized exchanges, repeated
summaries, tool pairing, summary failures and partial output, insufficient budget,
no-progress compaction, and cancel/deadline races. Model tests must cover unsupported
capabilities and identity drift. API/client tests must cover revision conflicts,
bounded input import, stale previews, immutable inputs, and lost mutation replies.
UI checks include both harness editors, incompatible configurations, compaction
inspection, comparison, and reviewed screenshots.

Native gates remain separate evidence from fixtures. Run live-model validation one
run at a time, after checking for active workers, and retain failed attempts. No
study or automated repeat loop is needed to validate the manual workflow. Finish
with an owner-run walkthrough that clones a recipe, changes a model or harness,
launches from the UI, inspects a compaction, and compares two preserved runs.

Completion means that manual workflow works through the common API with frozen
inputs and existing enforcement guarantees. It does not mean daemon recovery,
external harness execution, or unattended studies have been delivered.
