# Continuous compaction: native execution

#110 connects the shared `continuous_compaction_v1` controller to the packaged
worker, versioned archive, saved recipe API and workbench. Existing catalog
recipes keep their memento harness and reviewed allocations. Clone or revise a
saved recipe to select the continuous harness explicitly.

## Recipe and workbench workflow

Choose a catalog recipe with a definition (for example **Curiosity**), then
**Clone recipe**. Select **Continuous conversation with compaction** in the
Harness selector. Edit the context trigger, recent complete exchanges, summary
limit and compaction instructions, then **Save revision**. Clear **The model can
end the run with finish_run** to make the run last until its time or token limit.
Preview shows the active harness, shared model/budgets and effective protocol.
Launch uses that exact immutable revision through the normal durable launch
receipt.

Defaults are 75%, two retained exchanges, 2,048 summary tokens and the versioned
summary instruction template. The summary cap must fit the selected response
cap; initial work must fit with the controller's reserves. Saving rejects
impossible combinations, including prompt-only edits with inherited settings.
The trigger is an upper bound. Each work request must also leave the work
reserve: the safety margin, summary cap, instruction message and output limit.
The summary after the next exchange must fit too, and that check estimates the
request from its bytes. Either check can fire first. In the saved-recipe gate's
16,384-token context, the summary check starts compaction near 5,300 projected
tokens, well before the 75% trigger at 12,288. With Curiosity's 65,536-token
context and an 8,192-token summary cap, the reserve keeps projected work below
37,336 tokens, 57% of context.
No model, Docker or workshop connection is needed to read or edit saved
configurations. Preview and launch still require those execution capabilities.

The model can correct its own tool-call mistakes. A batch with an invalid
ordinary call runs none of its calls and returns each call's error to the model.
When the server can't parse a response's tool call, the request is retried, up to
five times in a row, and the failed attempt is charged its full allowance as
unknown usage. A work request is retried with a notice. A summary request is
planned again and sent unchanged; gpt-oss has answered one with a tool call even
though summary requests offer no tools. A response that calls no tool is
followed by a notice to continue or call `finish_run`, or, when the recipe leaves
that tool out, to continue until the budget runs out. A malformed or mixed
finish, a batch over the result allowance or over eight calls, and any other
summary failure still stop the run.

Definition schema 3 adds `compaction: { settings, protocol, inventory }` to the
frozen source recipe. Selecting the harness freezes its own protocol/inventory;
the old memento instructions do not go into continuous model requests. The
protocol ends with the recipe's wall time and generated-token budget and says
that `finish_run` ends the run early. With `allow_finish_run: false`, a separate
protocol says the run lasts until a limit and never names the tool. Work
requests then offer only `exec`, `read_file` and `write_file`, and a `finish_run`
call gets the ordinary unavailable-tool error. The inventory describes one
workshop that lasts the whole run. It ends with the recipe's workspace, entry
and `/tmp` limits, then the time after which the bridge stops a command
(`command_seconds` less the 3-second return allowance) and the `output_bytes`
that one response's tool results share. It suggests running longer jobs in the
background with their output in files. Both texts are written at selection, and
the protocol again when the finish choice changes; other setting changes keep
the frozen protocol. Clone and revise don't change those limits, so the frozen
texts stay accurate.
The task,
seeds, model, sampling and common allocations remain inherited. Legacy session
allocations/instructions remain inactive source data, enabling an explicit
switch back to memento; they are not synthetic sessions or limits on continuous
execution. The compatibility `Preview.parameters` field retains those source
values; `effective_configuration.config.parameters` is the active common
allocation. Clients cross-check it against the common projection.

Clone/revise accept optional `harness` and `compaction` fields. Omitting both
inherits the source. Selecting continuous installs defaults only when switching
from another harness; it preserves existing continuous settings. Compaction
settings with an active memento harness are refused. The
`recipe_compaction_settings_v1` feature advertises support, and
`recipe_compaction_finish_v1` advertises `allow_finish_run`; clients refuse to
send `false` without it. That setting defaults to true and is omitted when true,
so earlier definitions keep their bytes. No ledger version, migration, data
reset, model lock or workshop image change is needed.

Readable traces group work and summary calls by explicit request identity, keep
compaction plans/applied summaries separate, and distinguish completion claims
from terminal outcomes. Each compaction plan shows both the work and summary
request estimates. Rejections show their reason and detail, a response that
reaches its token limit says so, and the controller's stop reason and
diagnostics appear where it stopped. **Inspect source** retains exact before/after
conversations, request links, model output and estimate bases; externally
retained observations offer verified payload loading. The run header separates
work/compaction accounting without turning unknown usage into zero.

**Files → Load final workspace files** lists retained files, final tar and capture
index from the published archive. Open text/images or download verified content
through the existing inspector. The exact index records unavailable files too.
The search is bounded to 12,800 artifacts and 4 MiB of prepared metadata;
reaching either bound marks the listing incomplete. There are no intermediate
workspace snapshots or memory checkpoints. Richer occupancy visualization and
comparison remain #111.

## Preparation and ownership

`ContinuousProfile::freeze_definition` reconstructs a saved recipe's exact active
configuration and selected model identity. `NativeProfile` dispatches preparation
to the correct family; a memento profile rejects a continuous definition. The
capability digest includes the continuous recipe format, default input texts,
tool-call and summary recovery and the finish choice.

For the separate fixed native smoke, `ContinuousProfile::freeze` produces
`docker_continuous_compaction_v1`. Its validator reconstructs the exact supported
template, including seeds, tool responses, limits, helper/image identities and
model settings. The worker bundle's native capability digest includes that
template and the reviewed model lock.
`VerifiedBundle::prepare_continuous` checks capability compatibility before
freezing the archive; worker admission checks the profile family, manifest v2,
executable, mode, allocations and backend identity again. The test-only workspace
probe is still not a supported production worker profile.

This fixed smoke profile allocates 300 seconds, 16,384 generated tokens, a
16,384-token context, 512 tokens per model call, 45 seconds per inference,
2,048 retained tool-output bytes and 16 MiB of log evidence. It uses the policy's
75% trigger, two retained exchanges and a 512-token summary cap. Short smoke
instructions request a summary of at most 300 characters; this is task text,
not an additional truncation rule. General policy defaults remain unchanged.
Live work and summary requests use the same reviewed Qwen3:1.7b identity and
settings, with reasoning off and the context allocation frozen explicitly.

The worker acquires one volume, holder and run-scoped agent through the existing
private ownership journal and exact identity checks. `start_workspace` carries
no session identity and cannot restart after quiescence. Compaction performs only
model inference: it does not stop, capture or replace the workshop. The absolute
work deadline never renews. Finalization stops the agent, captures the workspace,
publishes through the owned writer and cleans up. Intermediate workspace captures
are unavailable. A completion declaration is separate from capture/publication
and cleanup success; lost receipts never authorize a final capture.

## Archive and read contract

The input manifest and execution records use the existing v2 family. The archive
retains settings, exact model requests/responses, native completion metadata,
before/after conversations, summary content, estimates and their bases, and
purpose-specific accounting. Ollama's first terminal `done_reason` is forwarded
without inference. Only `stop` can establish normal summary completion; missing,
partial and `length` outcomes cannot replace active history.

`compaction_planned`, `compaction_applied` and `completion_declared` observations
include `evidence.schema_version = 1` links. Each source names a request ID,
purpose and optional observation location (controller sequence, archive sequence
and exact record hash). Plans link their older and retained work requests plus
the preceding summary request. Replacements link the summary request and retained
plan; completion declarations link their work request. The full request/response
stream remains available through those request identities. Locations identify
bytes; they are not publication receipts.

Storage checks the links against admitted run-scoped requests and retained
observations, including externalized payloads. Missing source/plan locations
remain null rather than pointing to unrelated evidence. `compaction_history`
pages over a bounded archive window and returns recorded or explicitly omitted
entries. Its cursor advances over all records, so a page with no compaction rows
can still have a next page. Callers must follow `has_more` and
`next_after_sequence`, not infer completion from an empty result.

## Native gates

Use the locked Docker environment, user systemd and a private short temporary
path. Run one live check at a time, after confirming no Playtime worker is active:

```sh
TMPDIR=/home/data2/pt ./scripts/test-rust-continuous.sh --backend docker-scripted
TMPDIR=/home/data2/pt PLAYTIME_LIVE_ENDPOINT=http://127.0.0.1:11435 \
  ./scripts/test-rust-continuous.sh --backend live
```

The live endpoint must be an IP literal and expose the exact reviewed local Qwen
identity. See [local Ollama setup](local-ollama-development.md). This additional
gate does not replace the original GPT-OSS service run-and-view gate.

Both modes use the static bundle and real systemd supervisor, authenticated Begin,
Docker workshop, controller and archive writer. The seeded probe launches a
background counter and advances 48 stages, checking the same live process each
time. Its final result requires that counter to have advanced. The gate requires
at least two accepted compactions, a sole final capture containing the result,
reconciled summary usage, no unknown usage, and exact absence of all three owned
resources after cleanup. It compares the final offline read frontier with the
worker's authenticated receipt; the offline reader itself grants no authority.

Every attempt retains its printed evidence directory, frozen inputs, archive,
accounting and compaction projections, worker status and cleanup observations.
The first live development attempt used an ambiguous task and repeatedly supplied
a filename as `cwd`; it completed with zero compactions and correctly failed the
gate. A second attempt used valid arguments but declared completion after only
three probe stages, also failing the gate. The task now specifies one call per
response, exact arguments, and explicit incomplete-stage feedback. Neither failed
attempt is treated as continuity evidence.

Development evidence on 2026-10-26 (UTC), retained under `/home/data2/pt/`:

| Attempt | Result |
| --- | --- |
| `pc.gpQZCO0w` | Live: incorrect working directory, zero compactions; rejected by the gate |
| `pc.VaDmlptc` | Live: premature completion after three stages; rejected by the gate |
| `pc.ikHca3pe` | Live: seven compactions, then incomplete archive without a terminal publication; exact cleanup verified. Cause unconfirmed; retained as failed evidence. Worker diagnostics now report execution/publication failures to the journal. |
| `pc.8vwoWKHH` | Live: seven compactions, 51 work calls, 1,882 generated tokens (105 for summaries), zero unknown usage, final capture and all three owned resources absent |
| `pc.ugoZHQMB` | Scripted Docker: 15 compactions, 49 work calls, 2,048 generated tokens, final capture and all three owned resources absent |

CPU regressions cover malformed source links, missing/truncated completion
metadata, failed summary and cancellation finalization, lost capture receipts,
changed native profiles and uncertain workspace starts. The existing controller
golden covers policy decisions. Native success does not establish restart,
reattachment, replay or archive continuation.

## Saved recipe API/workbench gate

```sh
TMPDIR=/home/data2/pt VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json ./scripts/test-rust-registered-model.sh --continuous
```

This opt-in gate registers the existing local Qwen identity, clones a source with
the reviewed native probe's common budgets, selects compaction and freezes its
settings. It launches/replays through the API, edits the trigger through the real
workbench model/backend, and launches a distinct immutable revision from the UI.
Each run must apply at least two compactions, report matching purpose accounting,
publish exactly one final workspace containing verified `continuity-ok`, and
finish with complete cleanup. Frozen preview/archive settings must match. The
gate retains failed attempts and captures UI screenshots on the pinned renderer;
it does not establish daemon restart/recovery or task quality for other models.
