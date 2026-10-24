# Native workbench

`playtime-ui` is the Linux egui/eframe workbench for the public HTTP API. It uses
the shared client with the production daemon or an explicitly labeled fixture.
The backend prepares bounded data outside rendering; the view model owns user
selection, pending mutations, and stale-reply checks.

For normal use, [go.sh](rust-mvp.md#run-it) builds and opens the workbench against
the daemon. The dedicated UI simulation gate remains incomplete because it also
requires restart/worker-loss recovery; it is separate from the working service
and Docker/live run-and-view gates in [development](development.md).

## Recipe authoring

A service with `recipe_authoring_v1` enables **Clone recipe** and **Edit recipe**.
The editor changes the name, prompt and supported model settings. Model identity,
whole-run budgets, protocol and seeds stay read-only. Model settings include
reasoning, sampling, context capacity and ordinary-response/handoff caps; reset
buttons restore values from when editing began and changes remain visible. Save creates an immutable revision and refreshes its execution preview.
**Refresh recipes** updates listed heads without replacing the selected execution
revision. Save receipts have their own recovery controls and persist across UI
restarts; unsaved text does not. Editing remains available when execution preview
reports unavailable Docker or Ollama. See [saved recipes](rust-recipes.md#saved-recipes-and-prompt-editing).

Reviewed editor views: [wide](../crates/playtime-ui/tests/snapshots/recipe-editor.png)
and [narrow](../crates/playtime-ui/tests/snapshots/recipe-editor-narrow.png).
The model settings form has separate [wide](../crates/playtime-ui/tests/snapshots/model-settings.png)
and [narrow](../crates/playtime-ui/tests/snapshots/model-settings-narrow.png) views.

## Installed model browser

With a configured Ollama endpoint, choose **Models** in the connection bar.
**Refresh installed models** reads a bounded inventory; choose an exact model
name to inspect its digest, definition fingerprint and capability evidence.
Equal-digest aliases are labeled. Installed state and advisory loaded state are
separate, and loaded does not imply GPU fit. Unknown reasoning/context support
stays unknown; remote/cloud models are rejected.

The browser never changes the recipe or its execution identity. Registration and
recipe model selection are subsequent work (#133/#134). A selected recipe model
that is absent remains visibly missing, without a fallback. Metadata failures
retain stale inventory with an explicit notice and timestamp. Refresh, close and
reconnect supersede pending replies; work runs outside rendering and observation
continues independently. Reopening refreshes; there is no background polling.

Reviewed views: [wide](../crates/playtime-ui/tests/snapshots/model-browser.png),
[narrow](../crates/playtime-ui/tests/snapshots/model-browser-narrow.png),
[missing selection](../crates/playtime-ui/tests/snapshots/model-browser-missing.png),
and [unavailable metadata](../crates/playtime-ui/tests/snapshots/model-browser-offline.png).
See the [metadata API contract](rust-http-api.md#read-only-ollama-model-discovery).

## Frozen preview and file inspection

The launcher groups its frozen preview into Prompt, Run limits, Model, Workshop,
and Seed files. Model settings and host identity, workshop resources, all controller
limits, and named seeds come from the selected preview, prepared outside rendering.
Absent metadata says unavailable; a fixture or scripted adapter says that it does
not perform inference/tool execution. Zero retries stays zero. Seed names and UTF-8
byte sizes come from the frozen recipe map, with at most 64 names shown. Contents
and full identities remain in the paged **Details · full frozen preview** source.
**Protocol** and **Workshop inventory** expand as wrapped text. Prompt, protocol
and inventory excerpts are bounded to 4 KiB each; source uses 24 KiB pages so
expanded excerpts cannot consume the frame text budget and hide source bytes. Launch still sends
the exact frozen revision, regardless of these display summaries.

The connection bar keeps backend kind and endpoint visible; **Backend details**
contains its description. Run rows lead with recipe name (or execution recipe ID
when its catalog entry is unavailable), recorded admission time labeled UTC, and a
secondary run ID. Run state has its own untruncated line, so a long recipe name
cannot hide `needs release`, including at the minimum sidebar width. This does
not infer the start of model execution.

The Files inspector offers **Open image** or **Open file**, with readable size and
file kind. Each available file's **Details** contains the full name, exact byte size,
recorded kind, digest and opaque artifact ID. Opening, source pagination and verified
downloads retain their original artifact selection and stale-reply checks.

Before archive anchors exist, the timeline says **Waiting for archive publication**.
Observation reads that receive `evidence_not_ready` retry the same run/cursor within
the existing 60-second budget; they do not label a healthy connection as reconnecting.
A transport failure still shows reconnecting, even if it follows an archive wait.
Concurrent reads cannot clear one another's active waiting state. Success clears it;
selection/reconnect invalidate old replies, and budget/fatal errors remain visible.

Reviewed synthetic live-profile metadata: [preview](../crates/playtime-ui/tests/snapshots/live-preview.png),
[narrow preview](../crates/playtime-ui/tests/snapshots/live-preview-narrow.png),
[expanded protocol and inventory](../crates/playtime-ui/tests/snapshots/live-preview-details.png),
and [narrow file inspector](../crates/playtime-ui/tests/snapshots/files-narrow.png).
These fixtures render frozen example values without running inference or tools.

## Fixture and headless development

In separate terminals, with a new temporary fixture root:

```sh
cargo run --locked -p playtime-server --example api_fixture -- --root /tmp/playtime-ui-demo --listen 127.0.0.1:0 --lifetime-seconds 600
cargo run --locked -p playtime-ui -- --connection-file /tmp/playtime-ui-demo/.playtime/connection.json
```

The backend label explicitly says fixture replay. This exercises authentication,
HTTP, SSE and verified content, without workers, Docker, inference or a model.
The `playtime-test-support` fixture used by the process gate also simulates an
accepted launch whose HTTP reply is lost. For production, use the real daemon's
connection descriptor, such as
`/home/data2/playtime/root/.playtime/connection.json` with `go.sh` defaults.

To run the real workbench headlessly against the daemon's scripted simulation,
take screenshots and press controls by their accessible names, use
`./scripts/workbench-sandbox.sh start` and `scripts/workbench-drive.py`. Both keep
the display, D-Bus session and accessibility settings private to one directory.
See the [workbench playbook](../.claude/skills/run-workbench/SKILL.md).

Choose a recipe, review the frozen preview, then launch. The left pane contains
recent runs and unresolved launch receipts. The run list refreshes every 5
seconds and as soon as a launch is accepted. Lookup/retry retains the original
request and key. Failed recovery stays unresolved, including lookup 404/503,
invalid replies, rejected retries and local receipt errors. These failures do not
authorize a fresh launch; resolution requires a validated response for the
original request and a saved acknowledgment. A fresh launch that the service
refuses before admission (`run_busy` or `stale_preview`) has nothing to recover,
so its receipt is removed; a `run_busy` refusal names the run holding the slot.
The default Readable timeline groups observations by archived session and model
request identity, with each call's tools indented underneath it. Calls use their
recorded number and explicitly label forced handoff requests. Each tool remains
an independently virtualized block, including when a call has many tools or its
parent request has not loaded yet. Compact uses the same associations; Full keeps
the existing source-oriented ordering and sections.

Readable shows recorded thinking and response text, commands with exit status and
separate output/error fields, file operations with their requested/read content,
and submitted mementos with the requested finish flag. Short content is visible;
long sections start with a 512-byte excerpt. **Show more** expands a block to at
most 16 prepared sections, each bounded to 4 KiB. **Inspect source** pages exact
records and retained payloads for continuation. Empty sections disappear; missing
fields, incomplete inference, not-dispatched actions and upstream truncation stay
explicit. A request with no result says it is awaiting a recorded result while
live, or has no recorded result after the run ends. A write result does not claim
checkpoint publication, and a submitted memento does not verify model claims.

Compact and Readable leave out the archive records that repeat observations
(`run_started`, `phase_changed`, `effect_intent`, `request_finished`,
`command_finished` and `session_ended`). Session, checkpoint and run-end records
remain concise markers. Unknown observations stay inspectable. Source IDs,
metadata and raw JSON are available through Details/Inspect source.

Timeline blocks size to their displayed content. A sparse cache retains at most
2,048 measured heights, with estimates for unmeasured blocks; only visible blocks
are laid out. Expansion, resize, late history and disclosure changes preserve the
reader's activity. Follow controls scrolling only; pausing it prevents later
records from moving the reader to the end.

The session chooser jumps to recorded session boundaries and pauses following.
It shows at most 64 entries in a scrollable popup, with Earlier/More controls for
longer runs. The list says when earlier history is incomplete; configured session
limits never create entries. The current session follows the visible activity.
Jump to live cancels pending navigation and resumes following. Reconnect restores
a paused position by its archived event sequence, independent of rebuilt row IDs.

At each session start, **Memory received** shows the exact recorded delivery,
including an explicitly available empty memento, no memento supplied, or unavailable
content. When both the controller observation and canonical start are present,
Readable shows the boundary once; Full retains both sources. A standalone canonical
start loads only its explicit delivery reference, never a previous workspace's
memento. Referenced observations remain inspectable even when their inline ordinal
or content is unavailable.

Published checkpoints show a stacked comparison of received memory and the
**Published memento**, plus a compact list of checkpoint files/images.
**Compare memory** and **Browse files** select that exact session in the existing
inspector. Background excerpt reads never change inspector selection. Submitted
mementos remain on their tool request; a request, successful tool result or controller
checkpoint intent does not create a published checkpoint. Reused memory remains
exactly as delivered, and an interrupted session gains no invented final capture.
File-change classification requires complete adjacent-checkpoint metadata and known
hashes; missing hashes or partial lists explicitly make classification unavailable.
Session controls describe the current memento harness, not a new harness contract.
Whole-run usage accounting is unchanged; this slice adds no per-session estimates.

The run header leads with progress against the run's frozen limits: the session,
the work turn within it, tokens charged of the token budget, controller time of
the wall-time limit, and what the run is doing, such as thinking, running a
command, handing off or saving a checkpoint. The limits come from the
`controller_started` observation. The transcript index updates this summary as
records arrive, apart from the payload working set, so eviction never changes
it. Until earlier history has loaded, a value appears only if live records make
it current. An ended run leads with its outcome, the sessions it completed and
the tokens charged, then cleanup. A session whose checkpoint the stop interrupted
isn't counted, and a failed cleanup names its first error. States, outcomes,
cleanup, evidence and accounting read as words with thousands separators, and
unavailable values stay distinct from zero.
The run header and list name the orchestration reason, so an operator release
reads "released by operator". A run the daemon stopped monitoring shows its
`playtime runs release` command, and Cancel is off once the worker has stopped.
Memory compares delivered/published sources; publication does not verify model
claims. Files are saved versions, addressed only by opaque content IDs. Downloads
require an explicit destination and use the shared verified, atomic download path.

At 1440×900 the three panes resize independently. At 900×600 the inspector becomes
a drawer and the run list can collapse. Escape closes enlargement/drawer; it never
cancels the experiment. Pending allowance/allocation and file change classification
are explicitly unavailable where the current API does not project them.

## Ownership and bounded work

- `model.rs` owns independent connection, selection, mutation and history states.
  Replies carry connection/selection/preview/inspection/launch generations and run IDs.
  Late replies cannot move selection, roll back lifecycle, or install old previews.
  A status notice about a run, such as its stream's end, a failed load or a
  cancellation, lasts only while that run is open. The launcher stops watching
  the run it left, and a reconnect there doesn't resume it. Other notices, such
  as a saved or failed download, stay until the next notice replaces them.
  Preview refreshes preserve pending and unresolved launches; their replies remain
  applicable after preview changes without moving a newer run selection. Run
  snapshots reject older `updated_at` values and preserve terminal/cancellation
  facts even at equal timestamps. A cancellation reply installs a fully prepared
  view so the status, summary and source agree; no lifecycle outcome is inferred.
- Inline boundary excerpts use at most two background reads, sharing the existing
  preview semaphore. Each memory source is verified through the shared client,
  limited to 256 KiB for preview, then excerpted to 2 KiB. At most 64 boundary
  views remain cached for the current window (under 512 KiB including metadata);
  offscreen results are discarded. Larger/exact sources remain available through
  the inspector. Publication sequence, connection, run and selection checks keep
  stale replies from replacing newer navigation or inspection.
- `backend.rs` owns one cancellable client runtime, one selected observer and at
  most two preview/decoder jobs. Private files, HTTP, JSON, hashes, comparisons and
  image decode stay outside egui callbacks. Closing the client cancels/joins its
  tasks, never the remote experiment. Only explicit Cancel emits cancellation.
  Initial attachment, fixed history pages, status polling and cold source reads
  retry transient failures with exponential backoff for at most 60 seconds per
  consecutive outage (including retry requests). Retries preserve the exact run
  and cursor. Successful reads reset their own budget; other failing reads keep
  the connection marked Reconnecting. Fatal errors require explicit Reconnect.
  Poll retries run separately so they do not stall history/live application.
- `transcript.rs` maintains the sequence index and payload working set. Live attach
  starts from the paired RunView anchor before independent earlier-history paging.
  Apply precedes acknowledgment. Repeated sequences must have identical content.
  Cold visible/source pages refetch independently while live ingestion continues.
  A final response replaces streamed text; unknown observations keep source data.
  Retained references resolve by matching published content metadata to opaque IDs,
  with an explicit bounded lookup and visible failure if not published/found.
- `progress.rs` keeps the run's progress from indexed records and words the run
  header. It holds the frozen limits, the count of completed sessions, the latest
  session's requests and the latest record's time and activity, so its memory
  stays bounded by one session.
- `ui.rs` renders prepared strings/pixels and emits intents. It virtualizes the
  timeline, caps expanded items and pages file metadata. No URL, file, HTML, SVG or
  script loader is installed. egui/eframe/egui_kittest are pinned to 0.34.3; image is
  pinned to 0.25.10 with only PNG/JPEG/WebP/GIF codecs.

| Resource | Enforced bound |
| --- | --- |
| Event inbox | 64 records and 4 MiB serialized bytes, backpressured |
| Transcript payload / index | 16 MiB / 32 MiB, conservative index accounting |
| Prepared timeline window / laid-out source | 64 rows / 64 KiB |
| Prepared session chooser | 64 entries per page; session index included in 32 MiB transcript bound |
| Inline boundary previews | 64 current-window entries; ≤512 KiB; 2 reads concurrently |
| Sparse timeline height cache | 2,048 measurements; ≤256 KiB |
| Apply work per frame | 100 prepared rows/replies or 4 ms, with deferred replies |
| Control queue / concurrent client tasks | 64 / 16 |
| Preview/decode jobs / live streams | 2 / 1 selected stream |
| Memory diff | 256 KiB and 4,000 lines per input; larger sources page separately |
| Static image | 4 MiB encoded, side ≤4096, ≤16 million pixels |
| GIF | 1024² canvas, 300 frames or 60 source seconds per explicit play |
| GIF pacing | incremental decode, UI acknowledgment, ≥34 ms between updates |
| Managed CPU pixels / GPU texture payload | 64 MiB / 64 MiB; one selected texture |

Missing memory remains unavailable, not an empty string or an unchanged diff.
Raw source has continuation controls. Image decoding checks dimensions before
allocation and configures decoder limits. GIFs start paused, never collect all
frames, and stop when deselected/hidden; replay is explicit. PNG/WebP animations
are labeled still previews. Transient decoder/download buffers are separate from
cache budgets, and `image` allocation limits are best effort, not an RSS guarantee.

Private layout/disclosure/identity-bound selection lives in `workbench.json` beside
shared client state, using the same protected-file policy. Launch/cancel intent and
a cursor alone are never restored. Reopening attaches from fresh paired anchors.

The 32 MiB index boundary stops observation visibly without acknowledging the
unapplied record. Segment navigation past that boundary is follow-up work; current
source refetch covers the indexed segment. The retained-payload inspector exposes
verified raw source; automatic rich reprojection of oversized referenced model
responses is also follow-up work. These limits are not hidden as successful
completion.

## Checks and fixtures

In addition to the [required workspace checks](development.md#required-checks-for-rust-changes):

```sh
cargo test --locked -p playtime-ui
./scripts/test-rust-ui-e2e.sh --backend fixture
```

The fixture process gate builds once, discovers actual Cargo executables, then
uses shared owned-process support with fresh roots, port zero, isolated client
state, readiness deadlines, 30-second cases and a 180-second suite. It launches
through CLI and inspects through the desktop backend, then tests UI lost-reply
recovery across closing/reopening. Assertions use public HTTP, never service
archive files. Failure bundles allowlist diagnostics and redact credentials.

The dedicated `./scripts/test-rust-ui-e2e.sh --backend simulation` command
intentionally exits **1**. Its diagnostic names the original integration issues;
normal production simulation and supervision now work through the separate
service gate, but the full restart/worker-loss recovery matrix is incomplete.
No fixture fallback or skip is permitted. See [gate scope](development.md#gates-that-are-not-implemented).

`tests/support/generated.rs` produces 100,000 records across 100 sessions, over
10,000 activity rows and under 64 MiB serialized input. It includes Unicode,
unknown kinds, streamed-to-final replacement and a 980,000-byte single-line
payload. No generated archive is committed. The behavioral/media/HTTP/widget tests
cover stale replies, deduplication, cancellation uncertainty, missing memory,
explicit codecs, corrupt/oversized images, animation limits and real named controls.
The process fixtures supply actual disconnect/lost-reply behavior.

## Reviewed screenshots

The separate `ui-snapshots` CI job pins Ubuntu 26.04 image digest
`sha256:c098726dd9b9733f3e885f8de0ef375206a311b0a317eadc40d347b1115fb944`,
Mesa `26.0.8-1ubuntu0.3`, LLVM `1:21.1.8-6ubuntu1`, and Vulkan loader
`1.4.341.0-1`. The selector requires the CPU Vulkan adapter with exact driver info
`Mesa 26.0.8-1ubuntu0.3 (LLVM 21.1.8)`; missing packages/renderer fail explicitly.
Fonts come from the locked egui default-font package. Viewport, scale 1, fake clock,
and paused first animation frame are fixed. No display server or inference GPU is
required; ordinary interaction tests do not create a renderer.

```sh
VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json cargo test --locked -p playtime-ui --features ui-snapshots --test snapshots
```

A missing/mismatched baseline writes `tests/snapshots/*.new.png`. Inspect each new
image and its diff before accepting it. After that review, the explicit update is:

```sh
UPDATE_SNAPSHOTS=1 VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json cargo test --locked -p playtime-ui --features ui-snapshots --test snapshots
```

Never update images just to make a check pass. Baselines cover launcher, readable
activity, memory/file image inspection, connection error/stale information,
900×600 layout, and the header of a running and an ended run. Their fixture labels
remain visible; screenshots are not evidence of real execution. The `trace-handoff`,
`trace-streaming` and `trace-long` pairs cover failed reads, an unpublished handoff,
provisional thinking and bounded long excerpts at both viewport sizes. Their
commands are synthetic data and never execute. The `session-memory` and
`session-interrupted` pairs show delivered/published memory with contextual file
controls, and a submitted handoff without a published checkpoint. These are
focused synthetic trace views, without the run-header fixture.

The #124 projection was also inspected against retained native live archive
`20261023T054921Z-728c128b`: five calls, their associated commands/end-session
requests, the verification output `persistence-ok`, and exact source access. This
was read-only review of an existing run, not new inference or recovery evidence.

The #125 projection was additionally checked against retained Rust run
`20261023T052240Z-b3bdbbc6`: ten recorded sessions and ten published checkpoints,
including unchanged memory delivered again after a handoff. Navigation and source
rehydration agreed with reverse history, and checkpoint memento bytes matched their
archived digests. No archived commands or new inference were executed.

## Performance measurement

```sh
cargo run --release --locked -p playtime-ui --example ui-perf
```

This headless 60-second release measurement moves through prepared windows,
changes disclosure/selection and ingests 100 generated live records/second after
indexing the 100,000-record fixture. It reports frame quantiles, peak Linux RSS,
cache maxima and laid-out rows. Network/disk wait and initial generation are
excluded from frame timings. Hardware timing is deliberately outside ordinary CI.
See [the recorded measurement](rust-ui-performance.txt). This is prepared-view
performance evidence, not an end-to-end execution measurement. Service and
Docker/live gates exercise real run inspection separately; the complete
restart/recovery matrix remains unimplemented.
