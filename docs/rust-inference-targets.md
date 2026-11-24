# Named inference targets

Playtime can use multiple configured Ollama machines from one daemon. Choose the
inference target for each launch; the recipe still owns its exact model identity,
harness, prompt, sampling settings, context, seeds and budgets. The Docker
workshop stays on the controller machine. Selecting a target does not move tools
or workspace files to the inference host.

## Configure and launch

`./go.sh` builds and starts the workbench with the Bird/Framework example
configuration by default. Use `PLAYTIME_TARGETS=/absolute/targets.json ./go.sh`
for a custom configuration. `PLAYTIME_OLLAMA=http://host:11434 ./go.sh` retains
the legacy single-endpoint workflow; do not set both variables. An already
running daemon opened with `--no-build` keeps its existing configuration.

The normal workbench lists runs from its persistent main service root. Earlier
experiments may have separate histories: `./go.sh --no-build --history` lists
service roots under the data directory and opens the selected root in its own
workbench. `--history /absolute/root` also accepts roots elsewhere. Each keeps
its original service identity, ledger and archives; no records are imported or
merged. The selected history also receives any new launches from that window,
and the current daemon may upgrade an older ledger in place, after which older
builds refuse it. Open a copy to keep an evidence root unchanged.
Use the printed history-specific stop command to stop its daemon between runs.

Start with [`config/inference-targets.example.json`](../config/inference-targets.example.json).
It defines `bird` (RTX 4090) and `framework` (Radeon 8060S). Copy it to your own
configuration and adjust endpoints or add machines. The format is versioned JSON,
limited to 16 unique target IDs and 64 KiB. The providers are `ollama` and
[`strata`](#strata-targets). Names are display labels, not attested hardware
identities.

Rebuild the daemon, CLI/UI and frozen worker bundle together. The new worker
capability digest covers named-target execution; old bundles cannot execute it.
See [development](development.md#build) for the build commands. Then:

```sh
target/debug/playtimed --root /absolute/service-root --bundle /absolute/new-bundle \
  --docker-scripted --targets /absolute/inference-targets.json --recipes recipes

target/debug/playtime --connection-file /absolute/service-root/.playtime/connection.json targets
target/debug/playtime --connection-file /absolute/service-root/.playtime/connection.json \
  --target framework models

target/debug/playtime --connection-file /absolute/service-root/.playtime/connection.json \
  --target bird run live-two-session --mode live_smoke --idempotency-key bird-smoke-1 --wait
target/debug/playtime --connection-file /absolute/service-root/.playtime/connection.json \
  --target framework run live-two-session --mode live_smoke --idempotency-key framework-smoke-1 --wait
```

These two launches use the original locked GPT-OSS 20B smoke. For a saved recipe,
use its immutable execution recipe ID with `--mode experiment`. Selecting
Framework does not implicitly select a larger model: choose its reviewed model
registration in the recipe editor first.

The workbench offers an **Inference target** picker in the launcher and Models
window. Changing it invalidates the old preview and cached installed-model
inspection, while preserving recipe edits. The new preview and archived run
details identify the selected target. Models installed on another target are not
silently substituted. Loaded-state metadata does not establish GPU allocation or
context fit; use the retained [Framework test evidence](rust-framework-models.md)
for the combinations tested so far.

Named configurations require explicit selection. There is no automatic first
machine or failover. The existing `--ollama-endpoint URL` daemon flag remains
supported with its original unselected workflow; it is mutually exclusive with a
nonempty named configuration. Target configuration changes require a daemon
restart. Restarting does not migrate an active run.

## Identity and retained requests

`GET /api/v1/targets` lists configuration without contacting the machines.
`X-Playtime-Target` scopes inventory, inspection and registration review to one
configured target. With named targets, a metadata request without it is refused
as `invalid_parameters`; without them, those routes answer as before. Discovery
stays independent of workshop readiness. An offline target does not prevent
inspecting another target or editing saved recipes.

Previews and launches enable and check the shared Docker workshop one request at
a time. Each then checks its own target's model outside that gate, so a slow or
offline machine delays only requests for that machine. At startup the daemon
resolves every target's address concurrently. A target whose address doesn't
resolve then is resolved again on each later request for it.

Registration acceptance retains its existing service/endpoint binding. The daemon
finds the matching configured browser from that binding, and checks the exact
identity again. A binding that no configured machine has, such as a review from a
removed target, is refused as `model_metadata_changed` without contacting a host,
so the client can review again. A registration describes the model, not ownership
of a machine; the same exact identity can execute on either target where it is
installed.

Preview and launch bodies accept optional `target` IDs under the
`inference_targets_v1` capability. The target descriptor, normalized endpoint and
resolved addresses are frozen in `model.host` in the native configuration. The
preview fingerprint covers this configuration and the worker identity. Admission
and preparation check it again, so changing the target or endpoint invalidates an
old preview. Existing identity and pinned-address checks remain in force.

The selected target is also part of the retained launch request and idempotency
comparison. `launches retry` sends that retained request; a current picker choice
cannot redirect it. Replaying an already accepted request resolves the original
run before checking current target availability. Run inspection reads the target
from verified archived configuration and checks it against the accepted target
ID, rather than using today's configuration. Historical archives without a target
remain unlabelled; their endpoint evidence is unchanged.

Current admission and local live-run exclusion are unchanged. This feature adds
selection and provenance, not scheduling or exclusive ownership of remote Ollama.
The operator coordinates other clients such as Codex.

## Strata targets

A target with `provider: "strata"` names a [chat launcher](rust-chat-launcher.md)
profile instead of an endpoint:

```json
{"id": "framework-strata", "name": "Framework · Strata", "provider": "strata",
 "profile": "framework-gyro-m-fast"}
```

The daemon then needs `--strata-launcher /absolute/chat-launcher-package`, the
installed launcher package. It runs the launcher with `/usr/bin/python3 -I` and
`PATH=/usr/bin:/bin`, whatever its own environment. At startup it asks the
launcher to describe the profile's hold (`run-strata.py PROFILE
--describe-hold`), and the package's copy of the profile's lock must be byte for
byte one that this build compiled, now only `framework-strata-gyro-m-fast-v2`. A
target that fails this is unavailable, and each later request for it describes
it again. The target's model is its lock, so the model browser refuses the
target as `unsupported_capability`. The daemon advertises `strata_targets_v1`
when it has a Strata target.

So far a Strata target runs only the archived live smoke, `live-two-session`
with `--mode live_smoke`, which sends the lock's own settings; recipes there are
refused. Its preview freezes the lock and a template host without an endpoint,
so the preview revision doesn't depend on which server a hold lends later. The
preview reads the host's state through the launcher's read-only `--status`, and
it isn't ready unless the host is ready or starts on launch. Admission reads the
state again: a busy host is refused as `run_busy`, and any other state that a
hold would refuse as `model_unavailable`. Until this daemon's previous hold has
parked or stopped its server, the preview isn't ready and admission is refused
as `run_busy` too.

Preparation holds the host through `run-strata.py PROFILE --hold`. The launcher
starts or reuses the reviewed server, verifies its artifacts against the lock and
lends a loopback forward. The daemon checks the lock's name and digest, that the
verified host identity is the lock's and that the endpoint is loopback. Then it
inspects the server itself, which must serve the lock's model with nothing in
flight. It freezes the endpoint, the server's start identity and the verifier
report into `model.host`, and the worker refuses any other server process. A
hold may take the launcher's startup and verification bounds plus 75 seconds,
about 27 minutes for Framework's profile. Cancelling the run abandons a hold
that hasn't lent its host. A hold refused during preparation leaves the run
`preparation_failed` and waiting for a release, as any failed preparation does,
and no worker starts.

When the worker stops, the daemon ends the hold. Only a run that completed
normally, on the same server with nothing in flight, parks the model warm by
sending `release`. Every other ending closes the hold, and the launcher stops the
server. The daemon waits for the launcher's `ended` event and its exit, each
within the profile's cleanup bound plus a minute, then sends SIGTERM and waits 30
seconds more. It never kills a hold, which could orphan the hold's SSH session;
one that still runs is logged and left to finish. Each hold's launcher messages
go to `.playtime/strata-holds/RUN.log` under the service root. While a hold is
active or still ending, a release of its run is refused with the hold as a
remaining resource.

Stopping the daemon ends every hold and stops their servers, so a Strata run
doesn't survive a daemon restart: its worker's next request fails. A launcher
whose daemon dies reads EOF and stops its server too. The daemon ends its holds
together and waits for each, so a systemd unit that runs it should send the stop
signal only to the daemon (`KillMode=mixed`) and allow a stop timeout longer than
one hold's whole wait above, about 8 minutes for Framework's profile. Otherwise
systemd signals the holds' SSH sessions too, and the host's helper stops the
server only once it sees the lost session.

The machine-wide live-run lock still allows one live run at a time across
checkouts. Other clients of the same host, such as the chat console, hold it
through the same launcher, which refuses a second hold as busy.

### Strata gate

```sh
PLAYTIME_STRATA_LAUNCHER=/absolute/chat-launcher-package \
  ./scripts/test-rust-service-e2e.sh --case run-and-view --backend strata
```

This is the [live gate](rust-live-model.md#evidence) on a Strata target. It holds
`PLAYTIME_STRATA_PROFILE`, by default `framework-gyro-m-fast`, through a launcher
package that has hold mode and the profile's reviewed lock. Like the live gate,
it refuses to start while any Playtime worker unit is active, and it requires the
same agent completion, files, checkpoints, mementos, copied archive and Docker
cleanup. Another daemon root's admission must get `run_busy` while the run holds
the live-run lock. Every response must follow streamed deltas, and its request's
call record must have seen the server's terminal event. A response that generated
for 1.5 seconds or more, by the server's `predicted_ms`, must show text at least
300 ms before it ended, unless it only called tools. Strata reports none of
Ollama's durations, so the run's contention summary stays unknown and the gate
doesn't check it.

Before the workbench's launch, the gate waits until the first hold has parked the
server. At the end it stops the daemon with SIGTERM. Each hold must have ended
with its inference owner confirmed, and at least one completed run must have
parked the model. The daemon's stderr and each hold's launcher log are kept with
the evidence. Holding a cold host may take half an hour.

#### Native evidence, 2026-11-23

The gate passed on Framework's `framework-gyro-m-fast` profile, through a
launcher package staged from this checkout. CLI run `20261124T050557Z-d89eb9b3`
completed both sessions with 961 known generated tokens, no unknown usage, 137
records and complete Docker cleanup. Five of its responses generated for 1.5
seconds or more, and each showed text first. Workbench launch
`20261124T050711Z-1f8a8306` also finished `agent_finished`. The first hold
started the model cold, ready in 31.5 seconds and verified in 1.3 seconds. The
second reused the server that the first had parked, ready in 0.4 seconds. Both
holds lent the same server process and ended `released`, with the inference
owner confirmed. An earlier attempt failed before it reached the host: the daemon
looked up `python3` through its own `PATH`, which the gate points at a missing
directory. The daemon now runs the launcher with a fixed interpreter and `PATH`.
Evidence is retained outside version control.

## Validation

The HTTP contract tests use two explicitly fake Ollama metadata servers. They
check target routing, endpoint-bound review, missing-target refusal, offline
isolation and that metadata reads never call inference. UI state tests check stale
preview rejection and that switching targets cannot mutate an already queued
launch. Native acceptance must separately check the archived target and worker
execution on both machines. Fixture tests do not establish native execution or
restart/recovery behavior.

### Native evidence, 2026-10-27

Evidence and copied archives are retained outside Git at
`/home/data2/playtime/targets-1027`. Named bird run
`20261028T034019Z-d14e5352` and Framework run
`20261028T034029Z-ae5abaa0` both completed the original two-session GPT-OSS 20B
smoke with `agent_finished` and complete cleanup. Their frozen execution
configurations are identical after removing `model.host`. The verified archived
targets match their retained launch requests.

A Framework launch using bird's preview was refused as stale. Reusing an accepted
key with a different target was refused as a conflict. After restarting with both
targets removed, exact accepted launch requests still replayed to the original
runs with their original archived target labels. This checks completed-run
readback and replay, not active-worker restart/recovery. Registration reviews from
both hosts produced one shared model registration with distinct endpoint bindings;
both accepted registration requests also replayed after target removal.

The original CLI/UI fixture, worker-package, service-normal, Docker scripted and
live run-and-view gates passed separately. The reviewed target screenshots add
wide and narrow presentation fixtures; all existing baselines stayed unchanged.
An initial native test setup used a group-writable ancestor rejected by the
existing credential policy before any inference. That failure was retained; the
successful test used a private temporary service root.
