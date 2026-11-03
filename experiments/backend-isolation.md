# Framework backend context isolation

This is a Rust/native backend diagnostic, not a Python controller experiment or
a model-quality comparison. The stopped run and first reproduction are described
in [the continuous-native evidence](../docs/rust-continuous-native.md#framework-failure-history-investigation-2026-11-02).

## Comparison design, 2026-11-02

Keep Framework on GPT-OSS 120B and leave the 4090 to the other checkout. Run
one inference request at a time. Compare three variants in order:

1. The installed Ollama 0.32.3 runtime.
2. Its pinned llama.cpp `b10091` rebuilt with Ollama's compatibility hooks.
3. The same build with the C++ changes from upstream
   [PR 27624](https://github.com/ggml-org/llama.cpp/pull/27624), commit
   `c71983015e9969389cc297716e4875f5195106b1`.

The original installation stays intact. Rebuilt variants use a separate transient
service, loopback port and SSH tunnel, with the installed ROCm payload and model
files. Stop the idle original service during isolated trials; restore it after
the comparison. Do not run multiple loaded 120B runtimes concurrently.

Start each variant with a new model process. Replay the original summary request
once as a fixed warmup, followed by three fixed pairs. Each primer is a synthetic
smoke transcript with its own random 128-bit filename marker. Its target is the
archived Mandelbrot summary request with a different system-prefix identifier,
which forces a substantial prompt change. No target contains any primer marker.
Use exactly the same prepared requests across all variants.

The bounded Rust replay uses the existing native adapter and frozen model
identity/settings: High reasoning, 98,304 context tokens, sampling seed 1042.
Primers permit 1,024 generated tokens; targets and warmup permit 4,096. Each
request has a 400-second absolute deadline. There are at most seven requests per
variant. The replay holds the existing local live-model lock for each call,
offers no tools and has no tool executor. This lock does not exclude arbitrary
clients on the backend host.

Retain the actual outgoing HTTP bodies, raw response records, assembled results,
host journal and binary/source hashes. A marker in any target response, including
thinking, is contamination when that marker is absent from its outgoing body.
An empty or tool-calling summary without a marker is not a successful compaction
control. Three clean pairs alone do not prove universal isolation.

The unpatched rebuild controls for compiler and packaging differences. Only
`libllama-server-impl.so` differs between the two rebuilt payloads. The patch
backport adapts the header's old parameter names without changing semantics;
all C++ hunks apply without fuzz. Runtime version strings from source tarballs
report `0 (unknown)`, so source and binary digests identify these diagnostic
builds. They are not silently substituted for registered production identities.

Retained evidence and repeatable commands are under
`/home/data2/playtime/framework-cache-comparison-20261102`: `comparison-plan.json`,
`build-provenance.json`, `run-variant.sh`, the Rust replay binary/source,
`start-isolated.sh`, and read-only `analyze.py`. Failed setup attempts remain
separate from inference results. The analyzer does not launch inference.

Only after clean isolation checks and valid summary behavior should a short
native compaction run precede another hour-long experiment. Preserve failures
and incomplete evidence rather than treating a restart or cache flag as a
verified fix.

## Isolation results

| Runtime configuration | Contaminated target pairs | Clean prose summaries |
| --- | ---: | ---: |
| Installed Ollama 0.32.3, ROCm | 3/3 | 0/3 |
| Unpatched rebuild, ROCm | 3/3 | 0/3 |
| Candidate cache patch, ROCm | 3/3 | 0/3 |
| Candidate patch with RAM prompt cache disabled, ROCm | 3/3 | 0/3 |
| Unpatched rebuild, Vulkan/RADV on the same GPU | 0/3 | 3/3 |

All variants received byte-identical outgoing request bodies. The first three
variants also produced identical content, thinking and generated-token counts
across all seven requests. The patched journal explicitly shows prompt clearing
before the contaminated targets. This candidate does not fix our reproduction.

After that failure, a separately recorded extension disabled the RAM prompt
cache through `LLAMA_ARG_CACHE_RAM=0`. The journal confirms the effective setting;
all three targets still leaked. A bounded loopback packet capture reconstructed
complete native `/completion` requests and streaming responses for pairs two
and three. Each actual llama.cpp prompt contains both intended target messages
and no primer nonce; its native response includes the primer nonce. Response
token counts match the outer adapter capture, and tcpdump reported zero dropped
packets. This establishes contamination below Playtime and Ollama's rendering
and response-parsing layers. It does not identify the defective kernel or line.

The Vulkan extension follows reports specific to the same Radeon 8060S/gfx1151
hardware: [Ollama issue 17847](https://github.com/ollama/ollama/issues/17847) and
[issue 17895](https://github.com/ollama/ollama/issues/17895). Vulkan loaded all
37 layers on Framework's GPU, with the same model, sampler, context allocation
and normal cache settings. Its warmup and all three targets returned prose
without tool calls. The target summaries concern the Mandelbrot work and contain
none of the three foreign markers.

This tests a usable backend configuration, not a single-kernel substitution:
Ollama also chooses different backend defaults (ROCm batch/microbatch 2,048 and
direct I/O; Vulkan batch/microbatch 512 without the direct-I/O flag). Those
differences are retained in the host journals. No 4090 inference was performed.
Three clean pairs are encouraging regression evidence, not proof against every
possible contamination case. Earlier tool and summary failures on this ROCm
configuration cannot all be confidently attributed to the model itself.

`analysis.json` under each variant records each marker check and request/response
digest. `three-variant-equivalence.json` records the exact baseline/patch match;
`patched-no-cache/internal-boundary-proof.json` links the internal request and
response evidence. The separate native follow-up uses the unchanged installed
runtime with Vulkan selected, rather than installing the candidate patch.

## Native follow-up

The first follow-up used the original installed binaries with Vulkan, the frozen
failure-history worker bundle, and GPT-OSS 120B at High reasoning. The existing
native probe bounds stayed unchanged: 300 seconds, 16,384 context and generated
tokens, 512 tokens per work/summary response, and a 45-second model deadline.

Run `continuous-f934257b8554` completed 25 work responses, then stopped at its
first compaction. The summary consumed all 512 allowed tokens in thinking,
returned no final content, and ended with `finish_reason: length`. The controller
reported `compaction_failed`; the archive retains the more specific completion
reason. There were no accepted compactions. Usage was complete at 3,164 known
tokens, and all owned Docker resources were confirmed absent after complete
cleanup. This is a failed native gate, not a successful continuity claim.

Evidence is `/home/data2/pt/bc.WszzTE5E/run`. Its authenticated final receipt
matches the inspected archive prefix. The comparison retains the exact failed
summary and a separately bounded replay using 4,096 output tokens, matching the
already configured hour-summary allowance. That standalone replay also uses the
hour's 400-second deadline instead of the native smoke's 45 seconds. It completed
with `stop`, 3,104 generated tokens, 84.938 seconds and a 262-character summary,
with no tool call. The identical messages/settings are retained; only the output
allowance and outer diagnostic deadline differ. This supports a budget-fit
explanation, not a repair to the original failed archive.

A separate Low-reasoning follow-up kept the native probe's 512-token bounds
unchanged. Run `continuous-ff9ab5d8c352` passed: two accepted compactions, verified
`continuity-ok` output and final workspace capture, 2,962 known generated tokens,
no unknown usage, and complete cleanup with all three owned Docker resources
absent. Its evidence is `/home/data2/pt/bl.TYLvS3vq/run`; the captured writer
receipt matches the inspected archive prefix.

That run also exercised failure retention with a real model mistake. One exec
call used `bash /.../continuous-probe.sh` and returned exit 127 with a missing-file
error (controller sequences 456/458). The model recovered using the correct
path. The second compaction retained the original error, including its source
sequences and stderr, in work requests 52 through 55 despite subsequent success.
`native-low-failure-history.json` records those observed inputs. This is direct
native evidence that failure history survived compaction. Neither this
Low-reasoning pass nor the standalone replay substitutes for a completed
High-reasoning native gate.

The isolated backend, packet capture and SSH tunnel were stopped after testing.
The original Ollama service is active again with its original binaries and
configuration; the candidate patch and Vulkan selection were not installed as
production changes. Its model list is empty and no experimental worker remains
active. No new hour-long run was launched. The next controlled long experiment
should use the tested Vulkan configuration and the reviewed hour recipe's
existing summary allowance and deadline, retaining the same isolation checks.

## Summary-budget correction

The owner then requested that the 512-token smoke-summary budget be corrected.
The fixed native smoke now permits 4,096 summary/output tokens and 400 seconds
per model call, matching the hour experiment's existing summary allowance and
deadline. Its run cap is 900 seconds and 32,768 generated tokens. A 32,768-token
context accommodates the larger reserves and two retained exchanges; a 40%
trigger keeps multiple compactions within the 48-stage probe. Catalog recipes
and general compaction defaults are unchanged. See the
[compatibility note](../docs/rust-compatibility.md#native-smoke-summary-allocation-2026-11-02).

Validation evidence is retained under
`/home/data2/playtime/framework-summary-budget-20261102`:

- Formatting, workspace Clippy and 644 Rust tests passed. The frozen Python
  suite passed with 101 tests, six skipped. Its initial invocation inherited
  umask 077 and failed the frozen tar directory-mode comparison; rerunning with
  normal umask 022 passed without source or fixture changes.
- Scripted native run `continuous-6af9ecbc8fca` passed with 15 compactions,
  2,048 known tokens, final capture and cleanup
  (`/home/data2/pt/pc.gl6IBBCp/run`).
- High-reasoning Vulkan run `continuous-8b1bdb91689e` completed two summaries
  normally: 328 tokens in 20.499 seconds, and 956 tokens in 37.794 seconds. Both
  requests carried the 4,096-token allowance. The second exceeds the old cap.
  The run reached `agent_finished`; receipt-verified final artifacts contain
  stage 48 and `continuity-ok`, and all three owned Docker resources are absent
  (`/home/data2/pt/pc.1OVyTdmL/run`).
- That High run **failed the full native gate**: one work request returned a
  malformed tool call without generated-token usage. The controller recorded
  its parse error, reserved the full 4,096-token allowance and recovered. Usage
  remains 5,476 known tokens plus one unknown request; it is not reported as
  complete. Summary usage is complete at 1,284 tokens. The gate's unknown-usage
  condition correctly rejects this outcome despite task completion. The saved
  `native-high-parse-failure.json` and `native-high-result.json` retain the cause,
  source sequences and independently checked final capture.
- The scripted service run-and-view gate passed, including its deliberate
  worker-loss check; that check retains an incomplete archive/reservation and
  does not establish automatic restart recovery
  (`/home/data2/pt/playtime-service.azBuoRug` and `/home/data2/pt/p66-8322245c`).

The High run establishes that the expanded summary allocation works through
native compaction; it does not establish a fully passing High native gate or
resolve the separate malformed-call/missing-usage issue. No further inference
was repeated merely to obtain a clean result. The isolated service and tunnel
were stopped, the original unchanged Ollama service was restored and reports
no loaded models, and no new hour experiment was launched.

## Parse-failure history and Vulkan hour follow-up

The owner approved extending the bounded historical record to malformed model
responses, fixing native-gate diagnostics and running another controlled hour.
The controller now emits `failure_history_v2`: tool-result and parse-error
records share the four-record / 4 KiB bound. A parse-error record references its
actual controller observation, states that no tools executed, and retains its
unknown-usage reservation. It does not decide whether the model or backend
caused the malformed response. Successful recovery does not erase the record;
it becomes separate historical context when its owning exchange is compacted.
The captured Framework diagnostic has a versioned replay fixture.

Validation evidence is under
`/home/data2/playtime/framework-vulkan-hour-20261102`:

- Formatting, workspace Clippy, 644 workspace Rust tests, 89 engine tests with
  the test harness, 13 workspace-probe tests and the frozen Python suite passed
  (101 Python tests, six skipped).
- Scripted native run `continuous-a08fcae55bd3` passed with 15 compactions,
  final capture and cleanup (`/home/data2/pt/pc.Yg1O6gFI/run`).
- High native run `continuous-de7ef9bc066e` reached `agent_finished` with two
  compactions; receipt-verified final files contain stage 48 and
  `continuity-ok`, and all three owned resources are absent
  (`/home/data2/pt/pc.heCVubu9/run`). The full gate **failed** because two
  malformed responses lacked usage: 6,271 known tokens and 8,192 reserved
  tokens across two unknown requests. The error now names those counts.
  Fourteen later model inputs retained the first parse-error record; its
  source sequence, reservation and absence of tool dispatch were checked
  against the writer-confirmed archive prefix. Summary usage is complete at
  1,284 tokens. This establishes failure-history continuity, not a clean High
  model/backend gate.
- The scripted service run-and-view gate passed, including the deliberate
  worker-loss case. The latter retains incomplete evidence and does not
  establish automatic restart recovery.
- Three fresh-marker primer/target pairs on the regular Framework endpoint
  passed. Each primer returned its marker; all target inputs excluded every
  marker and all target responses contained none. Each target returned normal,
  nonempty prose, no tools, a normal stop and known usage. The target requests
  used High reasoning and the hour run's 98,304-token context. This limited
  preflight establishes no universal absence of contamination.

The original Ollama 0.32.3 binaries are unchanged. A new explicit service
configuration, `/etc/systemd/system/ollama.service.d/90-playtime-vulkan.conf`,
selects Vulkan and the RADV ICD on the Framework; the prior temporary diagnostic
service is unused. A matched daemon and frozen worker bundle were installed;
the worker executable SHA-256 is
`5a08e9e7405f98364fb871e86714081a0c094ae0dea3a1f57ac297c4b08fd52a`.
The hour recipe was cloned with only a new display name. Its prompt, seeds,
model registration, High reasoning, sampling, 3,600-second wall limit,
98,304-token context, 4,096-token summaries, 400-second model deadline and
600,000-token cap were verified unchanged. The frozen worker contains the new
failure-history implementation. No 4090 was used.

Run `20261103T010531Z-3d82f55c` was accepted at 2026-11-03 01:05:31 UTC
(17:05 Pacific on November 2). The authenticated Rust
client follows committed events; a read-only observer checks marker leakage,
model errors, tool failures and later failure-history inputs. External
observation bytes, if used, must match a content reference from those published
events. A discovered disk commit marker never advances that observation frontier.

### Completed hour outcome

The controller decided `wall_limit` at exactly 3,600,000 ms and completed final
capture/cleanup at approximately 3,600,978 ms. The authenticated client finished
successfully. Its final published prefix is 6,413 events / 17,730,880 bytes,
SHA-256 `b4ed482d9d43911886d3bbe53c1929938e42d684b8b37c3d5a567f4f75856505`.
That prefix was independently checked against the archive bytes using the API's
frontier, without promoting an on-disk marker. The service reports terminal,
complete cleanup; the worker's systemd exit is successful, its unit is inactive,
and the agent, holder and volume are independently confirmed absent on the
same Docker daemon. `final-proof.json` retains these checks.

There were 238 model requests: 230 work and eight compaction requests. All eight
summaries completed with known usage: 2,134–3,521 tokens each, 22,959 tokens in
total, taking 966.897 seconds in total. Each had the 4,096-token allowance and no
offered tools. The larger summary budget therefore worked throughout this hour;
summary generation still consumed about 27% of its wall time. Neither catalog
limits nor the live run were changed to improve this result.

Total usage is **87,182 known generated tokens plus 24,576 reserved tokens across
three unknown requests**, charging 111,758 tokens to the budget. Two work
responses (`r190`, `r234`) returned Ollama parse errors with invalid `]` syntax in
tool arguments and no usage. The third (`r238`) was interrupted at the wall
deadline. Usage remains incomplete; none of those requests is counted as zero.
Backend logs show cancellation of the final model task at the deadline. No tools
were dispatched for either malformed response.

The observer found no fresh or earlier diagnostic marker in any recorded model
output. The first summaries' completed-file names were checked against their
actual inputs and came from this run. This limited result does not prove that
all backend contamination is impossible, nor that summaries are factually
accurate.

Fourteen nonzero-exit or structured-error tool outcomes were recorded. Some are
normal environment/file probes rather than substantive mistakes. Two parse errors
were recorded separately. The failure-history audit checked 613 references in
200 later model inputs, all matching their original observation identities and
outcomes. The largest history message was 3,081 bytes, below 4 KiB. Its four-record
limit explicitly counts older omissions. Every failure eligible before the last
compaction appeared in retained history; the late parse error and late `file`
failure remain in the full archive, with no later compaction to promote them.
The first parse failure survived compactions seven and eight and appeared in 34
later work inputs with its original unknown reservation. See `history-audit.json`,
`failure-history.json`, and `run-analysis.json` in the evidence directory.

### Findings by responsibility

| Perspective | Observation | What this establishes |
| --- | --- | --- |
| Model use of available evidence | Call 142 repeated the missing `file` utility while its input explicitly contained the earlier failure at controller sequence 980. Call 204 repeated a prohibited working directory while its input contained the earlier boundary error at sequence 4186. | The controller preserved the evidence; the model did not use it. Availability does not guarantee adherence. Separate saved request/result records establish both examples. |
| Model command construction | A malformed heredoc executed intended documentation as commands; a later `sed` edit had a syntax error. The model recovered by using simpler commands/scripts. | These are executable-command mistakes with faithfully returned diagnostics, distinct from malformed tool-call JSON. |
| Model planning and verification | Packaging duplicates exhausted the 16 MiB workspace; writes returned explicit errors. Summaries described old measurements as current, an overwritten GIF as two files, and an uninspected seed directory as empty. | The larger summary budget prevents truncation pressure but does not ensure accurate memory or coherent artifact management. |
| Model/backend response boundary | Two replies were rejected for malformed tool arguments; Ollama omitted usage. | This unresolved class also occurred in the High smoke. The captured error alone does not establish whether generation or a backend transformation caused the bad JSON. |
| Workshop/reporting limitation | `tar -tzf /work/full_project.tar.gz \| wc -l` returned shell status 0 while stderr reported a truncated archive. | Bash runs without `pipefail` (`workshop/tool.py`). The controller preserved stderr and accurately reported the shell status, but this observation did not enter failure history because it had no nonzero exit or structured error. Earlier direct tar failure was retained. See `zero-exit-pipeline-errors.json`. |
| Controller/accounting behavior | Bounds, source references, unknown reservations, final capture and cleanup behaved as designed in this run. | Evidence supports those paths for this experiment; it does not resolve the separate restart/recovery gaps or make the failed strict High smoke gate pass. |

Final capture was downloaded through the Rust client's verified artifact route;
all regular files matched the published capture index. The 600×600 GIF has
60 frames. The 16-member source tarball validates, matches its current retained
files, and exactly matches the base64 archive embedded in `README_src.md`.
The placeholder was repaired by the end. Both HTML viewers still reference four
deleted PNGs, and the main `README.md` embeds a valid older archive rather than
the final source package. Four seed files remain; the summary's “empty” claim was
false. The smooth PNG is 616×616 despite earlier 800×800 descriptions. These are
artifact/claim inconsistencies, not evidence of lost controller records. The
truncated large archive was removed before final capture. Verification read
bounded file/archive data and did not execute any generated program. See
`capture-review.json` and `embedded-archive-review.json`.

The next system change to review is explicit pipeline-failure semantics in the
workshop bridge, with cases for masked failures and intentional SIGPIPE/early
consumers. It requires a deliberate image refresh and a new lock; no workshop
sources or locks were changed during this experiment. Backend diagnostic work
should retain the missing-usage distinction and isolate the malformed response
before assigning its cause. Model evaluations can now test repeated failure
avoidance with evidence demonstrably present and separately measure summary
factuality and artifact verification.

The regular Framework Ollama service remains explicitly configured for Vulkan,
using the original binaries. `playtimed-framework-vulkan-20261102.service` remains
the matched daemon; the completed experimental worker is gone. The hour was run from the existing `preserve-compaction-failures` checkout
before this tested baseline was committed.

## Pipeline reporting and bounded follow-up, 2026-11-03

The tested failure-history baseline was committed as `271ebcf`. The next system
fix enables Bash `pipefail` in the workshop bridge, without `errexit`. A tar
failure followed by a successful `wc -l` now returns exit 2 and its stderr;
successful stderr output still returns zero. Expected early-consumer SIGPIPE
returns 141 and can be handled explicitly. Nine real Docker cases cover these
behaviors, normal success, recovery, sequential commands and explicit opt-out.
The model inventory describes this contract.

Environment-only preparation deliberately refreshed the workshop, leaving model
identity and packages unchanged. New image
`sha256:b98a1e67a7170f14fb3452e59b559c17b2bf03eca597e84fdda52a8e491e68cb`
contains bridge SHA-256
`4e76de37f73cbaf512b22039af3850b2fbdb647d652f05a8885612079d15b311`.
The general and compiled image locks agree; older images and archives remain.
A new daemon/worker pair and recipe selection are required to use this image;
the existing hour experiment's runtime and saved definition were not modified.

Evidence is retained in `/home/data2/playtime/pipeline-reliability-20261103`.
Formatting, workspace Clippy, 644 Rust tests, 89 feature-gated engine tests and
the frozen Python suite (101 tests, six skips) passed. The real sandbox passed
all nine pipeline cases and exact cleanup. The scripted service run-and-view
and worker-loss checks passed, and `continuous-f243429d314f` passed 15 compactions
and final capture. These are distinct from live-model acceptance. Initial
readiness and worker-fixture failures during development caught the bridge hash
that had not yet been updated; they are retained separately from the successful
checks of the final source.

The diagnostic plan freezes the two hour-run malformed requests, two replays of
each previously repeated failure, two summary replays and two constructed
artifact-verification probes. Within each pair, only the second trial's
sampling seed changes in the model input.
Work uses 8,192 tokens and summaries 4,096, on the same Framework Vulkan runtime
and model identity. The bounded Rust replay has no tool executor: returned
commands measure proposed actions only. Raw HTTP capture and a bounded capture
of the selected local llama.cpp port distinguish generated output from backend
parsing. No unknown production usage is replaced by diagnostic measurements.

The live native run `continuous-48e843e65c68` completed stage 48, two compactions
and cleanup, with 6,010 known tokens and 8,192 reserved for two malformed
responses. **The strict gate failed**, because those requests have unknown
usage. Independent inspection verified the event prefix against the captured
worker receipt and checked the final archive/index hashes and `continuity-ok`
file (`live-proof.json`). That additional evidence does not turn the gate green.

### Model output versus backend error reporting

Both hour requests (`r190`, `r234`) reproduced malformed JSON on the first replay.
Their `/api/chat` streams ended with an error object and no `eval_count`. Two
initial packet-capture setup attempts failed before useful capture (remote file
permission and transient-service stdio setup); their logs remain. A bounded
capture streamed over SSH then recorded both requests and responses completely,
with no TCP gaps, conflicting retransmissions or kernel-dropped packets.

On that captured pass, `r190` again failed. The exact arguments in Ollama's error
appear verbatim in the underlying llama.cpp output, including the extra `]`
after the command string. An independent JSON decoder rejects those bytes.
This establishes a generated malformed call, rather than JSON damaged by
Playtime or introduced by Ollama's parser. The same internal response contains
`tokens_predicted: 405`, but the outer response contains only the parser error.
Ollama 0.32.3's Harmony parser defers argument decoding until completion; the
server's parser-error branch emits the error instead of the response carrying
metrics. This is a separate backend reporting defect. Playtime has no trustworthy
count on that API path and correctly retains an unknown request and its reserve.
Diagnostic counts do not replace any archived production accounting.

The captured `r234` replay instead returned a valid call, with 368 tokens at both
boundaries. The outgoing HTTP bodies for both captured replays were byte-identical
to their first-pass counterparts. The error is not deterministic, and this
capture directly attributes the malformed output only for `r190`.
`boundary-attribution.json`, `boundary-proof.json`, raw HTTP records and the PCAP
retain the comparison. The capture process was stopped after these two requests.
No runtime binary, GPU selection or model identity was changed.

### Focused model observations

These are small first-action/summary checks, not a general success-rate estimate:

- With the actual retained failure history, one of two `r204` replays again
  proposed `cwd: "/"`, which the earlier result explicitly rejected. The other
  used `/work`. Retention makes evidence available; it does not ensure adherence.
- Neither of two `r142` replays repeated the missing `file` executable. One
  proposed removing the partial archive and extracted directory; the other
  proposed listing the archive. These commands were never executed.
- Both summary replays completed within 4,096 tokens (2,923 and 2,060). They
  continued treating inherited display claims as verified. The second introduced
  a 2023 date absent from the input and asserted that the GIF had been opened
  locally, without a corresponding observation. A larger budget solves the
  observed truncation problem, not factual fidelity.
- Both constructed artifact probes began by listing `/work`. That is an
  inspection step, not proof of link, image or archive verification. The replay
  has no executor, so it does not establish completed artifact quality.

The next hour-long experiment is held after the live acceptance failure. The
concrete next system task is preserving backend usage on parser errors, with an
explicit failure/accounting contract and regression coverage. Separate model
work should measure failure avoidance and summary claims against observations;
a successful write or a model-generated summary is not artifact verification.

## Parser-error usage repair, 2026-11-03

The owner approved implementing the usage-reporting fix after the prior native
acceptance failure. The patch in
[`ollama-parser-error-usage-v1.patch`](ollama-parser-error-usage-v1.patch) applies
to the pinned Ollama v0.32.3 source. It preserves terminal measurements through
Harmony argument rejection and the generic stream error writer. The latter also
previously discarded all fields except `error`. A terminal rejection remains an
error with no assistant message or dispatchable tool calls. Nonterminal errors
and unrelated failures receive no invented measurements.

The Go binary identifies itself as `0.32.3-playtime-usage-v1`; its SHA-256 is
`60356f8f5561bea941a1e76b3e8c4fddb80742e2e083fc836a64f7ff95222fa9`.
It was built with Go 1.26.8, installed alongside the original binary, and uses
the unchanged installed native Vulkan payload. A separate GPT-OSS 120B lock
keeps the original model digest/definition and explicitly selects that backend
version. Old locks and archived inputs are unchanged. Candidate validation uses
a bounded transient service on Framework port 11436 and an SSH tunnel on local
port 11437 while the idle stock service is stopped.

Playtime's adapter validates the versioned terminal error envelope and passes
measured usage separately from the failed response. The controller settles it
once, preserves the error/retry behavior and executes no rejected tools. The
new failure-history version 3 preserves whether that request was known or
unknown through later compactions. CPU cases cover zero, 405 tokens, missing or
invalid counts, identity mismatch, duplicate records, late completion,
accounting overflow, allocation overrun, actual worker-to-archive accounting,
and both work and summary failures across repeated compaction.

Evidence is under `/home/data2/playtime/parse-usage-20261103`. The backend's
server/Harmony tests passed, as did formatting, workspace Clippy, 648 default
Rust tests, 90 feature-gated engine tests, 13 workspace-probe tests, and the
frozen Python suite (101 tests, six skips). Package checks and real scripted
service/worker-loss checks passed. `continuous-bfc04386f943` passed 15 scripted
compactions and final capture. The stock-backend live CLI/UI gate on Framework
also passed (`20261103T031414Z-11047334`, 493 known tokens, followed by workbench
run `20261103T031441Z-8804d504`). This checks backward compatibility separately
from the patched-backend inference evidence below.

Both captured requests reproduced malformed arguments with the patched backend.
Their native llama.cpp counts, outer Ollama counts and Playtime adapter counts
agreed exactly: 526 tokens for `r190` and 1,007 for `r234`. The malformed argument
bytes were present in the native output and independently rejected as JSON.
The outgoing request bodies matched the stock replay byte-for-byte. Complete
HTTP/TCP records had no gaps or dropped packets. These diagnostic replays had no
executor; `boundary-verified.json` records the three-way count comparison.

The patched Framework native gate then **passed** as
`continuous-65731f16b03f`. It completed stage 48, two compactions, final capture
and cleanup. Two rejected model calls (`r22`, 476 tokens; `r46`, 52 tokens)
recovered with known usage. Total usage was 6,369 known tokens: 5,841 from
successful responses and 528 from rejected responses, with zero unknown
requests or reserves. `native-proof.json` verifies the event prefix against the
captured worker receipt, error counts against raw terminal responses, no tool
dispatch for either rejected request, retained-history references, and the
final capture hashes and `continuity-ok` file. Native evidence is retained at
`/home/data2/pt/pc.ZBPwC5AF/run`.

After validation, the isolated service and temporary tunnel were stopped and
the original `ollama.service` restored. Its API again reports `0.32.3`, and both
installed binary hashes are unchanged. The candidate remains available beside
the original installation. The next hour requires explicit selection of the
patched backend and new lock with matching daemon/worker builds; existing saved
definitions are not silently migrated. No new hour experiment was launched in
this repair step. Malformed model calls and unsupported summary claims remain
model-quality issues; this repair preserves their measured cost and evidence.

A diagnostic build accidentally reused the previous investigation's mutable
Cargo target directory and overwrote its compiled replay executable. The old
execution hash, source, Cargo lock, inputs, raw HTTP/PCAP and result files remain;
`pipeline-reliability-20261103/provenance-addendum-20261103.json` explicitly records
that the old binary itself is no longer retained. Subsequent replay builds use
the new evidence directory. No archived experiment or raw measurement changed.

### Integration with run reports

Before PR, the branch incorporated `main` through `43dad7c` (CLI/UI run reports).
The only merge conflict was the compatibility log; both sets of entries remain.
A shared-client regression checks measured and unknown parser failures for work
and compaction, including zero-token usage and the later settlement fact, in
forward and reverse event order. Errors remain errors and usage is counted once.

An offline diagnostic also fed the captured Framework run's 929 events through
the actual shared report accumulator, after verifying the captured worker receipt's
byte bound and hash. Both orders produced identical reports: 52 work calls with
5,085 known tokens, two compaction calls with 1,284 known tokens, two parse errors,
zero unknown usage, and no unrequested or open calls. This verifies report
integration against retained native evidence; it is not a new inference run.

Post-merge checks passed: formatting, workspace and feature Clippy, 668 workspace
tests, 90 feature-gated engine tests, 13 workspace-probe tests, 12 CLI fixture
cases, two UI fixture cases, and the frozen Python suite (101 tests, six skips).
Evidence, diagnostic source and lock file, and `native-report-v2.json` are under
`/home/data2/playtime/pr-integration-20261103`. The earlier Docker/backend/native
checks above precede this report integration; no native execution code changed
during the merge or integration follow-up.

### Independent review corrections

The independent PR review found two accounting/reporting defects. Counted parser
failures could exceed a continuous request's admitted token allowance and still
retry below the total run allocation. Memento retries also described measured
failures as unknown full-allowance charges. Both are corrected: per-call overruns
stop after recording their measured charge and error, and retry wording now
matches the failed request's known or unknown usage. Unknown memento notices
remain byte-for-byte unchanged. A second read-only review found no remaining
actionable issues.

Regressions cover work and summary calls at their cap and one token above it,
and memento work/handoff retries with zero, nonzero and unknown usage. The
retention test's 405-token simulated summary failure now has a 512-token
allowance; previously its 256-token allowance unintentionally relied on the
missing overrun guard. The native capability advances to
`terminal_parse_error_usage: 2`; the backend patch and model locks are unchanged.

Evidence is under `/home/data2/playtime/pr180-review-20261103`. Formatting,
workspace/feature Clippy, 669 workspace tests, 92 feature-gated engine tests,
13 workspace-probe tests, the frozen Python suite (101 tests, six skips), and
the worker package gate passed. The scripted run-and-view and worker-loss gates
passed (`20261103T041536Z-8be3e5b4`). Scripted native run
`continuous-56e3b7de33ce` passed 15 compactions and final capture.

The subsequent Framework live run `continuous-7bf1d41d5a79` passed two compactions,
final capture and cleanup with 6,153 known tokens and zero unknown usage.
Its two parse failures (`r22`, 476 tokens; `r41`, 33 tokens) recovered and retained
known accounting. `native-proof.json` verifies the captured worker receipt,
raw error counts, absence of rejected-tool dispatch, retained failure references,
and final capture hashes. The run evidence is at
`/home/data2/pt/pc.Twxt55Zx/run`. The isolated service and owned tunnel were stopped,
and the original backend restored with unchanged installed binary hashes and
API version `0.32.3`.
