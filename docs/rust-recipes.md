# Reviewed recipe catalog

Long live runs come from a reviewed catalog: the `*.toml` files in `recipes/`.
The daemon loads the catalog at startup, checks every recipe against compiled
bounds, and freezes the chosen recipe into the run's archive at admission. The
first recipe is `curiosity`, the MVP's 10-session experiment, with limits the
owner confirmed on 2026-10-18.

## Run it

```sh
target/debug/playtimed --root /absolute/service-root --bundle /absolute/new-bundle \
  --docker-scripted --ollama-endpoint http://oldman-bird.local:11434 \
  --recipes /absolute/checkout/recipes
target/debug/playtime --connection-file /absolute/service-root/.playtime/connection.json \
  run curiosity --mode experiment --idempotency-key my-retained-key --wait
```

The workbench offers the same recipe. Execution uses `experiment` mode on the
configured live model (see [live model](rust-live-model.md)). Loading and authoring
recipes do not require `--ollama-endpoint` or a ready workshop. If any supplied
recipe is invalid, the daemon refuses to start and names the file and reason.
To change a file-backed source, edit it and restart the daemon between runs.
[Saved prompt revisions](#saved-recipes-and-prompt-editing) can be created from
the workbench without restarting.

## Format

A recipe file holds everything the model and the workshop get:

- `id` must match the file name.
- `name` is shown in the catalog.
- `seeds` (optional) names a directory relative to the repository root.
- `workspace_entries` caps the files and directories in `/work`.
- `prompt`, `protocol` and `inventory` are the exact texts the controller sends.
- `[parameters]` holds the controller's run parameters: `sampling_seed`,
  `memory_condition`, `[parameters.limits]` and `[parameters.workshop]`.
- `[model]` holds the request settings for the locked `gpt-oss:20b`: `thinking`
  (`default`, `low`, `medium` or `high`), `temperature`, `top_p`, `top_k` and
  `repeat_penalty`.

The context size is `context_tokens`; the keep-alive is fixed at five minutes.
Unknown fields are refused.

The preview shows the effective recipe. Its `effective_configuration` is the
profile that admission freezes, including the recipe and its seed contents, and
its seed summary gives each file's size and digest. The preview revision covers
the whole profile. The archive freezes the profile, with the recipe, in
`inputs/scenario.json`. The worker kind is `docker_live`.

## Bounds

`playtime_runner::docker::BOUNDS` sets reviewed upper limits on top of the
controller's own checks. The values: 20 sessions, 50 turns a session, 4 hours,
1,000,000 generated tokens, 16,384 tokens per call and per handoff, a 131,072-token
context, 600-second model and 300-second command caps, 16 CPUs, 32 GiB of memory,
a 48 MiB workspace with 4,096 entries, 1 GiB of `/tmp` and 1,024 PIDs.

Recipes must also meet these rules:

- The command cap must be more than 3 seconds, the return allowance each
  command reserves (see [run-and-view](rust-docker-run-and-view.md)).
- The event log can't exceed 64 MiB.
- Only the `self` memory condition is supported.
- Every session's capture of a full workspace must fit the archive (below).

The worker checks the same bounds when it loads a frozen profile. Bundles
advertise the bounds and a reference recipe's frozen template in their
`docker_profile` digest, so a worker and a daemon built differently refuse each
other at startup.

## Seeds

A recipe's seed directory is read at daemon startup and frozen into the recipe:
at most 16 regular UTF-8 files, each at most 131,072 bytes (128 KiB), and 512 KiB
in total. Seeds must also fit within half the workspace. Links, special files and
non-UTF-8 names or contents are refused. Empty directories and file modes aren't
kept.

At session 1, once the agent container is running and before the first model
request, the worker writes each file to `/work/seeds/<path>` through the fixed
bridge's `write_file`, as uid 1000. If a write fails, the run stops with an
infrastructure failure before anything is sent to the model. Session 1's
checkpoint lists the seeds as added files. As in Python, the first checkpoint
includes them.

## Limits for ten sessions

**Storage.** A checkpoint captures `/work` all or nothing. Each capture holds at
most 64 MiB and 4,096 entries, and stores the tar plus every retained file, so it
costs about twice the workspace. Captures accumulate across the run against the
archive's 512 MiB of verified content and 10,000 references. These are the
default storage limits every reader opens archives with, so a recipe can't raise
them. A session whose capture doesn't fit ends the run with an infrastructure
failure and loses that checkpoint.

The curiosity recipe therefore caps `/work` at 16 MiB and 900 entries instead of
Python's 1 GiB:

- At most about 34 MiB per session, and 340 MiB over ten sessions.
- Inputs take about 16 MB, mostly the worker and its sources.
- With publication headroom, that is about 370 of 512 MiB, and at most 9,144
  references.
- Python-era curiosity workspaces held 0.9–4.6 MB in 29–53 files.

When `/work` is full, the agent sees "No space left on device" and the run goes
on. `/tmp` holds 1 GiB, isn't captured, and is cleared at each reset.

**Event log.** The live smoke archives held 90–227 KB for 7–15 requests. Each
`model_request` record carries the conversation so far and dominated them, with
averages of 4.7–7.2 KB. Coalesced chunks average about 0.7 KB, at most two a
second while the model generates.

For curiosity, the context guard keeps each request's input under about 32K tokens:
the 64K context minus 8,192 reserved, the 8,192-token handoff and 16 KiB of
output. That is at most about 160 KB per `model_request`. A session makes at
most about 16 requests (10 turns, the handoff and parse retries), so ten sessions
log at most about 25 MB of requests. The other records add at most about 8 MB:

- chunks for up to 1,800 seconds of generation, about 2.5 MB;
- the generated text, in chunks and again in responses, about 2.5 MB;
- about 100 tool results of up to 16 KiB each, about 3 MB.

The worst case is about 33 MB, and a typical run logs far less. The 64 MiB log
leaves at least twice that, about 6,000 events against the 100,000 limit. Python-era
10-session runs logged up to 33 MB in their own format.

## The curiosity recipe

- **Run limits:**
  - 10 sessions of 10 turns, 1,800 work seconds, 300,000 generated tokens.
  - 8,192 tokens per call and per handoff, a 65,536-token context.
  - 180-second model and 60-second command caps.
  - 16 KiB of tool output, an 8 KiB memento, a 64 MiB log.
  - Five parse retries. A session's eighth parse failure, recovered or not,
    ends that session through its handoff, and the next session counts from zero.
- **Workshop:** 8 CPUs, 10,000 MiB of memory, a 16 MiB `/work` with 900 entries,
  1 GiB of `/tmp` and 256 PIDs.
- **Model:** `thinking: high`, temperature 0.8, top-p 0.95, top-k 40, repeat
  penalty 1.0, seed 1042.
- **Texts:**
  - Seeds come from `seeds/assortment-01`.
  - The prompt is `prompts/inspiration.txt`.
  - The protocol is the owner's 2026-10-18 edit of `prompts/protocol.txt`.
  - The inventory follows Python's format, the image's collections and
    `workshop/docs/INVENTORY.md`, plus one line on the workspace limits.

## Evidence

On 2026-10-18 a throwaway daemon loaded the catalog with `--recipes`. Its preview
showed `curiosity` with four seeds (5,787 bytes), `thinking: high` and the
confirmed limits. One live run, `20261018T235429Z-9911f51c` (evidence retained
outside version control), showed:

- **Seeds.** In session 1 the model's second command, `ls -R .` in `/work`, listed
  the four seeds, and the model then read each one. Session 1's checkpoint holds
  them with matching digests. The archive froze the recipe and its seeds in
  `inputs/scenario.json`.
- **Sessions.** Session 1 used its 10 turns in about 23 seconds of short, one-tool
  turns and handed off a memento. Session 2 started a fresh conversation with
  it.
- **Contention.** 22 timed requests, with no reloads and no queue waits.
- **Cancel.** A cancel after 45 seconds stopped the worker as `interrupted`.
  Within a second it published session 2's interrupted checkpoint, removed its
  Docker resources and finished the archive with cleanup complete.

Cancellation requests the worker unit's stop. Even when the worker writes a final
checkpoint, the daemon does not treat cancellation as normal confirmed completion;
the run can remain `reconciling` (`worker_unavailable`). An operator can release
it after verified worker termination and resource cleanup; see
[daemon completion and release](rust-daemon.md#results-and-safe-completion).

## Saved recipes and prompt editing

Services advertise `recipe_authoring_v1`. In the workbench, select a catalog
recipe, choose **Clone recipe**, edit its name and prompt, and **Save revision**.
Select **Edit recipe** on a saved recipe to create its next revision. The model identity, harness, whole-run budgets, protocol, and seeds remain
inherited and read-only. The model settings editor changes reasoning, sampling,
context capacity, and ordinary-response/memento-handoff caps. Unsaved
edits disable launch and prevent changing the selection until explicitly saved
or discarded. A conflicting edit keeps its text; it can be saved as a new recipe
or discarded before selecting the current head.

Saving selects an immutable execution ID and requests a fresh preview. A newer
head never retargets a selected revision, preview, or pending launch. The existing
launch request uses that execution ID as `recipe_id`; there is no implicit save
on launch. Clients write a private receipt under `recipe-saves/` before sending a
save. If its result is uncertain, **Look up save** and **Retry same save** retain
the original service, request, and key. Reopening the workbench restores these
pending receipts. Unsaved text is not persisted across process exit.

The service stores revisions, logical heads, and keyed results atomically in its
private SQLite ledger (version 6). Limits are 256 logical recipes, 4,096 retained
revisions, 128 MiB of definition/request/result bytes, and 4 MiB per encoded row.
Names allow 128 UTF-8 bytes and prompts 65,536 bytes. Existing seed and recipe
bounds still apply. Exhaustion refuses a new save without losing old reads or
replays. There is no deletion or garbage collection in this slice.

Saving copies inherited bytes from the already loaded source or saved revision;
it does not reopen TOML or seed paths. Saved recipes therefore survive removal of
the original `--recipes` directory. Supplying an invalid catalog still fails
startup, as does damaged saved history. Reading, cloning, and editing definitions
work without ready Docker or Ollama, including at startup. Preview returns
`execution_unavailable` without a launch token until the stored identities and
current execution environment can be verified. A fresh preview after readiness
returns does not change the saved revision. Accepted launch replay remains
available independently of readiness.

The combined picker lists source recipes and saved heads. Saved families keep
creation-sequence order through renames and new revisions. A cursor fixes the
initial creation cutoff, so later families appear on refresh. Each page observes
current heads consistently; it is not a snapshot of all heads across pages.
Source catalog/configuration changes invalidate combined-picker cursors; head
changes and readiness changes do not. Revision history has its own high-water
mark and service/family scope.

An authored run freezes its complete definition and provenance inside
`inputs/scenario.json`, alongside the effective recipe and scenario. Both daemon
and worker validate their agreement and supported model/environment identities.
The worker profile digest includes the definition-format capability, so an older
bundle cannot silently accept the new field. Rebuild the static worker bundle
when upgrading the daemon for authoring.

### Authoring validation, 2026-10-22

Automated coverage uses the real SQLite catalog and authenticated API/client:
concurrent head conflicts, replay and restart without sources, source conflicts,
rollback before save-result insertion, capacity refusal, stable head pagination,
cursor scope/generation, and revision-history growth. Workbench tests cover offline
editing, save/reopen/recovery, stale definition replies, keyboard editing, and
narrow layouts. The two editor screenshots use the pinned software renderer.

A native acceptance run cloned curiosity while Ollama was unavailable, restarted
without the source catalog, recovered the save, then restored readiness. It
previewed revision A, saved revision B, and launched A. Run
`20261023T024125Z-d3693877` completed with `agent_finished`, cleanup complete,
and a writer-confirmed terminal sequence of 91. Its first model request contains
A's exact prompt; its archived definition, effective recipe, scenario, and seeds
agree. Evidence is retained locally at
`/tmp/playtime-authored-native-sc2vuat0/evidence` (not a portable repository fixture).

An earlier probe was deliberately cancelled after its first tool and encountered
the existing incomplete cancellation/recovery path. It was released after the
service verified worker termination and absent resources; its evidence remains
at `/tmp/playtime-authored-native-n_r0z6k3`. This work does not complete the deferred
restart/recovery integration. The separate service-normal, Docker run-and-view,
and live run-and-view gates also passed, including a normal live workbench launch.

The same completed service was then restarted with a missing Docker socket and
without sources. It saved another clone and replayed the accepted launch while
preview was unavailable. Restoring the socket produced a fresh ready preview
without restarting; the saved definition and picker listing stayed identical.
That check is recorded in the same evidence directory as
`readiness-restoration.json`.

## Model settings editor

`recipe_model_settings_v1` adds **Model settings** to the clone/revision form.
Reasoning offers Model default, Low, Medium and High for the locked GPT-OSS model.
Model default omits `think`; its effective default level is not claimed known.
Temperature accepts 0–2, context up to 131,072 tokens and each response/handoff
cap up to 16,384. Advanced fields accept top-p >0 and <=1, top-k 1–1,000, repeat
penalty >0 and <=2, and numeric sampling seed 0–2,147,483,647. These choices do
not modify whole-run budgets or the checked-in recipe. Reset buttons restore the
values at editor entry. Changed values remain visible before Save revision.

The service checks structural limits and conservative first-request context fit
without inference. Prompt construction and reserves are shared with the running
controller. A context that would immediately hand off is refused with a bounded
explanation; no field is silently adjusted. Later requests still enforce context
and remaining generation independently. Context estimates and the locked model's
reported ceiling do not guarantee GPU memory fit or a number of turns.

`GET /recipes/{recipe_id}/model-settings` returns identity-bound settings and
capabilities without requiring live Docker/Ollama. Clone/revise requests optionally
supply a complete `model_settings` object; omission retains the source settings.
New settings revisions freeze `model_capabilities` and its provenance. The current
resolver is a reviewed fallback scoped to the complete GPT-OSS lock and Ollama
0.32.3, with context ceiling from locked `model_info`; it does not infer support
from names. [Ollama's thinking contract](https://docs.ollama.com/capabilities/thinking)
distinguishes explicit reasoning choices from omission. Model registration (#7)
will extend this resolver to other models and advertised metadata.

The fields are additive: existing frozen definitions, keyed request JSON and
replies remain readable; the service ledger stays at version 6. Rebuild the worker
bundle because its capability digest now includes settings authoring. This does
not establish a general migration or data-reset policy.

### Settings validation evidence (2026-10-22)

The native acceptance run `20261023T043804Z-78c826dc` saved settings while Ollama
was unavailable, reopened without the source catalog, restored readiness, and
launched reviewed revision A after saving head B. A local forwarding test
transport retained the actual Ollama request: temperature 0, sampling seed 0,
context 49,152, ordinary response cap 4,096, top-p 1, top-k 40, repeat penalty 1,
and omitted `think`. Its archived handoff cap was 2,048. Definition, scenario and
model settings agreed. It ended `agent_finished`, with complete cleanup, 219
known generated tokens and confirmed frontier sequence 63. Evidence is retained
outside Git at `/tmp/playtime-model-settings-native-zj58z8kb/evidence`.

The final-bundle repeat `20261023T044216Z-cee307b9` also passed the same checks;
evidence is retained at `/tmp/playtime-model-settings-native-nnwwrm1p/evidence`.

The standard live smoke gate did not pass on two retained attempts. Run
`20261023T043720Z-91131432` ended at the session limit because the model used
`finish_run=false` in its final handoff. Run `20261023T043844Z-44e9afb2` ended
`agent_finished`, but a required saved file was absent. Both had complete cleanup
and known usage. Their roots are `/tmp/pt20-ad6c10701eb2` and
`/tmp/pt20-a15a56acc70e`. The authored-settings acceptance does not substitute for
that gate or establish restart/recovery behavior.

### Live smoke failure investigation (2026-10-22)

Inspection of those two archives explains the failures more precisely:

- `20261023T043720Z-91131432`: in session 2 the model repeated both session-1
  setup commands, then called `end_session` with `finish_run=false`. The
  controller correctly stopped at its two-session limit. It never received the
  command to write `result.txt`.
- `20261023T043844Z-44e9afb2`: the model split the session-2 verification into
  six calls: read the text, inspect the image, check `/tmp`, read the counter,
  sleep, and read the counter again. All checks succeeded, including the same
  counter value (`9`) before and after the wait. Those calls exhausted the six
  work turns, so the controller offered only `end_session`. The model ended the
  run and claimed `result.txt exists` in its memento without having written it.
  The missing file is consistent with the command history and checkpoint index;
  this is not evidence of an artifact disappearing during capture.

Both archives retain the unchanged baseline smoke scenario and model settings
(temperature 0.8, low reasoning, context 32,768), without an authored recipe or
settings override. The session-opening messages match the pre-settings-PR
construction byte for byte. Replaying the archived model responses and tool
results through the Rust simulator reproduces all six/ten request context and
budget decisions and both stop reasons. This replay treats command text as data;
it neither executes commands nor re-establishes Docker or live-model behavior.
Local replay inputs/results are under `/tmp/playtime-smoke-investigation`.

The evidence points to model instruction-following and a brittle smoke task,
not a settings-authoring regression. The prompt includes both sessions' steps
and says to run each command in its own call, while the second session fits its
turn budget only if the model groups the verification commands. Follow-up should
clarify the numbered command blocks and session selection, retain independent
artifact assertions, and report the actual stop reason and exhausted limit.
The two failed gates remain failed evidence; no additional live run was made
during this investigation and no recipe limits were increased.

The subsequent [smoke prompt and gate follow-up](rust-live-model.md#evidence)
clarified command grouping and corrected the gate's text-based session selector.
The full live CLI/workbench gate then passed with the original limits and model
settings; its new evidence is recorded there alongside the retained failures.
