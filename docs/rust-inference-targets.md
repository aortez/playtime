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
limited to 16 unique target IDs and 64 KiB. Only `provider: "ollama"` is currently
implemented. Names are display labels, not attested hardware identities.

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
restart. Restarting does not migrate an active run or complete the deferred
worker-loss/recovery functionality.

## Identity and retained requests

`GET /api/v1/targets` lists configuration without contacting the machines.
`X-Playtime-Target` scopes inventory, inspection and registration review to one
configured target. With named targets, a metadata request without it is refused
as `invalid_parameters`; without them, those routes answer as before. Discovery
stays independent of workshop readiness. An offline target does not prevent
inspecting another target or editing saved recipes.

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
