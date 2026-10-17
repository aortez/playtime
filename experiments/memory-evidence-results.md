# Memory evidence pilot results · 2026-10-17

The appendix was delivered and archived as designed. B retained more usable
generators in the 1042 and 3042 pairs, but still carried false or stale claims
and repeated failures. These observations do not establish that the appendix
caused better memory or a reliable effect size.

The [complete local audit](../runs/pilots/20261017T220435Z-653bd6b1/review.html)
contains 493 quoted claim rows across all 51 saved checkpoints, evidence,
next-session actions, independent artifact checks and all six reports. The
successful handoff preceding the lost B2042 checkpoint has a separate ten-row
event-based audit. Run records remain local and excluded from Git.

## Frozen execution

Controller commit: `e09ef0250f0fa368adaa9cd8a31f287062480467`.
Controller archive SHA-256:
`38c289b3a2a82633ba4899c988ecd0bd0bb460ca803b6a9d0c24ab55412d8e84`.
All six slots were frozen before inference and executed from that archived
controller, once each, sequentially in the required A/B, B/A, A/B order.
There was no coaching, selective replacement or inference retry.

The [plan](../runs/pilots/20261017T220435Z-653bd6b1/pilot.json) records every input
hash, pair membership and order. Shared identities include:

- `gpt-oss:20b`, digest
  `17052f91a42e97930aa6e28a6c6c06a983e6a58dbb00434885a0cf5313e376f7`;
  Ollama `0.32.3`.
- Workshop image
  `sha256:797d27ef63ead176997892c6f2bbcd7fa4fad34eb52047011842a99eddd65093`.
- RTX 4090, UUID `GPU-08abdd4f-ec73-9117-136f-0291e25c316f`, driver `595.91.07`,
  observed before, during and after the batch on `oldman-bird.local`.
- Unchanged curiosity inspiration, protocol, initial workspace and sampling:
  high reasoning, 65,536 context, temperature 0.8, top-p 0.95, top-k 40,
  repeat penalty 1.0; ten sessions, ten work turns per session, 100,000 charged
  generation tokens and 900 seconds per run.

[Post-run verification](../runs/pilots/20261017T220435Z-653bd6b1/integrity.json)
confirmed all frozen identities and reconstructed every generated/received
appendix against source records and actual model requests. The 21 deliveries
were 621–3,629 UTF-8 bytes each, totaling 53,425 bytes; all stayed below 4,096.
They collectively marked five omitted file changes, five omitted commands,
48 truncated fields and 33,442 omitted excerpt bytes.

## Costs and outcomes

Known generation and unknown-usage reservations are separate. Reservations
are conservative budget charges, not measured generation. Sessions below are
started/snapshotted; handoffs include the successful save before B2042's
snapshot failure. Appendix bytes count each fresh-session delivery once.

| Run / local report | Known tokens | Reserved | Charged | Seconds | Calls | Stop | Sessions | Handoffs | Appendix bytes |
| --- | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: |
| [1042 A](../runs/20261017T220435Z-7ee55ec2/report.html) | 55,417 | 41,487 | 96,904 | 421.690 | 95 | session_limit | 10/10 | 4 | 0 |
| [1042 B](../runs/20261017T220435Z-5cf6f10f/report.html) | 63,967 | 36,033 | 100,000 | 441.751 | 73 | token_limit | 8/8 | 4 | 16,486 |
| [2042 B](../runs/20261017T220435Z-d6cec6b6/report.html) | 42,832 | 32,768 | 75,600 | 343.443 | 69 | error: Docker shutdown timeout | 6/5 | 5 | 15,920 |
| [2042 A](../runs/20261017T220435Z-1be775f7/report.html) | 60,130 | 32,768 | 92,898 | 417.315 | 102 | session_limit | 10/10 | 9 | 0 |
| [3042 A](../runs/20261017T220435Z-37a7043d/report.html) | 33,135 | 58,673 | 91,808 | 262.704 | 86 | tool_parse_error_limit | 8/8 | 5 | 0 |
| [3042 B](../runs/20261017T220435Z-eab44c2e/report.html) | 59,580 | 32,768 | 92,348 | 571.142 | 113 | session_limit | 10/10 | 6 | 21,019 |

Total: **315,061 known + 234,497 reserved = 549,558 charged tokens**, 538 calls,
31 calls with unknown usage, 2,458.045 seconds (41.0 minutes), 52 session starts,
51 checkpoints and 33 handoffs. The six-run bounds of 600,000 charged tokens
and 90 minutes were respected. Money and electricity were not measured.

## Claim and continuity audit

S/C/U means supported/contradicted/unverified. Rows are reviewer-defined claims,
with closely related output/dimension facts sometimes grouped; mixed claims
are contradicted when an asserted part is false, with supported parts explained.
Intentions and preferences are excluded. Repeated stale claims count again at
each checkpoint; these are not independent observations or comparable trials.
Definitions and exact quotes are in the
[audit protocol](../runs/pilots/20261017T220435Z-653bd6b1/audit-protocol.json) and
[claim ledger](../runs/pilots/20261017T220435Z-653bd6b1/claims.json).

| Run | S | C | U | Only memory changed | No file changes | Exact reattempts after failure | Failed again | Failed again across sessions |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1042 A | 69 | 22 | 1 | 1 | 3 | 3 | 3 | 0 |
| 1042 B | 91 | 21 | 1 | 2 | 2 | 3 | 1 | 1 |
| 2042 B | 44 | 0 | 7 | 1 | 1 | 1 | 0 | 0 |
| 2042 A | 51 | 2 | 7 | 3 | 1 | 10 | 6 | 3 |
| 3042 A | 39 | 13 | 0 | 3 | 1 | 4 | 4 | 4 |
| 3042 B | 123 | 2 | 0 | 0 | 2 | 4 | 4 | 4 |

Exact reattempts match stripped command text and cwd after an observed failure;
some succeed after repairs. Missing results are unknown. Near-duplicates and
different cwd spellings are not merged. File changes alone are not progress.

**1042:** A created valid PNGs, then introduced an `IndentationError` in session 6.
Stale passing-test claims persisted; the final memory claimed ten passing tests
and an unobserved 4000×3000 output. B retained working basic, smooth and zoom
generators but repeatedly misstated GIF frame counts and described an existing
deep script as “to be created.” Its session-4 appendix contained the five-frame
command and an argument error, yet memory remained inaccurate. B used 3,096 more
charged tokens and 20.061 more seconds; neither condition prevented false claims.

**2042:** A repaired its color renderer and produced static images and zoom GIFs,
then created a numerically incorrect Game of Life animation. B's clustering
statistics and saved fractal artifacts checked out. B stopped after a successful
sixth handoff because Docker shutdown timed out; only five workspaces were saved.
Its last handoff accurately acknowledged a color-cycle `TypeError`, but that new
artifact cannot be independently checked. B's 17,298 fewer charged tokens and
73.872 fewer seconds reflect an early failure, not demonstrated efficiency.

**3042:** A claimed absent color images and an unapplied repair, then partially
corrected its Gray–Scott memory to acknowledge grayscale fallback. B acknowledged
render timeouts, made smaller previews, and eventually produced an 800×800,
30-frame GIF in 31.347 seconds. It still repeated failed commands and retained a
numeric defect in its original renderer. B used only 540 more charged tokens,
but 308.438 more seconds and 27 more calls; A had larger unknown-usage reservations.

## Independent artifact checks

All 51 saved checkpoints were checked inside isolated workshops using the locked
image and uid 1000. The selected records contain 329 commands plus a supplementary
argument probe. Every saved raster frame decoded (241 image occurrences across
checkpoints). Numerical checks and execution exposed defects despite valid files:

- **1042 A:** seven retained PNGs decode. Checkpoint 5 passes seven archived
  unittest tests; checkpoints 6–10 fail imports, CLI and tests at line 114.
- **1042 B:** basic/smooth/deep sample renders and a new zoom GIF work.
  `zoom_test.gif` has ten frames; `zoom.gif` has five. Negative comma-separated
  center arguments fail; equals-form `--center=…` works for the deep renderer.
- **2042 B:** 500 points, cluster counts 104/130/154/112, recomputed inertia
  357.7481677032367 and zero nearest-centroid mismatches. Static scalar samples
  match; fourteen PNGs and a ten-frame GIF decode at checkpoint 5. Restored
  generators run. The unsnapshotted color-cycle program remains unvalidated.
- **2042 A:** fractal samples, regenerated GIF sizes/frame counts and colored
  PNG metadata check out. Game of Life instead empties 4,021 live cells in its
  second frame; an independent B3/S23 calculation requires 1,874 live cells.
- **3042 A:** grayscale fractal samples and Gray–Scott finite differences/update
  checks match. Both fractal color functions still fail on `cm.get_cmap`;
  Gray–Scott saves grayscale. The claimed color outputs are absent.
- **3042 B:** five PNGs and five GIFs decode. Continuous routines pass 221-point
  scalar comparisons and bounded animation checks. The basic renderer wrongly
  assigns the iteration ceiling to 24/221 first-escape samples; the original
  zoom source has the same operation. Its final placeholder Python file is invalid.

Exact commands, results and snapshot hashes are retained in the
[artifact summary](../runs/pilots/20261017T220435Z-653bd6b1/artifact-summary.json).
Initial review restoration made copied files root-owned; 17 affected checkpoint
checks were repeated after restoring identical bytes/modes as uid 1000. Both
attempts and the diagnostic are retained in
[review corrections](../runs/pilots/20261017T220435Z-653bd6b1/review-corrections.json).
These were review corrections, not inference reruns. All original snapshot
hashes still match, and all review workshops were cleaned up.

## Limits and validation

Each pair's first request was identical, but trajectories diverged during
session 1, before any appendix. For 1042 the second request differs only in
generated tool-call IDs; for the other pairs it already differs in content.
Fixed seeds did not ensure matched trajectories. Three pairs, one model/GPU,
unequal run lengths, a lost checkpoint, an unblinded reviewer and wording-dependent
claim counts prevent a reliable effect estimate. Extra context is also part of B,
so even a positive result cannot isolate factual content from extra-text effects.
Prefix truncation can hide the final exception line. Artifact checks are bounded
samples, not exhaustive correctness proofs or artistic-quality judgments.
Earlier exploratory runs were background evidence, not controls.

Before freezing, the controller suite passed 80 tests with six Docker skips;
the Docker-enabled suite passed all 80. Scripted A and B smoke runs passed,
as did JavaScript syntax and diff checks. Logs and smoke IDs are in
[validation.json](../runs/pilots/20261017T220435Z-653bd6b1/validation.json).
Workshop sources and existing model/image locks were unchanged. The controller
remains standard-library-only; executable artifact checks stayed in the workshop.
