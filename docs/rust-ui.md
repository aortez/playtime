# Native workbench fixture milestone

`playtime-ui` is the first Linux egui/eframe workbench for the public HTTP API.
The Python launcher remains operational. This branch advances #22 against the
scripted service; it does **not** complete #22 or the production simulation gate.

The checkout starts from the shared-client revisions `da5f3a3` (acknowledged SSE
frontier consistency) and `e532b9d` (CLI/API integration), with #42's observation
mapping and durable driver merged through `a8a3dab`. All changes were made in the
existing checkout. No unfinished files were copied from `../playtime2`.

## Demo

In separate terminals, with a new temporary fixture root:

```sh
cargo run --locked -p playtime-server --example api_fixture -- --root /tmp/playtime-ui-demo --listen 127.0.0.1:0 --lifetime-seconds 600
cargo run --locked -p playtime-ui -- --connection-file /tmp/playtime-ui-demo/.playtime/connection.json
```

The backend label explicitly says fixture replay. This exercises authentication,
HTTP, SSE and verified content, without workers, Docker, inference or a model.
The `playtime-test-support` fixture used by the process gate also simulates an
accepted launch whose HTTP reply is lost. A production demo must use #20's real
connection descriptor after its worker/archive adapter is implemented.

To run the real workbench headlessly against the daemon's scripted simulation,
take screenshots and press controls by their accessible names, use
`./scripts/workbench-sandbox.sh start` and `scripts/workbench-drive.py`. Both keep
the display, D-Bus session and accessibility settings private to one directory.
The playbook is `.claude/skills/run-workbench/SKILL.md`.

Choose a recipe, review the frozen preview, then launch. The left pane contains
recent runs and unresolved launch receipts. The run list refreshes every 5
seconds and as soon as a launch is accepted. Lookup/retry retains the original
request and key. Failed recovery stays unresolved, including lookup 404/503,
invalid replies, rejected retries and local receipt errors. These failures do not
authorize a fresh launch; resolution requires a validated response for the
original request and a saved acknowledgment. A fresh launch that the service
refuses before admission (`run_busy` or `stale_preview`) has nothing to recover,
so its receipt is removed; a `run_busy` refusal names the run holding the slot.
The default Readable timeline groups #42 observations by stable
session/request/tool identities. Compact and Readable show each step once: they
leave out the archive records that repeat the observations (`run_started`,
`phase_changed`, `effect_intent`, `request_finished`, `command_finished` and
`session_ended`), and show `session_started`, `checkpoint_published` and
`run_finished` as one-line markers. A session marker says whether the session
received a memento, a checkpoint marker lists the files and images it saved and
which are new or changed, and the run's marker gives the outcome, cleanup and
tokens charged. Full lists every record as a card, as stored. Unknown records keep
their source-retained card in every view. Full and individual expansion reveal
recorded sections; source inspection pages exact JSON for every row, markers
included. Follow controls scrolling only. Expanding, collapsing or changing
disclosure keeps the toggled row, or else the top row, in place; moving between
Full and the other views asks the backend where that row is in the new list.
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
  cancellation, lasts only while that run is open. Other notices, such as a saved
  download, stay until the next notice replaces them.
  Preview refreshes preserve pending and unresolved launches; their replies remain
  applicable after preview changes without moving a newer run selection. Run
  snapshots reject older `updated_at` values and preserve terminal/cancellation
  facts even at equal timestamps. A cancellation reply installs a fully prepared
  view so the status, summary and source agree; no lifecycle outcome is inferred.
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

```sh
cargo test --locked -p playtime-ui
./scripts/test-rust-ui-e2e.sh --backend fixture
./scripts/test-rust-ui-e2e.sh --backend simulation
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 -m unittest discover -s tests -v
```

The fixture process gate builds once, discovers actual Cargo executables, then
uses #21's owned process support with fresh roots, port zero, isolated client
state, readiness deadlines, 30-second cases and a 180-second suite. It launches
through CLI and inspects through the desktop backend, then tests UI lost-reply
recovery across closing/reopening. Assertions use public HTTP, never service
archive files. Failure bundles allowlist diagnostics and redact credentials.

The simulation command intentionally exits **1** with the missing #20/#43 adapter
identified. #42 is merged, but production service supervision and worker-loss
recovery are not connected here. No fixture fallback or skip is permitted. Both
real simulation and fixture gates must pass before closing #22.

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
remain visible; screenshots are not evidence of real execution.

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
performance evidence; production worker/service switching and image-heavy workloads
still require the real #20 integration run before the whole #22 acceptance is met.
