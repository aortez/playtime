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

A bounded diversity screen uses single-flight, JSON Lines and these three
additions: five four-minute tasks, at most 20 minutes of model work per
configuration before setup and grading. A screen selects tasks from this
catalog rather than copying their definitions. A first screen is one attempt
per task; repeats must be declared separately, with failures kept.

## Harder screen and calibration

The hard screen replaces the SQL and quadratic problems with two original
tasks. It keeps five four-minute attempts and the same 20-minute maximum per
configuration:

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

## Long-context policy edit

`context-policy-edit-v1` tests applying three approved rules from a populated
archive while ignoring rejected proposals and preserving existing behavior.
Its versioned archive generator, `archive.mjs`, is in the assessment
directory; only the seed files and the generated archive reach the model. The
independent checker uses fixed boundary-price tables, legacy behavior and
protected-file checks. Reference/starter controls and deliberately faulty
pricing variants are checked before model attempts. Record the archive size and
hash, actual prompt tokens, sampling, tool envelope and deadlines for each
study; allocation alone is not a populated-context measurement. This one
synthetic edit is not a general long-context accuracy benchmark.

## Study progress and learnings

Study plans, result records and evidence stay outside the repository; this
section keeps only the high-level progress. These studies used an external
Codex CLI adapter with Playtime's locked Docker workshop, not native Playtime
controller runs, so they measure the whole agent setup rather than reasoning
alone.

- **2026-11-07, Bird hard screen** (Gyro-M served by Strata, one 240-second
  attempt per task, revision 6): Gyro-M passed all functional checks on 3/5
  tasks (single-flight, JSON Lines, delta debugging) but scored 2/20 on crash
  journal and 6/20 on history ordering, ending both with unexecuted tool
  markup. A matched IQ2 control did better on those two: 20/20 and 12/20.
- **2026-11-07, Bird runtime comparison** on the journal/history pair: Strata
  with DFlash2, Strata without drafting and a llama.cpp fork without drafting
  fully solved 2/2, 2/2 and 1/2. The llama.cpp miss spent its whole
  8,192-token response on reasoning and never edited the starter. The DFlash2
  history artifact exposed a missing grader case, a deferred no-op write, which
  revision 7 adds as `deferred_noop_write`.
- **2026-11-10, Framework drafting** (Gyro-M on Strata 0.2.12, medium thinking,
  two repetitions each of crash journal, history and JSON Lines, revision 7):
  drafting off left 2/6 fully functional artifacts at stop and DFlash2 3/6.
  Eleven of twelve attempts hit the four-minute deadline and every history
  attempt left the starter untouched, so this establishes no quality
  difference. DFlash2 raised generation throughput from 27.6 to 48.4 tokens/s.
- **2026-11-10, 128K context edit** (`context-policy-edit-v1`, about 131K input
  tokens, compaction threshold raised to 150,000 so the archive stayed in
  context, revision 8): both modes passed every functional check. DFlash2 took
  about 23% less task time (331 s against 430 s). Cold prompt processing,
  about 208 s to the first token, dominated the wait; cached follow-ups started
  within about 1–2 s.

Next: compare low and medium thinking on the same hard tasks, since medium
thinking often used up the deadline before delivering code. No frontier
baseline under comparable tools and budgets has been run, so the hard tasks
remain uncalibrated. The fast (DFlash2) profile is an experimental worker
option; keep the original profile for comparison.

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
revision 3 and later require **regular files of more than 50 bytes**. The other
nine tasks use a single deliverable check: `README.md` must be a regular file,
with no minimum size. These checks do not verify the
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
unchanged.

Assessment revision **7** adds the deferred-no-op-write regression to the
history checker, preserving `check.py` for revision 6 and selecting
`check-v7.py` for later revisions. Assessment revision **8**, now the default,
adds the context-policy edit task. Existing acceptance cases retain their
revision-7 behavior.

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
starter credit is not evidence that the full task is easy. The authored
references are grader controls, not a frontier baseline. The reference
artifacts are assessment fixtures and are never included in a model's seed
bundle.

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
