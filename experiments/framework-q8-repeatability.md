# Framework Q8 coding repeatability

The owner approved adding the three runnable coding recipes and comparing
Ornith 1.5 35B Q8 with Qwen3.6 35B Q8 on November 6, 2026. The frozen design is
[framework-q8-repeatability-v1.json](framework-q8-repeatability-v1.json).
Evidence is retained at `/home/data2/playtime/framework-q8-repeatability-20261106`.

## Exact models

| Model | Installed alias | Manifest SHA-256 |
| --- | --- | --- |
| Ornith 1.5 35B Q8 | `ornith-1.5:35b-q8-codex-128k` | `e16d49bcb069bf4775e562b320bf7b1378e2c87af118e2e46c034870721389a6` |
| Qwen3.6 35B Q8 | `qwen36-q8:codex-128k` | `bcad7fafcadb408fdcf791adbb1d88a47b8e3bbcf9fa40ad16c6c0a15a679552` |

Both aliases retain their November 5 identities on Ollama 0.40.14/ROCm. Their
sampling defaults, 128K allocation, batch size 512, full-layer placement and
absence of speculative decoding are included in the locked definitions. Qwen's
alias also retains its renderer correction. No weights or runtime settings were
changed for this study.

The native three-request review passed for each model with omitted, On and Off
thinking at 131,072 context and a 2,048-token response allowance. It checked a
synthetic tool call, a unique tool-result receipt, streaming, completed usage and
client cancellation. Independent server logs recorded cancellation release and
idle slots after each probe. Both loaded identities reported 38,561,361,427 bytes
of GPU allocation, equal to their total allocation. Short probes establish native
compatibility and allocation, not full-window recall or coding correctness.

The new locks are compiled into the registered-model resolver and worker digest.
Rebuild the daemon and frozen worker bundle together before registering them.
Existing aliases, earlier locks and historical results retain their identities.

## Recipes and comparison design

The catalog now includes `ledger-repair`, `dependency-planner-repair` and
`byte-range-repair`. Their seeds come from the corresponding versioned bundles;
reference solutions and independent checks remain outside model input. Each
source has a 900-second wall limit. Other source limits match `simulation-repair`,
including the 400-second per-model-call limit and 8,192-token response allowance.
The recipes load with the original model; save a revision selecting the desired
registration, settings and harness before launching.

The comparison uses the native continuous-compaction harness, thinking On,
131,072 context, temperature 0.6, top-p 0.95, top-k 20 and repeat penalty 1.
Compaction uses the existing defaults: 75% trigger, two recent exchanges,
2,048 summary tokens and early completion through `finish_run`. Both models
receive the same task-specific prompt, seeds, tools and controller instructions.
This is a new Playtime-native study, not another Codex CLI run or a Codex-base
prompt treatment; keep its scores and timings separate from the earlier trials.

Three paired rounds use seeds 1042, 1043 and 1044. Model order alternates by task
and round. Each attempt gets a fresh workspace, and the preceding model is
unloaded only after idle confirmation so every attempt starts without a retained
model context. Loading time is included in observed latency and reported
separately where the backend supplies it. No downloads overlap the comparison.

A first timeout stops that model's remaining scheduled attempts, following the
owner's earlier stop-after-timeout preference. Skipped attempts remain skipped;
there are no automatic retries. Infrastructure failure or uncertain cleanup
stops scheduling pending investigation. Every accepted attempt is retained.

## Assessment and reporting

After each run stops and cleanup completes, download the verified workspace and
its final capture index. CLI downloads deliberately save files as 0600 and omit
empty directories. Before assessment, restore directory entries and normalize
regular files to 0755 when the verified index has execute bits, otherwise 0644,
matching the checker's snapshot policy. Reject links and unsupported entries;
never execute candidate code on the host. Retain the verified raw capture too.
Run `scripts/check-coding-repair.py` with the original execution outcome. Assess
all 71 named checks across the three tasks, keeping functional and deliverable
counts separate. A timed-out partial workspace is not a completed repair even
if its files pass. Do not give independent-check feedback to a later attempt.

Report completion and all-check pass counts per task/model, case failures,
elapsed time, generated tokens and unknown-usage requests. Latency uses archived
observations: first native output, first non-thinking content/tool output when
available, and first tool action, measured from the first work dispatch. These
are observer timings, not precise provider-side token timestamps. Retain missing
or incomplete timing as unknown. Report repeated-input tokens separately from
uncached prompt work and peak context.

Results will be appended after the frozen schedule has completed or stopped
under its declared policy. Three rounds can expose variability on these tasks;
they do not establish a general coding-quality ranking.
