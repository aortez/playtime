# Rust migration compatibility matrix

Baseline: `ad7a2f2a5a6947d396f601a43fe622716e41200c`, including the committed
memory experiment and interrupted-pilot exit behavior. #17 provides shared types
and fixtures; #18 adds the bounded controller and deterministic simulation.
**The operational Rust commands below remain pending.** The existing Python
commands remain supported during development. See [controller behavior and
explicit race rules](rust-controller.md) for the implemented simulation scope.

| Current operation | Behavior to preserve | Rust delivery / deliberate changes |
| --- | --- | --- |
| `prepare RECIPE [--refresh] [--image]` | Reuse the locked image or verify/restore its archive. An explicit refresh creates new locks/image identity. Inspect the installed model; never download weights. Keep old archives. | #23/#24, CLI #21. Image rebuild remains an explicit operator action. |
| `check RECIPE [--offline]` | Validate local inputs and locked image; ordinary check also verifies model digest/definition/backend version. No inference or lock refresh. Python can restore the exact archived image during this check. | #23/#24. Explicit image restoration must be distinguished from read-only preview/readiness; test the final check behavior before cutover. |
| `smoke RECIPE` | Scripted two-session run using real Docker; tools, timeout, workspace persistence, fresh processes/tmp, report, and cleanup are checked. No model required. | #24/#26. A fully simulated Rust run is an additional, explicitly labeled mode and does not replace this integration check. |
| `smoke RECIPE --live` | Same small persistence task through the locked model with declared smaller session/token/time caps; retain failures. | #23/#24. Live validation waits for study/GPU availability and an agreed bounded recipe. |
| `run RECIPE` | Freeze inputs/controller/identities; one active run per data root; fresh conversations, bounded work, conservative unknown usage, checkpoints and terminal records. | #18–#21/#23/#24. All Rust launches belong to the service and a separate worker; CLI disconnect no longer owns run lifetime. Explicit cancellation uses the API. |
| `repeat RUN [--sampling-seed N] [--memory-condition …] [--endpoint …]` | Start from verified archived **initial** inputs. Keep allowed overrides explicit and archived. Endpoint relocation retains identity checks. Changes to recipes/prompts/seeds in the checkout do not affect the repeat. | #19/#27. Python exact repeats use the archived Python controller. Rust is a new controller baseline; Rust repeats verify its frozen executable, inputs, and platform. |
| `report RUN` | Standalone HTML, unknown usage and partial records visible, full event disclosure, memory/evidence/file changes and supported inline images. Escape model text/filenames; HTML/SVG remain source. Regeneration is derived output. | #19/#26. Read supported Python archives without modifying evidence or requiring conversion. Keep export usable offline and remotely. |
| `serve [--port N]` | Current loopback dashboard offers recipe preview, launch, live observation/stop, history, artifact access, archive/latest links. Detached workers survive browser/service restart. | #20/#22/#25/#26. Rust daemon plus native egui client replace the browser control UI. `/api/v1` is a new API, not legacy browser-route compatibility. Portable HTML reports remain. Authentication also protects history/streams/artifacts; initial remote transport is an SSH tunnel. |
| `pilot-prepare RECIPE` | Freeze six slots, pairing/conditions/seeds, order, all inputs and aggregate allocations under one controller baseline. | #19/#27. A Rust plan has a new controller identity. Existing frozen Python plans retain their controller. |
| `pilot-run PILOT` | Execute frozen slots in order, retain failures and unknown usage, never retry a started slot. After interruption, only eligible unstarted slots may proceed. Interrupted execution exits 130. | #20/#21/#27. Durable slot admission/idempotency ledger; test lost responses/crashes without launching another live study. |
| `worker RUN_ID` (internal), global `--root` | Run the frozen controller against its designated artifact store, reverify inputs, own the lock, and retain terminal diagnostics. | #19/#20/#27. Internal `playtime-worker` executable with separate identity; service clients use IDs/API, not shared filesystem paths. |

## Historical routes and intentional contract changes

Historical exact repeats continue through their frozen controller, for example:

```sh
python3 runs/<run-id>/inputs/controller/playtime --root "$PWD" repeat runs/<run-id>
```

Do not relabel a Python archive as Rust or interpret Python canonical-JSON hashes
using Rust serialization. New manifests hash exact stored input bytes and hash
the manifest's own stored bytes separately. Worker executable identity is
independent of service/viewer builds; archived source and dependencies alone do
not establish executable identity or guarantee a bit-identical rebuild.

Rust references use opaque IDs rather than exposing path semantics. HTTP/event/
input-manifest versions are independent. New event names, typed memory/cleanup
states, and usage fields are not a conversion of legacy log records. The legacy
generic stop reason `error` maps to `infrastructure_failure` in the new
vocabulary while retaining the original diagnostic. #19 must preserve the
legacy source reason when presenting normalized records.

The foundation's limit DTO names explicit byte units instead of Python's MiB
settings; adapters must convert exactly. `cpu_cores` retains fractional CPU
values without rounding. Full model/recipe fields, preview revisions, and
overrides remain in their owning issues. No operating recipe is changed here.

## Behavioral evidence for the controller port

| Behavior | Baseline evidence |
| --- | --- |
| `finish_run=false` ends a session; `true` finishes after a successful memento write. Oversized mementos do not get written. | `legacy/recovery.json`, `tests/test_budgets.py`, `tests/test_recovery.py` |
| At most two handoff requests, only `end_session`; ordinary JSON-looking text does not save memory. One reminder consumes existing allowance. | `tests/test_recovery.py` handoff tests |
| Reserve the dispatched cap before inference; absent final usage retains it, including across sessions. Later success never erases an unknown charge. | `legacy/recovery.json`, `tests/test_budgets.py`, `tests/test_recovery.py` |
| Measured context is reused only for an unchanged prefix; a fresh session starts estimation again. Reject oversized context before dispatch; retain bounded original tool output and record delivered truncation. | `legacy/context-seed.json`, `tests/test_context.py`, `tests/test_recovery.py` |
| Parser recovery consumes ordinary turns/budget, with consecutive and whole-run ceilings. Partial-response tools never run. | `legacy/recovery.json`, `tests/test_recovery.py`, `tests/test_ollama.py` |
| Default `self` has two initial messages. `self_plus_evidence` adds the exact bounded prior-session appendix as a third message, separate from MEMENTO.md and counted against context. Evidence selects deterministic observations, not correctness judgments. | `legacy/evidence.json`, `tests/test_evidence.py` |
| Sampling seed advances per request, wraps modulo 2³¹, and does not reset at session boundaries. Seed files are separate frozen inputs, including the first checkpoint comparison. | `legacy/context-seed.json`, `legacy/recovery.json`, `tests/test_core.py`, `tests/test_evidence.py` |
| Frozen pilots retain started/failed slots, verify membership and aggregate allocation, and return interruption status. | `tests/test_pilot.py` |

`legacy/` above is under
[`crates/playtime-protocol/tests/fixtures`](../crates/playtime-protocol/tests/fixtures/README.md).
`tests/test_rust_foundation.py` regenerates the normalized excerpts in memory
using the real Python controller with existing fake adapters and compares them
to the committed expectations. It also checks every synthetic content reference
against exact file bytes. It never reads a private run archive.

## Cutover gate

#28 closes this matrix with executable checks, tested revisions, installation,
remote operation, and rollback instructions. It must preserve all existing run
and image archives, refuse cutover while an old worker holds the shared data
root, and document remaining differences. Passing foundation fixture tests is
not evidence that a Rust worker can yet perform these operations.

## Rust CLI command map

`target/debug/playtime` (or `cargo run --locked -p playtime-cli -- …`) is the Rust
API client. `./playtime` remains the operational Python launcher until #28. The
Rust command/fixture gate is implemented; the real service simulation gate remains
#20/#42/#43. See [the native client guide](rust-cli.md).

| Python behavior | Rust CLI behavior |
| --- | --- |
| `run RECIPE_PATH` freezes and owns foreground execution | `run RECIPE_ID` uses a catalog ID, saves a keyed receipt, submits to the service, and detaches after acceptance by default; `--wait`/`--follow` observes without owning execution |
| Model adapter `--endpoint` is an Ollama endpoint | Rust `--endpoint` is the Playtime HTTP service, paired with `--token-file`; no model connection occurs in the client |
| Browser control and worker status through `serve` | `service status`, catalog/preview, runs, events, checkpoints, and artifacts use the authenticated versioned API; `playtimed` integration remains #20 |
| Stop the owned foreground experiment | SIGINT/SIGTERM only detach the Rust observer; use `runs cancel ID [--wait]` to request service cancellation |
| Inspect local run folders/reports | Rust inspects opaque run/session/artifact IDs through the API and verifies explicit downloads; it never opens the service's run folders |
| `prepare`, `check`, `smoke` | Reserved, exit 6 with #23/#24 ownership; simulation does not prove real Docker smoke |
| `report` | Reserved, exit 6; #26 |
| `repeat`, `pilot-prepare`, `pilot-run` | Reserved, exit 6; #27 |
| `serve`, internal `worker` | Reserved in the client, exit 6; #20 daemon/internal worker |

The shared resolver, durable launch-receipt format, cursor publication, and
verified-download helper live in `playtime-client` for both #21 and #22. CLI
parsing/presentation stay in `playtime-cli`. A CLI/client failure and an
experimental outcome have distinct exit classifications; unknown usage is never
converted to zero. No hidden Python fallback is installed.

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
new executing bundles verify it against embedded literals without advertising a
real-workshop execution capability.

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
