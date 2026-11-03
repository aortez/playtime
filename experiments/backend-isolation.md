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
