# Coding-repair benchmarks

Three versioned, synthetic Python repair tasks from the November 5, 2026
Ornith/Qwen Codex CLI comparison. These are seed bundles plus independent
assessments, usable with captured output from Codex CLI or the Rust Playtime
runner. They do not add an experiment controller or run a model.

| Bundle | Functional checks | Deliverable checks | Total |
| --- | ---: | ---: | ---: |
| `ledger-repair-v1` | 8 | 2 | 10 |
| `dependency-planner-repair-v1` | 22 | 2 | 24 |
| `byte-range-repair-v1` | 35 | 2 | 37 |

Each bundle has `TASK.md`, a deliberately broken program and any sample inputs
under `seeds/<id>`. Its independent checker, known-good reference with example
outputs, historical checker and provenance are under `assessments/<id>`. Only
the seed directory is supplied to the model. The checks cover exact arithmetic,
malformed and Unicode numeric inputs, graph cycles/deep chains, mutation,
sorting, CLI failures, and the required example files.

## Use a task in a future run

Validate the bundles first:

```sh
python3 scripts/seed-bundle.py list
python3 scripts/seed-bundle.py validate
```

A file-backed Rust recipe selects `seeds/<id>`. Follow the existing
[recipe authoring workflow](../../docs/rust-recipes.md); these bundles do not
change the recipe catalog or its reviewed limits. For example, copy an existing
recipe to a new file, change its `id` to match that filename, and set:

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
loading that recipe. The original Codex trials used a 900-second maximum per
task. Their later stop-after-first-timeout rule was a batch scheduling policy;
this assessment command neither schedules tasks nor retries them. Preserve
skipped tasks as skipped. A new Playtime-native run has a different harness and
prompt envelope even when its task files are identical; record it accordingly.

After the run stops, download and verify the final workspace through the Rust
CLI, or retain the stopped Codex workspace and its original run record. Point
the checker at the directory containing `report.py`, `planner.py` or `ranges.py`
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
special files are refused. Candidate-local Python modules are supported.

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

The two deliverable checks per task inspect the requested example and README
presence. The historical README rule is only **more than 50 bytes**; it does not
verify the quality or truth of the documentation. Individual cases can exercise
several inputs, so 70/71 is a case tally, not a calibrated accuracy percentage.
Finite examples are not a proof of correctness. Keep assessment feedback away
from the model until its final output has been captured.

## Isolation and versions

The assessment runs only in the image pinned by `environments/general.lock.json`,
as uid 1000, with read-only candidate/checker mounts, no network, one CPU,
256 MiB memory with no extra swap, 32 PIDs and a 32 MiB temporary filesystem.
The default assessment deadline is 30 seconds (`--assessment-timeout` accepts
1–120); this is separate from the model's task budget. The owned container is
removed on completion, timeout or CLI cancellation (SIGINT/SIGTERM). There is no
host import/execution of candidate programs, Docker socket mount, or implicit
image pull.

Candidate prints and low-level stdout/stderr writes are suppressed while the
checker runs; only its structured report is emitted. This prevents ordinary
program output from corrupting the report. The grading code is not a security
boundary against a program deliberately trying to evade the checks; container
isolation and assessment correctness serve different purposes.

Task revision **1** preserves the original task/starter bytes. Assessment
revision **2** retains the same acceptance cases while adding helper-module
support, clean report transport and explicit failure handling. Each task's
`historical-check.py` is the original checker, with its digest and original seed
digests in `provenance.json`. Historical model scores retain their original
checker identity. Do not silently replace old scores when changing assessment
semantics; increment the assessment revision and retain the earlier evidence.

## Validate without running models

```sh
python3 scripts/test-coding-repair.py -v
python3 scripts/test-coding-repair-docker.py -v
```

The first command validates frozen identities, report completeness/counts,
snapshot behavior, skipped outcomes and unavailable infrastructure without
executing candidates. CI runs it alongside seed-bundle validation. The Docker
gate runs all six positive/negative controls, local-helper and noisy-output
cases, missing programs, partial-workspace outcomes and deadline/cancellation
cleanup. It requires the existing locked image and fails if prerequisites are
absent.

Expected control scores are **10/10, 24/24, 37/37** for the references, and
**1/10, 9/24, 6/37** for the unrepaired starters. The reference artifacts are
assessment fixtures and are never included in a model's seed bundle.

The [migration validation](../../experiments/coding-repair/assessment-migration-2026-11-06.json)
reassessed 23 retained Codex workspaces from the Qwen trial. All 517 individual
case verdicts matched their original receipts. It records the assessed workspace
and checker identities and preserves timeouts as timeouts; no model was rerun.
