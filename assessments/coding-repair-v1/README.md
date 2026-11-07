# Coding-repair benchmarks

Six versioned Python coding tasks: the original three repairs from the November
5, 2026 Ornith/Qwen comparison, plus three adaptations of Q20–Q22 of the
[Reddit model test](https://www.reddit.com/r/LocalLLM/comments/1xnikyz/please_test_your_models_before_spending_money_on/).
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

Each bundle has `TASK.md`, a deliberately broken program and any sample inputs
under `seeds/<id>`. Its independent checker, known-good reference with example
outputs, historical checker and provenance are under `assessments/<id>`. Only
the seed directory is supplied to the model. The checks cover exact arithmetic,
malformed and Unicode numeric inputs, graph cycles/deep chains, mutation,
sorting, CLI failures, and the required example files. The imported tasks add
waiter cancellation and cleanup races, streaming UTF-8/JSON parsing with bounded
allocation, and exact shortest walks with target-relevant negative cycles.

Playtime is the canonical home for all six tasks and their graders. The Reddit
adaptations preserve the task IDs, seed bytes, reference files and acceptance
cases from notes' `bird-screen-v2`. The notes copy is a historical archive for
reproducing old receipts, not a second catalog to extend.

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
budget. `async-singleflight`, `streaming-jsonlines` and `shortest-walk` each retain
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
`planner.py`, `ranges.py`, `singleflight.py`, `jsonlines.py` or `shortestwalk.py`)
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
Reddit-derived tasks retain their original single deliverable check: `README.md`
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

Assessment revision **4**, now the default, adds the three Reddit-derived tasks
in `catalog-v4.json`. Their checkers gain normal registered module imports and
the shared private report transport; `driver-v4.py` preloads their standard-library
dependencies. Acceptance cases and README rules are preserved. The original
three tasks keep revision 3's checks. Revisions 2 and 3 retain their catalogs,
driver and checkers; `--assessment-revision 3` selects the preceding three-task
catalog. New tasks are unavailable under those older revisions. This import
does not establish a quality ranking or make every task discriminating for every
model; retain per-task results, functional/deliverable counts and matched budgets.

## Validate without running models

```sh
python3 scripts/test-coding-repair.py -v
python3 scripts/test-coding-repair-docker.py -v
```

The first command validates frozen identities, report completeness/counts,
snapshot behavior, skipped outcomes and unavailable infrastructure without
executing candidates. CI runs it alongside seed-bundle validation. The Docker
gate runs reference and starter controls for all six tasks, ten targeted faults
in the imported tasks, local modules/executables,
dataclasses with postponed annotations, noisy output, missing programs,
README directories under historical and current assessment revisions,
partial-workspace outcomes and deadline/cancellation cleanup (including signals
during removal). It requires the existing locked image and fails if prerequisites
are absent.

Expected control scores are **10/10, 24/24, 37/37** for the references, and
**1/10, 9/24, 6/37** for the unrepaired starters. The imported tasks' references
score **11/11, 21/21, 17/17**, with **0** passing checks for their starters.
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
