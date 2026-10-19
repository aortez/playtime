# Recipes as data (#74)

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

The workbench offers the same recipe. Catalog recipes run in `experiment` mode on
the live model, so `--recipes` needs `--ollama-endpoint` (see
[live model](rust-live-model.md)). If any recipe is invalid, the daemon refuses to
start and names the file and the reason. To change a recipe, edit it and restart
the daemon between runs, never during one.

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
  (`low`, `medium` or `high`), `temperature`, `top_p`, `top_k` and
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

The daemon stops the worker's unit as soon as it delivers a cancel. It never
confirms the final frontier, so it shows the run as `reconciling`
(`worker_unavailable`) until an operator releases it (#71).
