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
new executing bundles verify it against embedded literals without advertising a
real-workshop execution capability.
