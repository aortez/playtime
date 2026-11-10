# Coding-repair benchmarks

Twelve versioned coding tasks: the original three repairs from the November
5, 2026 Ornith/Qwen comparison, six adaptations of Q20–Q22, Q24, Q33 and Q35 of the
[Reddit model test](https://www.reddit.com/r/LocalLLM/comments/1xnikyz/please_test_your_models_before_spending_money_on/),
two original compound-state tasks for a harder screen, and a long-context
policy-edit task.
These are seed bundles plus independent
assessments, usable with captured output from Codex CLI or the Rust Playtime
runner. They do not add an experiment controller or run a model.

| Bundle | Functional checks | Deliverable checks | Total |
| --- | ---: | ---: | ---: |
| `ledger-repair-v1` | 8 | 2 | 10 |
| `dependency-planner-repair-v1` | 22 | 2 | 24 |
| `byte-range-repair-v1` | 35 | 2 | 37 |
| `async-singleflight-v1` | 10 | 1 | 11 |
| `streaming-jsonlines-v1` | 20 | 1 | 21 |
| `shortest-walk-v1` | 16 | 1 | 17 |
| `sql-tombstones-v1` | 15 | 1 | 16 |
| `delta-debugging-v1` | 13 | 1 | 14 |
| `stable-quadratic-v1` | 17 | 1 | 18 |
| `crash-journal-v1` | 20 | 1 | 21 |
| `linearizable-kv-v1` | 21 | 1 | 22 |
| `context-policy-edit-v1` | 12 | 1 | 13 |

Each bundle has `TASK.md`, a deliberately broken program and any sample inputs
under `seeds/<id>`. Its independent checker, known-good reference with example
outputs, historical checker and provenance are under `assessments/<id>`. Only
the seed directory is supplied to the model. The checks cover exact arithmetic,
malformed and Unicode numeric inputs, graph cycles/deep chains, mutation,
sorting, CLI failures, and the required example files. The imported tasks add
waiter cancellation and cleanup races, streaming UTF-8/JSON parsing with bounded
allocation, and exact shortest walks with target-relevant negative cycles.

Playtime is the canonical home for these tasks and their graders. The original
three Reddit adaptations preserve the task IDs, seed bytes, reference files and acceptance
cases from notes' `bird-screen-v2`. The notes copy is a historical archive for
reproducing old receipts, not a second catalog to extend.

The three later adaptations broaden coverage: SQL chooses live versions after
ranking tombstones, delta debugging must handle non-monotone predicates, and the
quadratic solver must retain small roots despite floating-point cancellation.
The SQL oracle is independent Python selection, the minimizer is checked by its
properties without requiring a particular valid subsequence, and the numerical
oracle uses the ordinary formula at 400-digit decimal precision. Each includes
80 deterministic generated examples as one named case. These cases are not 80
independent benchmark problems. README presence earns a separate deliverable
check; proofs, algorithm-choice requirements and explanation quality need manual
review and are not established by the automated score.

The [bounded diversity selection](../../experiments/coding-repair/diversity-screen-v1.json)
uses single-flight, JSON Lines and these three additions: five four-minute tasks,
at most 20 minutes of model work per configuration before setup and grading.
It selects from this catalog and does not copy task definitions. A first screen
is one attempt per task; repeats must be declared separately, with failures kept.

## Harder screen and calibration

The [hard screen](../../experiments/coding-repair/hard-screen-v1.json) replaces
the SQL and quadratic problems with two original tasks. It keeps five
four-minute attempts and the same 20-minute maximum per configuration:

- **Crash journal:** recover the exact valid byte prefix while preserving
  interleaved transaction atomicity, commit order and earlier commits across
  truncation/corruption. The grader uses known frame boundaries and independent
  commit-log replay, including every byte-level crash point in a sample log.
- **Linearizable key-value history:** find the lexicographically first complete
  ordering consistent with real-time precedence and observed reads/atomic
  multi-key compare-and-swap results. Small cases use an exhaustive permutation
  oracle; a larger case exercises equivalent-prefix pruning. Locally legal
  choices can require backtracking, and pruning by completed operations alone
  loses state needed to find a valid witness.

These tasks are **uncalibrated**. Passing references and rejecting eleven
targeted faults establish grader controls, not a difficulty gap between local
and frontier models. Start with the two new tasks (at most eight minutes of
model work per configuration). Measure a frontier baseline under comparable
tools and budgets, then repeat promising distinctions before ranking models.
The target is full solvability for a frontier model and meaningful headroom for
locals; do not force that outcome by selecting favorable runs. If both groups
pass, retain sanity coverage and version the next harder tasks. If both fail,
inspect task clarity, the oracle and the budget first.

The primary score is **fully solved functional tasks**, reported as X/Y.
Per-case results remain diagnostics, with each task weighted equally if a
partial-credit mean is shown. Do not pool many easy checks into a near-perfect
headline percentage. README presence, explanations/proofs, timeouts and speed
are separate. An incomplete assessment is unknown; a timed-out candidate can
still have a functional score from its stopped workspace. Generated examples
within a case are not independent problems. Preserve old task versions,
calibration outcomes and held-out confirmation runs separately.

The [November 7 Bird screen](../../experiments/coding-repair/gyro-m-hard-screen-2026-11-07.json)
ran one 240-second attempt per task using Codex CLI 0.160.0 inside the locked
Docker workshop, with the model served by Strata. This was an external adapter,
not a native Playtime controller run. Gyro-M passed all functional checks on
3/5 tasks; single-flight and JSON Lines passed from timed-out workspaces, while
delta debugging completed. It scored 2/20 on crash journal and 6/20 on history
ordering. A matched IQ2 control on only those two new tasks scored 20/20 and
12/20 respectively, completing both attempts within the deadline. M ended both
new tasks with unexecuted tool markup; IQ2 did so on history ordering. The
captured implementations also contain concrete bugs. These are measurements of
the full agent setup, not an isolated reasoning comparison or a general model
ranking. The frontier baseline remains unrun. L downloaded concurrently, so
elapsed times are not an isolated speed comparison. Raw attempts, preflight
failures, source hashes, stopped workspaces and cleanup receipts are retained
at the paths in the result record.

The subsequent [Gyro-M runtime comparison](../../experiments/coding-repair/gyro-runtime-comparison-2026-11-07.json)
ran the same journal/history pair under Strata with DFlash2, Strata with both
neural and suffix drafting disabled, and the compatible publisher llama.cpp
fork without drafting. The GGUF, explicit Codex catalog, function tools,
four-minute task limits and 8,192-token per-response cap were shared. The
function-only tool configuration differs from the earlier screen, so its scores
are not pooled with this one.

Frozen functional task scores were **2/2, 2/2 and 1/2** respectively. Both
Strata journal turns completed; both history turns timed out with 20/20
functional checks. The llama journal timed out with 20/20. Its history turn
consumed a full 8,192-token response in reasoning, received `response.completed`
and ended without editing the starter (6/20). Keep that token-cap delivery
failure separate from reasoning quality. Required READMEs were present on
2/2, 1/2 and 0/2 tasks; presence does not validate their explanations.

The [posthoc no-op-write counterexample](../../experiments/coding-repair/gyro-noop-write-counterexample-2026-11-07.json)
exposes a missing grader case: the DFlash history implementation incorrectly
forces a currently no-op write that a valid ordering needs later to restore a
value. It fails this case while the no-drafting Strata candidate passes. An
independent enumeration of all 24 permutations confirms the sole valid ordering.
This diagnostic was selected after inspecting the first artifact, was never fed
back to a candidate, and does not alter assessment revision 6 or its scores.
Assessment revision 7 promotes this case as `deferred_noop_write`, before the
Framework drafting comparison. The revision-6 checker and scores remain intact.

One attempt per arm and this selected diagnostic do not establish a general
runtime accuracy ranking. No populated 64K/128K accuracy or frontier baseline
was tested in that Bird comparison. Its result record retains setup corrections,
completed and partial API evidence, resource measurements, stopped workspaces
and cleanup receipts.

## Framework drafting and populated-context comparison

The [November 10 study plan](../../experiments/coding-repair/framework-drafting-2026-11-10-plan.json)
freezes three coding tasks, two repetitions and both drafting modes: twelve
four-minute attempts, plus two one-minute exact-output controls. The run uses
the installed Framework Gyro-M weights, Strata 0.2.12 / HIP Halo 0.1.58, medium
thinking, identical explicit sampling and an alternating paired order. It uses
assessment revision 7 and captures every final or timed-out workspace. The
external Codex CLI adapter uses Playtime's locked Docker workshop; this is not
a native Playtime controller run. Model commands execute only inside Docker.

`context-policy-edit-v1` separately tests applying three approved rules from
a populated archive while ignoring rejected proposals and preserving existing
behavior. Its versioned archive generator is in the assessment directory;
only the seed files and generated archive reach the model. The independent
checker uses fixed boundary-price tables, legacy behavior and protected-file
checks. Reference/starter controls and deliberately faulty pricing variants
are checked before model attempts. The archive size and hash, actual prompt
tokens, sampling, tool envelope and deadlines must be recorded for each study.
Allocation alone is not a populated-context measurement.

The context probe keeps the 163,840-token allocation but raises the experimental
compaction threshold to 150,000 so the 128K input remains present during the
edit. It does not change installed launcher profiles. Initial prompt processing
and later tool work are reported separately. This one synthetic edit is not a
general long-context accuracy benchmark.

The [completed study record](../../experiments/coding-repair/framework-drafting-2026-11-10.json)
retains all sixteen attempts, frozen source identities, request/response traces,
stopped workspaces and restoration evidence. The separate
[128K plan](../../experiments/coding-repair/framework-drafting-long-2026-11-10-plan.json)
records the calibrated archive and the one failed setup token-count request;
there were no retries of scored attempts.

Functional checks on the captured coding artifacts, in repetition order:

| Task | Drafting off | DFlash2 |
| --- | --- | --- |
| Crash journal | 19/20, 20/20 | 20/20, 20/20 |
| Linearizable history | 6/21, 6/21 | 6/21, 6/21 |
| Streaming JSON Lines | 0/20, 20/20 | 20/20, 0/20 |

This gives **2/6 versus 3/6 fully functional artifacts at stop**, not completed
deliveries. Eleven attempts hit the four-minute deadline. The remaining
DFlash2 parser attempt ended in 13.6 seconds with malformed tool markup emitted
as text and an untouched starter. All four history attempts also left the
starter unchanged; 6/21 is its baseline score. Only one coding artifact had a
README. No coding attempt both finished its turn and passed all functional
checks. Two repetitions do not establish a quality winner or a drafting-caused
regression; interactive tool output and sampling trajectories can differ.

Generation-time-weighted engine throughput was **27.6 tokens/s off versus
48.4 with DFlash2**, including observed generation near cancellation. Unknown
partial API usage remains unknown. Both exact-output controls passed both file
checks and completed: 43.1 seconds off, 29.5 seconds with drafting. These prompts
explicitly required no newline, so they do not erase the earlier differently
worded fast-profile smoke failure.

The single synthetic 128K edit completed successfully under both modes:

| Measurement | Drafting off | DFlash2 |
| --- | ---: | ---: |
| Initial input tokens | 130,990 | 130,990 |
| Cold first token | 207.9 s | 207.3 s |
| Completed task elapsed | 430.1 s | 330.8 s |
| Generation throughput | 26.4 tokens/s | 55.0 tokens/s |
| Private functional/preservation checks | 12/12 | 12/12 |
| README present | Yes | Yes |
| Cached follow-up first-token range | 0.9–2.2 s | 0.8–2.2 s |

DFlash2 used about 23% less total task time in this pair. The identical initial
prompt produced the same first tool calls, but subsequent trajectories and
amounts of generated text differed. This is task elapsed time, not a fixed-output
speed measurement. Initial prompt processing accounts for most of the remaining
wait. First-correct-artifact time was not measured; the grader assesses the final
captured workspace. The frontier baseline remains unrun.

The study left Framework in balanced mode with idle Ollama, no Strata server,
unchanged retained Ollama model identities and no owned containers or volumes.
The kernel journal had no entries during the study. Both installed profiles
retain 163,840 context, 114,688 compaction and medium default thinking.
The next useful bounded comparison is low versus medium thinking on the same
hard tasks: medium often consumed the deadline before delivering code. These
results support the fast profile as an experimental worker option, with the
original profile retained for comparison.

## Use a task in a future run

Validate the bundles first:

```sh
python3 scripts/seed-bundle.py list
python3 scripts/seed-bundle.py validate
```

A file-backed Rust recipe selects `seeds/<id>`. Follow the existing
[recipe authoring workflow](../../docs/rust-recipes.md); these bundles do not
change controller bounds. The catalog includes `ledger-repair`,
`dependency-planner-repair`, and `byte-range-repair`, each with a 900-second task
budget. `async-singleflight`, `streaming-jsonlines`, `shortest-walk`,
`sql-tombstones`, `delta-debugging`, `stable-quadratic`, `crash-journal`
and `linearizable-kv` each use
the bounded screen's **240-second** budget. Select a registered model, context
and harness in a saved revision before launching. For a new task, copy an
existing recipe to a new file, change its `id`
to match that filename, and set:

```toml
name = "CSV ledger repair"
seeds = "seeds/ledger-repair-v1"
prompt = '''
Read /work/seeds/TASK.md. Copy the supplied program and sample inputs into
/work, then complete the repair there. Save the implementation, requested
example output and README.md under /work. Run your own checks and finish with
the changes and checks you actually performed.
'''
```

Review the remaining model, harness, early-finish and budget settings before
loading that recipe. The original three-task Codex trials used a 900-second
maximum per task; the later Bird screen used 240 seconds. The original trials'
stop-after-first-timeout rule was a batch scheduling policy;
this assessment command neither schedules tasks nor retries them. Preserve
skipped tasks as skipped. A new Playtime-native run has a different harness and
prompt envelope even when its task files are identical; record it accordingly.

After the run stops, download and verify the final workspace through the Rust
CLI, or retain the stopped Codex workspace and its original run record. Point
the checker at the directory containing the task's program (`report.py`,
`planner.py`, `ranges.py`, `singleflight.py`, `jsonlines.py`, `shortestwalk.py`,
`query.sql`, `ddmin.py`, `quadratic.py`, `journal.py` or `linearize.py`)
and the requested example/README files. Do not assess a workspace still being
edited.

```sh
python3 scripts/check-coding-repair.py ledger-repair-v1 \
  /absolute/captured-workspace --outcome completed --metadata run-metadata.json \
  > independent.json
```

The optional metadata file is a JSON object. Record the harness and version,
model identity/quantization, runtime, context/KV settings, thinking and sampling
settings, task deadline, elapsed time and the original run/archive identifier.
For example:

```json
{
  "harness": "codex_cli",
  "harness_version": "0.160.0",
  "run_id": "retained-run-identifier",
  "model": "record the exact model and digest",
  "runtime": "record the runtime and version",
  "context_tokens": 131072,
  "thinking": "medium",
  "deadline_seconds": 900,
  "elapsed_seconds": 739.995
}
```

The helper freezes bounded copies of the candidate and checker before execution
and records their SHA-256 identities, task/assessment revisions and the locked
workshop image. The original workspace is not modified. Candidate files are
limited to the Rust capture bounds of 4,096 entries and 48 MiB; symlinks and
special files are refused. The fingerprint includes empty directories and each
file's effective permissions: executable files become mode 0755 and other files
0644, without special permission bits. Candidate-local Python modules and
executable helpers are supported, including dataclasses with postponed annotations.

## Read the result

Execution and assessment are separate fields:

- `execution.outcome`: caller-supplied `completed`, `timed_out`, `cancelled`,
  `failed`, `skipped`, or the default `unknown`. Metadata is also supplied by
  the caller; the checker does not establish worker completion or archive
  publication.
- `assessment.status`: `complete` means all expected cases produced valid
  verdicts and the container stopped normally. `incomplete` means the checker
  could not establish a score. `not_run` is used for an explicitly skipped task.
- A complete assessment has separate `functional` and `deliverable` counts,
  the original combined count, and all named case results.

An exit code of **0** means all independent checks passed (or the task was
explicitly skipped; inspect `assessment.status`). **1** means a complete
assessment with failed checks. **2** means an incomplete assessment or invalid
invocation. Missing Docker/image, timeout, OOM, invalid output or cleanup failure
does not become a zero score. A timed-out model's partial files can pass every
check while `execution.outcome` remains `timed_out`.

```sh
# Preserve an unattempted task without invoking Docker or inventing a score.
python3 scripts/check-coding-repair.py byte-range-repair-v1 --outcome skipped

# Check partial output without relabeling the model run as completed.
python3 scripts/check-coding-repair.py ledger-repair-v1 \
  /absolute/timed-out-workspace --outcome timed_out
```

The original three tasks have two deliverable checks for the example and README;
revision 3 and later require **regular files of more than 50 bytes**. The three
original Reddit-derived tasks and the three later adaptations use a single
deliverable check: `README.md`
must be a regular file, with no minimum size. These checks do not verify the
quality or truth of the documentation. Individual cases can exercise
several inputs, so 70/71 is a case tally, not a calibrated accuracy percentage.
Finite examples are not a proof of correctness. Keep assessment feedback away
from the model until its final output has been captured.

## Isolation and versions

The assessment runs only in the image pinned by `environments/general.lock.json`,
as uid 1000, with read-only candidate/checker mounts, no network, one CPU,
256 MiB memory with no extra swap, 32 PIDs and a 32 MiB temporary filesystem.
The default assessment deadline is 30 seconds (`--assessment-timeout` accepts
1–120); this is separate from the model's task budget. The owned container is
removed on completion, timeout or CLI cancellation (SIGINT/SIGTERM), before the
scratch snapshots are deleted. Cancellation during removal is recorded and
reported after bounded cleanup finishes. There is no host import/execution of
candidate programs, Docker socket mount, or implicit image pull.

Candidate prints and low-level stdout/stderr writes are suppressed while the
checker runs; only its structured report is emitted. This prevents ordinary
program output from corrupting the report. The grading code is not a security
boundary against a program deliberately trying to evade the checks; container
isolation and assessment correctness serve different purposes.

Task revision **1** preserves the original task/starter bytes. Assessment
revision **2** retains the same acceptance cases while adding normal module
imports, local executable helpers, clean report transport and explicit failure
handling. Each task's `historical-check.py` is the original checker, with its
digest and original seed
digests in `provenance.json`. Historical model scores retain their original
checker identity. Do not silently replace old scores when changing assessment
semantics; increment the assessment revision and retain the earlier evidence.

Assessment revision **3** fixes the planner and byte-range
README checks: a directory named `README.md` no longer earns deliverable credit.
Ledger already required a regular file and keeps the same rules. Case IDs,
task inputs and positive/negative control scores are unchanged. The revision-2
`catalog.json` and each `check.py` remain untouched; `catalog-v3.json` and the
two `check-v3.py` files select the new rules. To reproduce the old acceptance
semantics, pass `--assessment-revision 2`. The report still identifies the
current helper bytes, so it is a new assessment, not the original receipt.

Assessment revision **4** adds the original three Reddit-derived tasks
in `catalog-v4.json`. Their checkers gain normal registered module imports and
the shared private report transport; `driver-v4.py` preloads their standard-library
dependencies. Acceptance cases and README rules are preserved. The original
three tasks keep revision 3's checks. Revisions 2 and 3 retain their catalogs,
driver and checkers; `--assessment-revision 3` selects the preceding three-task
catalog. New tasks are unavailable under those older revisions. This import
does not establish a quality ranking or make every task discriminating for every
model; retain per-task results, functional/deliverable counts and matched budgets.

Assessment revision **5** adds Q24/Q33/Q35 in `catalog-v5.json`
and preloads SQLite, Decimal and math in `driver-v5.py`. Earlier task checkers and
all previous catalogs/drivers remain unchanged. Newly authored task provenance
pins `initial_checker_sha256`; it has no redundant historical checker copy.

Assessment revision **6** adds crash-journal and linearizable
history in `catalog-v6.json`. Its driver preloads zlib and itertools for the
new graders. Earlier catalogs, drivers, task bytes and checker cases remain
unchanged; the earlier diversity selection explicitly retains revision 5.

Assessment revision **7** adds the deferred-no-op-write regression to the
history checker, preserving `check.py` for revision 6 and selecting
`check-v7.py` for later revisions. Assessment revision **8**, now the default,
adds the context-policy edit task. Existing acceptance cases retain their
revision-7 behavior. The Framework coding run explicitly pins revision 7;
its later context-edit stage uses revision 8.

## Validate without running models

```sh
python3 scripts/test-coding-repair.py -v
python3 scripts/test-coding-repair-docker.py -v
```

The first command validates frozen identities, report completeness/counts,
snapshot behavior, skipped outcomes and unavailable infrastructure without
executing candidates. CI runs it alongside seed-bundle validation. The Docker
gate runs reference and starter controls for all twelve tasks, ten targeted faults
in the original imports, nine in the later adaptations and eleven in the harder
tasks, plus deferred-no-op and context-policy regressions, local modules/executables,
dataclasses with postponed annotations, noisy output, missing programs,
README directories under historical and current assessment revisions,
partial-workspace outcomes and deadline/cancellation cleanup (including signals
during removal). It requires the existing locked image and fails if prerequisites
are absent.

Expected control scores are **10/10, 24/24, 37/37** for the references, and
**1/10, 9/24, 6/37** for the unrepaired starters. The imported tasks' references
score **11/11, 21/21, 17/17**, with **0** passing checks for their starters.
The later SQL/minimizer/numerical references score **16/16, 14/14, 18/18**;
their starters score **4/16, 0/14, 0/18**.
The journal reference scores **21/21**, with **1/21** for its starter. The
history reference scores **22/22**, with **6/22** for its starter under revisions
7 and 8 (revision 6 remains 21/21 and 6/21). The context-policy reference scores
**13/13**, with **9/13** for the starter that already implements legacy behavior.
Some empty/impossible cases are already correct in the stubs;
starter credit is not evidence that the full task is easy.
The [hard-screen validation](../../experiments/coding-repair/hard-screen-validation-2026-11-07.json)
records these controls, eleven injected faults and repository checks. It contains
no model attempts; the authored references are not a frontier baseline.
The reference artifacts are
assessment fixtures and are never included in a model's seed bundle.

The [migration validation](../../experiments/coding-repair/assessment-migration-2026-11-06.json)
reassessed 23 retained Codex workspaces from the Qwen trial. All 517 individual
case verdicts matched their original receipts. It records the assessed workspace
and checker identities and preserves timeouts as timeouts; no model was rerun.

The [Reddit-task migration validation](../../experiments/coding-repair/reddit-migration-2026-11-07.json)
reassessed ten retained Bird screen and Strata IQ2/IQ3 workspaces. All **162**
named case verdicts matched the original receipts, including the **26 failures**;
all five timed-out executions remain timed out. Candidate file hashes were
verified against the original receipts before assessment. These are migrated
measurements, not new model trials or evidence of a quality improvement.
