# Seed bundles

Reusable starting materials for exploration and directed experiments. This first
slice is file-based authoring; the workbench still inherits seeds from a recipe.
The existing Rust runner freezes their bytes into each execution. These helpers
do not run a model or replace the Rust controller.

## Browse, create, and clone

From the checkout:

```sh
python3 scripts/seed-bundle.py list
python3 scripts/seed-bundle.py new my-materials --name "My starting materials"
python3 scripts/seed-bundle.py clone assortment-01 my-assortment --name "My assortment"
python3 scripts/seed-bundle.py validate my-materials
python3 scripts/seed-bundle.py validate
```

`new` writes a manifest and an `ABOUT.txt` template. Add or edit ordinary UTF-8
files under `seeds/<id>/`, describe their origin, then validate. `clone` copies
the source bytes and records its revision and content digest as provenance. It
does not copy an assessment: review and attach one explicitly if the clone is
also a directed task. Commands refuse existing destinations.

To use a bundle, set `seeds = "seeds/<id>"` in a file-backed recipe. Restart the
daemon between runs to load file changes, then clone/save/preview that recipe
through the normal authoring workflow. The recipe freezes the actual seed bytes;
editing source files cannot change a previously saved revision or running run.
See [recipes](../docs/rust-recipes.md).

The helper checks the current runner's file bounds: 16 regular UTF-8 files,
128 KiB per file, 192 KiB per JSON-encoded file, 512 KiB total, and 200-byte
relative paths. Links and special files are refused. The receipt reports minimum
workspace bytes and entries; the Rust recipe validator remains authoritative
for fitting seeds into the selected workspace and archive budgets. Empty
directories and executable modes do not persist as seed inputs.

## Manifest schema 1

Each `seed-bundles/<id>.json` contains:

| Field | Meaning |
| --- | --- |
| `schema_version` | `1` |
| `id` | Lowercase letters/digits with hyphens; matches the filename |
| `name`, `description` | Human-readable purpose |
| `revision` | Positive, manually maintained revision number |
| `files` | `seeds/<id>`; only this directory is sent to the model |
| `provenance` | Origin, authorship, or source bundle digest |
| `assessment` | Optional `assessments/<id>` directory, otherwise `null` |

Manifests and source files are versioned in Git. Increment `revision` when
publishing changed starting materials. The validation receipt's bundle digest
binds manifest metadata and the sorted file paths, sizes, and SHA-256 digests.
It does **not** bind assessment implementation bytes: record those separately
when planning an assessed experiment. The manifest/revision is a local authoring
convention, not a new service-side immutable catalog. A later API/workbench slice
can add saved bundle revisions and selection using the existing recipe model.

## Initial catalog

- **assortment-01:** optional signals, colors, and a cellular automaton. Open
  exploration; no independent correctness score.
- **simulation-repair-v1:** repair custom initialization, periodic updates, exact
  step counts, snapshot independence, and continuation. The recipe
  `simulation-repair` provides a ten-minute source allocation. Select the desired
  registered model and harness in a saved revision before launching.
- **ledger-repair-v1:** repair exact integer money calculations, row validation,
  duplicate handling and a deterministic CSV report; 10 acceptance checks.
- **dependency-planner-repair-v1:** repair execution waves, validation and cycle
  reporting without mutating input; 24 acceptance checks.
- **byte-range-repair-v1:** repair exact range parsing/normalization and CLI
  validation; 37 acceptance checks.

The three coding bundles preserve the November 2026 Ornith/Qwen trial's task
inputs. Their [assessment guide](../assessments/coding-repair-v1/README.md)
explains recipe use, the reusable Docker checker, reference/starter controls,
versioned provenance and the distinction between passing checks and finishing
a run. Their checkers and reference solutions stay outside the supplied seeds.

## Assessing the repair task

The model writes `SELF_ASSESSMENT.json`, with a `passed`, `failed`, or `unverified`
verdict and evidence for each requirement. That is a model claim, not a controller
completion status or an independent score. The model can write and run its own
tests. Keep `assessments/` outside the supplied seed directory.

Before a trial, freeze the checker and validate it against both the broken
starter and known-correct reference:

```sh
bash scripts/check-simulation-repair.sh seeds/simulation-repair-v1
# Expected exit 1, complete JSON: R1 passes; R2-R5 fail.
bash scripts/check-simulation-repair.sh assessments/simulation-repair-v1/reference
# Expected exit 0, complete JSON: all five pass.
```

After the run, download the writer-confirmed final workspace with the Rust CLI.
Retain the original files and their verified digests before assessment. Then:

```sh
bash scripts/check-simulation-repair.sh /absolute/downloaded-workspace > independent.json
# Check exit status: 0 or 1 with a complete five-requirement JSON report.
# A timeout, OOM, missing image, or invalid/missing report is incomplete evidence.
python3 scripts/compare-seed-assessment.py \
  /absolute/downloaded-workspace/SELF_ASSESSMENT.json independent.json
```

The checker executes the candidate only inside the pinned workshop image as uid
1000, with read-only mounts, no network, a 30-second deadline, and CPU/memory/PID
bounds. The container is removed on completion or timeout. Model-controlled
programs are never imported or executed on the host. Assessment has its own
container lifetime and cannot alter the frozen experiment archive.

Preserve per-requirement results and missing/malformed self-assessments. The
comparison flags claimed passes that fail independent checks; it preserves the
model's evidence text but does not verify every cited observation. Independent
checks are finite correctness examples, not a proof or a security boundary
against a candidate intentionally evading a grader. Hide their results until
the model's final assessment has been captured.

## First trial

The [first Ornith trial](../experiments/seed-repair/first-trial.json) on the RTX
4090 used 64K context and a ten-minute allocation. It stopped after 157 seconds
because the second compaction spent its entire 4,096-token budget on reasoning,
returning no visible summary. Cleanup and final capture completed.

The captured implementation passed all five independent checks. Its
self-assessment claimed five passes, but used a `verdicts` object instead of the
required `requirements` array; the comparison records that as invalid rather
than silently converting it. The model also recovered from overwriting its own
test file, while incorrectly attributing that overwrite to the harness in a
summary. This is evidence for the separate correctness, self-report, and run
outcome records, not a completed ten-minute trial or a model ranking.

The [8,192-token summary trial](../experiments/seed-repair/summary-8k-trial.json)
changed only that allowance and reached the full ten-minute limit. All nine
compactions succeeded; one used 5,734 tokens, exceeding the earlier cap. The
model submitted a valid self-assessment claiming five passes, but the captured
program passed four independent checks. Its last returned snapshot aliased the
live simulation, failing R4. Its own tests mutated the first snapshot or a
`values()` copy and missed this case. Many later responses repeated completion
because this fixed-duration recipe disables early finish. These findings support
keeping task correctness, self-assessment, and execution outcome separate; they
do not show that a larger summary allowance changes task quality in general.

PR review extended the checker to accept candidate-local helper modules and to
check snapshot independence after zero updates as well as positive step counts.
The [review validation](../experiments/seed-repair/review-validation.json) records
positive/negative controls and unchanged results on both verified trial captures.
Original trial records retain their original checker identities.

## Validation

```sh
python3 scripts/test-seed-bundles.py -v
python3 scripts/test-seed-assessment.py -v
python3 scripts/seed-bundle.py validate
```

These are offline helper tests, not Python-controller experiments. Validate
checker changes with the additional Docker controls:

```sh
python3 scripts/test-seed-assessment-docker.py -v
```

This gate requires the existing locked image and checks the broken starter,
correct reference, valid local-module imports, and zero-step snapshot aliasing.
It never executes candidate code on the host and fails when prerequisites are
missing. CI runs the offline helper tests and catalog validation; the Docker
controls run locally where the locked image is available. For task changes, update
the requirement/checker correspondence and retain both positive and negative
controls before collecting a new model trial.
