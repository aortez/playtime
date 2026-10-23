# Prompt comparison: first chunk

> **Historical Python results.** The study status and continuation commands below
> are retained as recorded in October 2026. The Python controller is now frozen;
> do not run or resume these experiments. See the
> [Rust operation guide](../docs/rust-mvp.md#run-it) for current execution.

Chunk 1 completed on 2026-10-18 UTC, using sampling seed 1042 in the frozen order
Current, Minimal, Persona. The study is paused. All six runs in chunks 2 and 3
remain frozen and unstarted.

The [combined local review](../runs/studies/20261018T003256Z-prompt-screening/chunk-1-review.html)
contains artifact checks, selected checkpoint claims, and links to their evidence.
The [protocol](prompt-screening.md) defines the comparison; the local
[frozen plan](../runs/studies/20261018T003256Z-prompt-screening/study.json) records
the nine archives and shared identities. Local reports require retaining `runs/`.

## What happened

| Condition and report | Observed trajectory |
| --- | --- |
| [Current](../runs/20261018T003500Z-0349f7aa/report.html) | Chose Mandelbrot before inspecting seed contents, then developed Ulam prime spirals. Its memory described broad computational-art interests. |
| [Minimal](../runs/20261018T003500Z-f95d55f4/report.html) | Inspected the seed files, produced cellular-automaton and waveform-spectrum images, then repeatedly failed to create an animated GIF. A Docker timeout ended the run. |
| [Persona](../runs/20261018T003500Z-6adc67a2/report.html) | Produced Mandelbrot images, zoom GIFs, and a gallery. The saved mementos contain general interests rather than a concrete invented past; later memory adds an interest in interactive web pages. |

These are observations from one sampling seed, not evidence of a reliable prompt
effect. The minimal run also completed less work because of its infrastructure
failure. All conditions retained the normal protocol, inventory, tool guidance,
and explicit memory mechanism. The minimal invitation did not test discovering
memory without instructions.

## Costs and stops

| Condition | Known generation | Unknown reserve | Charged tokens | Calls (unknown usage) | Agent seconds | Saved / started sessions | Stop |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Current | 45,512 | 52,170 | 97,682 | 89 (7) | 327.708 | 10 / 10 | Session limit |
| Minimal | 22,897 | 32,768 | 55,665 | 67 (4) | 211.202 | 5 / 6 | Docker command timeout |
| Persona | 53,845 | 40,960 | 94,805 | 105 (5) | 442.199 | 10 / 10 | Session limit |

Total: 122,254 known generation tokens plus 125,898 reserved for unknown usage,
or 248,152 charged tokens; 261 calls and 981.109 seconds (16.35 minutes) of agent
work. Charged tokens are a conservative budget measure, not measured generation.
Snapshotting, review, and cleanup add time. Monetary and electricity costs were
not measured. Cleanup succeeded for all three runs.

The minimal run's exact error was `PlaytimeError: Command timed out: docker` at
the session-6 boundary. Its final handoff is retained in the event log, but no
sixth workspace snapshot was saved. The final extracted workspace is checkpoint
5. The failed run was retained and was not retried.

## Independent checks

Checks ran as uid 1000 in isolated copies of five saved workshop checkpoints.
They cover selected properties, not complete artifact correctness:

- Current's final Ulam geometry and prime masks passed independent references.
  Its color Mandelbrot classification passed 221 reference points. Earlier,
  an accidental overwrite left the Ulam script containing only `import os`:
  it exited successfully but generated no new image. Old output remained.
- Minimal's two saved plots matched independently computed cellular-automaton
  and direct-DFT references pixel for pixel. Its saved GIF generator fails on
  missing `imageio`; no GIF exists. Checkpoints 3–5 nevertheless claim creation.
  The event-recorded final handoff corrects that claim.
- Persona's basic Mandelbrot function misclassified 24 of 221 reference points;
  its zoom and smooth functions passed the selected checks. Its images and GIFs
  decode, but the gallery requests a nonexistent GIF and the Play/Pause button
  only toggles an unused flag. Later mementos acknowledge these defects without
  repairing the page. The claimed 4096-pixel image is actually 3600 pixels wide.

The [selected claim ledger](../runs/studies/20261018T003256Z-prompt-screening/selected-claims-chunk-1.json)
contains seven supported, seven contradicted, and one unverified claim. These
selected claims are not an exhaustive accuracy score. Preferences and invented
identity are outside factual scoring. One reviewer browser probe initially used
an incorrect executable path; its corrected probe and both records are retained
in the [review corrections](../runs/studies/20261018T003256Z-prompt-screening/review-corrections.json).
No model inference was repeated during review.

## Starting materials as a separate comparison

The existing pack contains a cellular automaton, waveform samples, and a palette.
Minimal followed those materials, which makes their influence worth testing.
Current chose Mandelbrot before reading them, so their contents do not explain
that initial choice. The shared instructions, tools, model, and sampling may
also contribute to repeated themes; this chunk does not isolate those causes.

A useful next comparison is the existing seed pack versus an empty workspace,
holding the minimal invitation and all other settings fixed. Run a fresh pair
per sampling seed and pause between pairs. Then consider a contrasting seed pack
if the first comparison warrants it. This is a proposed separate study, not a
change to the six remaining prompt-comparison slots. Copy the chosen recipe and
change only `seeds` plus identifying metadata: the existing `empty.toml` also
changes thinking, budgets, and workshop resources, so it is not a matched control.

## Validation and resuming

All nine runs use archived Python controller commit
`ad7a2f2a5a6947d396f601a43fe622716e41200c`. Controller and workshop code were
unchanged. The controller suite passed 82 tests with six opt-in Docker tests
skipped; real-Docker scripted smoke and the model/image identity check passed.
See the [validation record](../runs/studies/20261018T003256Z-prompt-screening/validation.json).

All nine archive identities and all 26 actual session-opening inputs were
verified. Reinvoking completed chunk 1 launched nothing and preserved its event
and result hashes. The [integrity record](../runs/studies/20261018T003256Z-prompt-screening/integrity-chunk-1.json)
also confirms that the six later runs have not started.

When continuing, invoke each chunk separately from the project root:

```sh
python3 runs/studies/20261018T003256Z-prompt-screening/run_chunk.py 2
# Review and pause before separately invoking:
python3 runs/studies/20261018T003256Z-prompt-screening/run_chunk.py 3
```

Chunk 2 uses seed 2042 in Minimal, Persona, Current order. Chunk 3 uses seed 3042
in Persona, Current, Minimal order. The launcher preserves every started slot;
it never selectively retries a failed run. No later chunk starts automatically.

One completed seed, unequal realized work, the Docker timeout, added prompt
length, runtime nondeterminism, and human judgment limit interpretation. Further
chunks can test whether these trajectories recur without changing the frozen
conditions or coaching the model.
