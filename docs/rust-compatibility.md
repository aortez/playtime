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

## Contract notes

Rust references use opaque IDs rather than exposing path semantics. HTTP, event
and input-manifest versions are independent. New manifests hash exact stored
input bytes and hash the manifest's own stored bytes separately. Worker
executable identity is independent of service/viewer builds; archived source and
dependencies alone do not establish executable identity or guarantee a
bit-identical rebuild.

Limit DTOs name explicit byte units, and `cpu_cores` retains fractional CPU
values without rounding. Full model/recipe fields, preview revisions and
overrides remain in their owning issues.

## Rust CLI command map

`target/debug/playtime` (or `cargo run --locked -p playtime-cli -- …`) is the Rust
API client. `./playtime` is the frozen Python launcher. The
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
