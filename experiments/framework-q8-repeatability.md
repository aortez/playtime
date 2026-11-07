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

## Results

[Machine-readable results](framework-q8-repeatability-results-v1.json) retain
run IDs, settings references, outcomes, functional/deliverable counts, failure
details, timing and evidence hashes.

The schedule ended at 2026-11-07T00:01:06.084917+00:00. Of 18 planned attempts,
11 ran and seven were skipped after Ornith's first timeout. 9 attempts completed
normally; 5 completed with every check passing. These counts include a wall
timeout and a separate tool-batch-cap stop as unsuccessful attempts, even where
partial files passed checks.

| Model | Task | Round | Outcome | Checks | Time (m:ss) | Known output tokens |
| --- | --- | ---: | --- | ---: | ---: | ---: |
| Ornith Q8 | Ledger | 1 | Completed | 10/10 | 9:10 | 21,404 |
| Qwen3.6 Q8 | Ledger | 1 | Completed | 9/10 | 4:04 | 9,278 |
| Qwen3.6 Q8 | Planner | 1 | Completed | 23/24 | 7:18 | 16,575 |
| Ornith Q8 | Planner | 1 | Wall timeout | 23/24 | 15:01 | 36,732 |
| Qwen3.6 Q8 | Byte range | 1 | Completed | 37/37 | 5:42 | 12,871 |
| Qwen3.6 Q8 | Ledger | 2 | Completed | 9/10 | 4:49 | 10,857 |
| Qwen3.6 Q8 | Planner | 2 | Completed | 24/24 | 5:06 | 11,832 |
| Qwen3.6 Q8 | Byte range | 2 | Completed | 25/37 | 6:42 | 15,287 |
| Qwen3.6 Q8 | Ledger | 3 | Tool-batch cap | 8/10 | 1:40 | 3,556 |
| Qwen3.6 Q8 | Planner | 3 | Completed | 24/24 | 5:40 | 12,945 |
| Qwen3.6 Q8 | Byte range | 3 | Completed | 37/37 | 3:47 | 8,608 |

Ornith's planner files passed all 22 functional cases and produced the sample
plan, but the README was missing and the run had not finished at the 15-minute
deadline. Calls four and six each exhausted the 8,192-token response allowance,
mostly in reasoning, without a tool call. The remaining seven Ornith attempts
were skipped under the frozen policy, so this study does not establish Ornith
repeatability.

Qwen's third ledger attempt returned nine valid `write_file` calls creating
distinct test fixtures in one response. The existing controller limit is eight;
it stopped before dispatching that batch. This is a controller compatibility
outcome, and its partial 8/10 score is not a completed repair. Scheduling
stopped for inspection, then resumed only the two unstarted Qwen tasks after
cleanup and idle state were confirmed. The original failed attempt, controller
limit, prompts, settings and frozen worker were retained; no model attempt was
retried.

Qwen's ledger invalid-row case failed in all three rounds. The first planner
round failed `invalid_item`. Its second byte-range repair returned `[0, 9]`
instead of `[[0, 9]]` for full-file responses, causing 12 case failures. Full
per-attempt failures are retained in the JSON rather than collapsed into one
accuracy percentage.

## Timing and interpretation

| Model | Measured generation rate across attempts | Median cold first output | Median cold first tool action |
| --- | ---: | ---: | ---: |
| Ornith Q8 | 42.2–43.0 tokens/s | 12.2 s | 12.8 s |
| Qwen3.6 Q8 | 40.7–44.2 tokens/s | 12.2 s | 12.7 s |

Generation rates include reasoning and tool-call tokens and use completed
backend generation counts/durations. Cold model load is included in
first-output/action latency; the operating system file cache was not cleared.
Archive chunks are coalesced, so these are observer timings rather than exact
first-token timestamps.

The first ledger pair illustrates the difference between throughput and task
speed. Both averaged about 42.2 generation tokens/s, but Ornith generated 21,404
tokens and took 9:10, while Qwen generated 9,278 and took 4:04. Ornith spent
about 8:27 generating, 19 seconds processing prompts and 11 seconds loading;
Qwen spent about 3:40 generating, 6 seconds processing prompts and 10 seconds
loading.

The attempts used 69.0 minutes of measured run time in total. Schedule wall time
was 101.1 minutes, including grading, unloading/loading boundaries and the
operator investigation pause. Completed responses account for 159,945 generated
tokens; 1 interrupted request has unknown usage, with its 8,192-token
reservation kept separate from measured generation.

The largest reported completed prompt was 22,609 tokens despite the
131,072-token allocation. These tasks do not test full-window recall or
establish long-context quality. Input totals repeat context across requests;
peak input and backend-reported uncached prompt tokens are recorded separately.

Both models meet the desired roughly 30–40 tokens/s generation range on these
task contexts, but that alone does not establish dependable autonomous coding.
The observed task failures, timeout and controller interaction matter more than
the similar raw speed. Keep this native-harness study separate from the earlier
Codex results and treat the incomplete Ornith schedule as a limitation of the
comparison.

## Validation and retained evidence

- Formatting and workspace Clippy passed; workspace tests had 687 passes and 35 ignored tests.
- The frozen Python suite passed (101 tests, six skipped).
- Nine offline and ten Docker coding-checker controls passed; all five seed bundles validated.
- Docker scripted and original-model live run-and-view gates passed. Bird was restored to Ollama 0.40.14 afterward.
- All six default/on/off native compatibility probes passed before task inference.
- Every accepted attempt has a complete cleanup receipt and retained workspace, independent assessment, event history and report. Assessment restored executable bits and directories from the verified capture index.

After collection, the study model was unloaded and the isolated daemon stopped.
Framework remained on Ollama 0.40.14 with no loaded models. The service history
and all evidence remain available.

Raw evidence, orchestration, the original stop and resume decision remain under
`/home/data2/playtime/framework-q8-repeatability-20261106`. The isolated service
history is `/home/data2/playtime/q8-1106`. The worker was built from clean
commit `a3d373f147d29b99f193ebe9af54996339106cd3`; result documentation was
added afterward. No weights, runtime versions, model settings, assessment
identities or task inputs were changed during the comparison.
